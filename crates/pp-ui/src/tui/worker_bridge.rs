//! TUI worker 与主循环之间的命令／事件协议，以及 worker 侧的发布与 ACK 助手。

use anyhow::Result;
use pp_config::config::ModelChoice;
use pp_config::model_state::Selection;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::RunOutcome;
use pp_core::events::AgentEvent;
use pp_core::model::ModelSource;
use pp_core::model::ThinkLevel;
use pp_core::plan::ExecutionContext;
use pp_core::plan::PlanReview;
use pp_core::plan::PlanReviewReport;
use pp_core::plan::ReviewToken;
use pp_core::plan::WorkflowState;
use pp_runtime::agent::Agent;
use pp_runtime::agent::ExecutionPreparation;
use pp_runtime::agent::context::CompactionOutcome;
use pp_runtime::agent::input::InputQueue;
use pp_runtime::agent::plan_management::PlanListing;
use pp_runtime::agent::plan_management::PlanOpenIntent;
use pp_runtime::agent::plan_management::PlanSelection;
use pp_runtime::tools::ChangesView;
use pp_runtime::tools::UndoResult;
use pp_runtime::tools::UndoToken;
use std::{
    collections::BTreeMap,
    sync::mpsc,
    time::{Duration, Instant},
};
pub(super) enum WorkerCommand {
    Mcp {
        action: pp_runtime::mcp::Action,
        cancellation: CancellationToken,
    },
    ReloadModels,
    Shell {
        command: String,
        call_id: String,
        cancellation: CancellationToken,
    },
    RunInput {
        prepared: pp_runtime::user_input::UserInput,
        cancellation: CancellationToken,
        input: InputQueue,
    },
    Run {
        prompt: String,
        cancellation: CancellationToken,
        input: InputQueue,
    },
    Continue {
        cancellation: CancellationToken,
        input: InputQueue,
    },
    Compact {
        cancellation: CancellationToken,
    },
    Reset,
    /// 会话根切换（/resume、/tree、/new）会停掉旧子 Agent 根；Hub 打开时按需重建。
    EnsureSubagents,
    SwitchModel(String),
    ApplyModelTargets,
    SaveDefaultThinkLevel(ThinkLevel),
    SaveDefaultModel(String),
    SaveModelRole(
        pp_config::config::ModelRole,
        Option<pp_config::config::RoleAssignment>,
    ),
    ToggleFavoriteModel(String),
    DeleteSession(String),
    Resume {
        id: String,
        cancellation: CancellationToken,
    },
    ReadTree,
    NavigateTree(String),
    ReadChanges,
    UndoFile(UndoToken),
    EnterPlan {
        cancellation: CancellationToken,
    },
    ReadPlan,
    LeavePlan {
        cancellation: CancellationToken,
        discard_plan_id: Option<String>,
    },
    SavePlanAndLeave {
        token: ReviewToken,
        cancellation: CancellationToken,
        input: InputQueue,
    },
    // /plan-list 管理：列表/预览/打开/重命名/删除，全部在 worker 侧完成 I/O。
    ListPlans,
    ReadListedPlan {
        id: String,
    },
    OpenListedPlan {
        selection: PlanSelection,
        intent: PlanOpenIntent,
        cancellation: CancellationToken,
    },
    RenameListedPlan {
        selection: PlanSelection,
        file_name: String,
    },
    DeleteListedPlan {
        selection: PlanSelection,
    },
    ApprovePlan {
        token: pp_core::plan::ReviewToken,
        context: ExecutionContext,
        cancellation: CancellationToken,
        input: InputQueue,
    },
    // 独立 reviewer：进度经 ReviewProgress 事件回流，终态为 PlanReviewed。
    ReviewPlan {
        token: pp_core::plan::ReviewToken,
        cancellation: CancellationToken,
        input: InputQueue,
    },
}
pub(super) enum WorkerEvent {
    McpManaged {
        result: Result<()>,
        view: pp_runtime::mcp::View,
        show: bool,
    },
    McpSnapshot(pp_runtime::mcp::View),
    DialogFinished {
        call_id: String,
    },
    ModelsReloaded(Result<ModelListing>),
    Ask {
        request: pp_core::interaction::AskRequest,
        reply: mpsc::SyncSender<Vec<pp_core::interaction::AskAnswer>>,
        cancellation: CancellationToken,
    },
    Progress(AgentEvent),
    ShellCompleted(Result<()>),
    #[cfg(debug_assertions)]
    MockPrepared(pp_runtime::mock_ai::MockTestSnapshot),
    #[cfg(debug_assertions)]
    MockCompleted {
        result: Result<pp_runtime::mock_ai::MockTestReport>,
        elapsed: Duration,
        finished_at: chrono::DateTime<chrono::Local>,
    },
    Completed {
        result: Result<RunOutcome>,
        elapsed: Duration,
        finished_at: chrono::DateTime<chrono::Local>,
    },
    SessionStarted(String),
    /// 根会话临时管道 registry 绑定。启动与换根后发布；接收端用
    /// `same_instance` 判断是否真正换根并递增打开面板的 epoch。
    PipeStoreBound {
        store: pp_runtime::tools::PipeStore,
    },
    // 压缩终态：与普通任务 Completed 是不同的判别值，不触发任务收尾展示。
    Compacted(Result<CompactionOutcome>),
    Reset,
    ModelSwitched(Result<(ModelChoice, ThinkLevel)>),
    DefaultThinkLevelSaved(Result<ThinkLevel>),
    DefaultModelSaved(Result<String>),
    ModelRolesSaved(Result<(pp_config::config::ModelRoles, String, ThinkLevel)>),
    ModelState {
        revision: u64,
        model: ModelChoice,
        effort: ThinkLevel,
        source: ModelSource,
    },
    FavoriteModelsSaved(Result<Vec<String>>),
    SessionDeleted(Result<()>),
    SessionResumed(Result<ResumeState>),
    /// 子 Agent 根绑定重建结果：只影响 Hub 可读性，不改变会话状态。
    SubagentsEnsured(Result<()>),
    TreeRead(Result<pp_runtime::agent::persistence::tree::View>),
    TreeNavigated(Result<pp_runtime::agent::TreeNavigation>),
    ChangesRead(ChangesView),
    FileUndone(Result<UndoResult>),
    // /plan-list 管理结果；失败保留旧列表与输入，只展示可见错误。
    PlansListed(Result<PlanListing>),
    ListedPlanRead(Result<PlanSelection>),
    // 携带完整执行准备：UI 采纳 workflow/replay/todo 快照，但不自动请求模型。
    ListedPlanOpened(Result<ExecutionPreparation>),
    ListedPlanRenamed(Result<PlanListing>),
    ListedPlanDeleted(Result<PlanListing>),
    // 计划模式转换结果；Err 时 worker 会紧随一份真实 workflow 快照，
    // 避免 UI 在转换失败的提示中显示错误的模式。
    WorkflowChanged(Result<WorkflowState>),
    PlanRead(Result<PlanReview>),
    // 审查进度：只进独立 review transcript，不进主对话、用量或派发。
    ReviewProgress(AgentEvent),
    // reviewer 终态：成功携带最终报告；失败/取消保留上次完整报告。
    PlanReviewed(Result<PlanReviewReport>),
    // 无专属结果事件的动作（普通运行/压缩/切换模型/树导航）之后发布的快照。
    Workflow(WorkflowState),
    // lazy=false 后台预热失败：单服务器隔离，正常工具不受影响。
    LspWarmupFailed {
        server: String,
        error: String,
    },
    // 空闲轮询迟到诊断时的错误；去重后交给 UI 提示，不终止会话。
    LspPollFailed(String),
    // 一次性交接：UI 采纳 workflow/replay/usage/todo 快照并 ACK 后 worker 才继续。
    ExecutionPrepared {
        workflow: WorkflowState,
        session_id: Option<String>,
        usage: Option<pp_core::context::ContextUsage>,
        replay: Option<Vec<pp_runtime::agent::persistence::ReplayRecord>>,
        /// 交接快照：UI 必须在采纳 replay/替换 workflow 之后、ACK 之前渲染，
        /// Fresh 清理后才入卡，源会话不能先 emit 再被清掉。
        todo_snapshot: Option<AgentEvent>,
        reply: mpsc::SyncSender<()>,
    },
}

/// 转换失败后的纠正快照：发布真实 workflow 与模型字段；所有 Plan 转换失败路径共用。
pub(super) fn publish_transition_failure(agent: &Agent, snapshots: &mut Vec<WorkerEvent>) {
    push_workflow_snapshot(agent, snapshots);
}

/// 换根后立即发布新的临时管道 registry；接收端按实例身份过滤重复发布。
pub(super) fn publish_pipe_store(events: &mpsc::SyncSender<WorkerEvent>, agent: &Agent) -> bool {
    events
        .send(WorkerEvent::PipeStoreBound {
            store: agent.pipe_store(),
        })
        .is_ok()
}

/// Publish the workflow snapshot so the UI never keeps a stale plan/mode.
pub(super) fn push_workflow_snapshot(agent: &Agent, snapshots: &mut Vec<WorkerEvent>) {
    snapshots.push(WorkerEvent::model_state(agent));
    snapshots.push(WorkerEvent::Workflow(agent.workflow().clone()));
}

/// Status-bar usage plus workflow snapshot after a task or state boundary.
pub(super) fn push_runtime_snapshot(agent: &Agent, snapshots: &mut Vec<WorkerEvent>) {
    if let Some(event) = usage_snapshot(agent) {
        snapshots.push(event);
    }
    push_workflow_snapshot(agent, snapshots);
}

/// Send one UI-bound event with a reply channel and wait for the ACK; a
/// disconnected UI or cancellation cancels the worker.
fn send_and_wait_ack<T: Send>(
    events: &mpsc::SyncSender<WorkerEvent>,
    make: impl FnOnce(mpsc::SyncSender<T>) -> WorkerEvent,
    cancellation: &CancellationToken,
) -> Result<T> {
    let (reply, ack) = mpsc::sync_channel(1);
    if events.send(make(reply)).is_err() {
        cancellation.cancel();
        return Err(pp_core::cancellation::Cancelled.into());
    }
    loop {
        pp_core::cancellation::check(cancellation)?;
        match ack.recv_timeout(Duration::from_millis(50)) {
            Ok(value) => return Ok(value),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                cancellation.cancel();
                return Err(pp_core::cancellation::Cancelled.into());
            }
        }
    }
}

/// 发布 ExecutionPrepared 并等待 UI 采纳快照后的 ACK。
/// ACK 失败或取消时返回错误。
pub(super) fn publish_prepared(
    events: &mpsc::SyncSender<WorkerEvent>,
    agent: &Agent,
    preparation: ExecutionPreparation,
    cancellation: &CancellationToken,
) -> Result<()> {
    events
        .send(WorkerEvent::model_state(agent))
        .map_err(|_| anyhow::anyhow!("UI disconnected during workflow transition"))?;
    send_and_wait_ack(
        events,
        |reply| WorkerEvent::ExecutionPrepared {
            workflow: preparation.workflow,
            session_id: preparation.session_id,
            usage: agent.context_usage().ok(),
            replay: preparation.replay,
            todo_snapshot: preparation.todo_snapshot,
            reply,
        },
        cancellation,
    )
}

pub(super) fn request_ask(
    events: &mpsc::SyncSender<WorkerEvent>,
    request: pp_core::interaction::AskRequest,
    cancellation: &CancellationToken,
) -> Result<Vec<pp_core::interaction::AskAnswer>> {
    pp_core::cancellation::check(cancellation)?;
    let call_id = request.call_id.clone();
    let answers = send_and_wait_ack(
        events,
        |reply| WorkerEvent::Ask {
            request,
            reply,
            cancellation: cancellation.clone(),
        },
        cancellation,
    );
    let _ = events.try_send(WorkerEvent::DialogFinished { call_id });
    pp_core::cancellation::check(cancellation)?;
    answers
}

impl WorkerEvent {
    pub(super) fn model_state(agent: &Agent) -> Self {
        Self::ModelState {
            revision: agent
                .runtime_models
                .as_ref()
                .map_or(0, |binding| binding.seen),
            model: agent.current_model().clone(),
            effort: agent.think_level(),
            source: agent.model_source(),
        }
    }

    pub(super) fn completed(started: Instant, result: Result<RunOutcome>) -> Self {
        Self::Completed {
            result,
            elapsed: started.elapsed(),
            finished_at: chrono::Local::now(),
        }
    }
}

/// worker 完整加载并验证后的模型目录快照。
pub(super) struct ModelListing {
    pub(super) roles: pp_config::config::ModelRoles,
    pub(super) default_effort: ThinkLevel,
    pub(super) choices: Vec<ModelChoice>,
    pub(super) default_reference: String,
    pub(super) favorites: Vec<String>,
    pub(super) usage: BTreeMap<String, u64>,
    /// `a`：手动覆盖快照，供 `/Roles` 只读行与来源标记恢复。
    pub(super) override_selection: Option<Selection>,
    /// 当前生效模型来源；重新加载目录后覆盖可能被 `ModelState` 校正。
    pub(super) source: ModelSource,
}

impl ModelListing {
    pub(super) fn from_agent(agent: &Agent) -> Result<Self> {
        Ok(Self {
            choices: agent.model_choices(),
            default_reference: agent.default_model_reference(),
            favorites: agent.favorite_models(),
            roles: agent.model_roles(),
            default_effort: agent.default_think_level(),
            usage: agent.model_usage()?,
            override_selection: agent.models().override_selection(),
            source: agent.model_source(),
        })
    }
}

/// 由 worker 一次性准备好的会话切换产物；全部就绪后才替换 UI 状态。
/// 历史 replay 通过 ExecutionPrepared 交接，这里只保留会话 ID。
pub(super) struct ResumeState {
    pub(super) id: String,
}

// worker 内部把 Agent 用量快照包装为进度事件；估算失败时跳过，不中断命令。
pub(super) fn usage_snapshot(agent: &Agent) -> Option<WorkerEvent> {
    agent
        .context_usage()
        .ok()
        .map(|usage| WorkerEvent::Progress(AgentEvent::ContextUpdated { usage }))
}
