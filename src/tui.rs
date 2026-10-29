mod app;
mod ask;
mod auth;
#[cfg(test)]
mod auth_tests;
mod changes;
mod edit_diff;
mod files;
mod markdown;
mod menu;
mod model;
mod notification;
mod plan;
mod plan_leave;
mod plan_list;
mod roles;
mod scrollback;
mod sessions;
mod skills;
mod statusline;
mod subagents;
mod surface;
mod tips;
mod todo;
mod tool_display;
mod transcript;
mod transcript_style;
mod tree;
mod welcome;

pub(crate) use transcript_style::display_text;

use crate::{
    agent::{
        Agent, ExecutionPreparation,
        context::CompactionOutcome,
        input::{InputQueue, PendingKind, QueueSnapshot},
        plan_management::{PlanListing, PlanOpenIntent, PlanSelection},
    },
    cancellation::{CancellationToken, RunOutcome},
    commands::{self, COMMANDS, Command, PlanCommand},
    config::{ModelChoice, NotificationOptions, ThinkLevel},
    events::AgentEvent,
    plan::{
        ExecutionContext, PlanLocation, PlanReview, PlanReviewReport, PlanStatus, ReviewToken,
        WorkMode, WorkflowState,
    },
    shortcuts::{FUNCTION_MODIFIER, platform_text},
    tools::{ChangesView, UndoResult, UndoToken},
};
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use menu::{MenuEvent, MenuItem, SelectionMenu};
use notification::{Level, MouseAction, Notifications};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders},
};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};
use statusline::{StatusContext, WorkspaceStatus};
use std::{
    io,
    path::Path,
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
    time::{Duration, Instant},
};
use transcript::Transcript;

const WARNING: &str = "write/edit only modify files inside the startup directory; bash has no directory isolation, so run in a trusted or temporary directory.\nForeground bash has an execution timeout; managed background tasks survive tool calls and stop at this user task's end or controlled exit. Ctrl + C stops managed background groups, but cleanup of other foreground children is not guaranteed. No shell/file side-effect rollback; deliberately detached descendants are not covered.";
// F1 帮助中的队列快捷键提示。
const STEERING_HINT: &str = platform_text(
    "Enter steer · ⌥ + Enter queue · ⌥ + Q pending messages · ⌥ + R resume queue",
    "Enter steer · Alt + Enter queue · Ctrl + Q pending messages · Ctrl + R resume queue",
);
const SHORTCUTS_TAIL: &str = platform_text(
    " · ↑↓ move/history/menu · Shift + Tab cycle thinking · Ctrl + F favorite selected model · Ctrl + S save selected model as default · ⌥ + S save selected effort as default · ⌥ + P choose model · F1 help · Tab complete · Esc cancel task/dismiss · Esc Esc session tree (empty input) · Ctrl + A / Ctrl + E line start/end · Ctrl + K delete to end of line · Ctrl + J newline · Ctrl + O expand/collapse all tool output · ⌥ + T expand/collapse TODO · ⌥ + PgUp/PgDn scroll TODO · Wheel/PgUp/PgDn scroll · Ctrl + C quit",
    " · ↑↓ move/history/menu · Shift + Tab cycle thinking · Ctrl + F favorite selected model · Ctrl + S save selected model/effort as default in its menu · Ctrl + P choose model · F1 help · Tab complete · Esc cancel task/dismiss · Esc Esc session tree (empty input) · Ctrl + A / Ctrl + E line start/end · Ctrl + K delete to end of line · Ctrl + J newline · Ctrl + O expand/collapse all tool output · Ctrl + T expand/collapse TODO · Ctrl + PgUp/PgDn scroll TODO · Wheel/PgUp/PgDn scroll · Ctrl + C quit",
);
const EFFORT_HINT: &str = platform_text(
    "Enter select · ⌥ + S set selected effort as default · Esc cancel",
    "Enter select · Ctrl + S set selected effort as default · Esc cancel",
);
const MOUSE_SCROLL_LINES: usize = 3;

pub(crate) fn run(
    agent: Result<Agent>,
    cwd: &Path,
    skills: crate::skills::SharedSkills,
    file_commands: commands::FileCommands,
    auto_continue: bool,
    notification_options: NotificationOptions,
    bootstrap: Option<crate::agent::Bootstrap>,
) -> Result<()> {
    // 初始化中途失败和 panic 同样恢复终端模式。
    let _guard = TerminalGuard;
    surface::initialize().context("initializing terminal")?;
    let mut surface = surface::Surface::new(io::stdout());
    let mut canvas = ratatui::Terminal::new(ratatui::backend::TestBackend::new(1, 1))?;
    let mut app = App::new_with_skills(agent, skills, file_commands, cwd, notification_options)?;
    app.bootstrap = bootstrap;
    if auto_continue && matches!(app.state, State::Ready) {
        // -c：Execute 保持原继续行为；Plan/Draft 提示继续输入；Plan/Ready 进入审阅。
        match (
            &app.workflow.mode,
            app.workflow.plan.as_ref().map(|plan| plan.status),
        ) {
            (WorkMode::Plan, Some(PlanStatus::Ready)) => app.request_plan_read(),
            (WorkMode::Plan, _) => app.notify(
                Level::Normal,
                "Plan mode is active; describe the next research or discussion input.".into(),
            ),
            (WorkMode::Execute, _) => {
                app.send_task(None);
            }
        }
    }
    let mut workspace = WorkspaceStatus::new(cwd)?;
    let mut dirty = true;
    let mut generation = app.conversation.native_generation();
    loop {
        dirty |= app.receive();
        dirty |= workspace.receive();
        dirty |= app.notifications.tick(Instant::now());
        dirty |= app.conversation.tick(Instant::now());
        dirty |= app.review_transcript_tick(Instant::now());
        dirty |= app.token_speed.tick(Instant::now());
        if dirty {
            if generation != app.conversation.native_generation() {
                surface.reset_history();
                generation = app.conversation.native_generation();
                app.history_browser = false;
            }
            paint_surface(
                &mut app,
                &workspace.context,
                &mut canvas,
                &mut surface,
                false,
            )
            .context("drawing terminal")?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(50)).context("polling terminal input")? {
            let event = event::read().context("reading terminal input")?;
            if app.handle(event, &mut dirty) {
                if let Some(cancellation) = &app.active_cancellation {
                    cancellation.cancel();
                }
                // 在资源销毁前封存已接收内容；之后不再调用 App::draw。
                paint_surface(
                    &mut app,
                    &workspace.context,
                    &mut canvas,
                    &mut surface,
                    true,
                )
                .context("finishing terminal transcript")?;
                app.shutdown_background_tasks()?;
                surface
                    .finish(canvas.backend().buffer().area.height)
                    .context("restoring shell cursor")?;
                // 恢复终端模式后输出，避免提示与状态栏或 shell 提示符混在同一行。
                drop(_guard);
                crate::write_resume_hint(&mut io::stderr(), app.session_id.as_deref())?;
                return Ok(());
            }
        }
    }
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        surface::restore();
    }
}

fn paint_surface(
    app: &mut App,
    context: &StatusContext,
    canvas: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
    surface: &mut surface::Surface<impl io::Write>,
    exiting: bool,
) -> Result<()> {
    let (width, height) = crossterm::terminal::size()?;
    let area = Rect::new(0, 0, width.max(1), height.max(1));
    if canvas.backend().buffer().area != area {
        canvas.backend_mut().resize(area.width, area.height);
        canvas.resize(area)?;
    }
    let alternate = !exiting && app.needs_alternate();
    app.native_render = !alternate;
    if !alternate {
        let conversation_height = app.main_layout(area).conversation.height;
        while let Some(batch) =
            app.conversation
                .next_native_batch(width, conversation_height, exiting)
        {
            app.conversation.preview_native_batch(&batch);
            let mut rendered = None;
            canvas.draw(|frame| {
                app.draw(frame, context);
                rendered = Some(frame.buffer_mut().clone());
            })?;
            let buffer = if exiting {
                app.normal_snapshot.as_ref().expect("normal frame")
            } else {
                rendered.as_ref().expect("rendered frame")
            };
            let cursor = canvas
                .backend()
                .cursor_visible()
                .then(|| canvas.backend().cursor_position());
            surface.paint(buffer, cursor, false, &batch.rows)?;
            app.conversation.acknowledge_native_batch(batch);
        }
    }
    let mut rendered = None;
    canvas.draw(|frame| {
        app.draw(frame, context);
        rendered = Some(frame.buffer_mut().clone());
    })?;
    let cursor = canvas
        .backend()
        .cursor_visible()
        .then(|| canvas.backend().cursor_position());
    let buffer = if exiting {
        app.normal_snapshot.as_ref().expect("normal frame")
    } else {
        rendered.as_ref().expect("rendered frame")
    };
    surface.paint(buffer, cursor, alternate, &[])?;
    Ok(())
}

enum WorkerCommand {
    ReloadModels,
    Shell {
        command: String,
        call_id: String,
        cancellation: CancellationToken,
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
    SwitchModel(String),
    SetThinkLevel(ThinkLevel),
    SaveDefaultThinkLevel(ThinkLevel),
    SaveDefaultModel(String),
    SaveModelRole(
        crate::config::ModelRole,
        Option<crate::config::RoleAssignment>,
    ),
    ToggleFavoriteModel(String),
    ListSessions,
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
        token: crate::plan::ReviewToken,
        context: ExecutionContext,
        cancellation: CancellationToken,
        input: InputQueue,
    },
    // 独立 reviewer：进度经 ReviewProgress 事件回流，终态为 PlanReviewed。
    ReviewPlan {
        token: crate::plan::ReviewToken,
        cancellation: CancellationToken,
        input: InputQueue,
    },
}
enum WorkerEvent {
    DialogFinished {
        call_id: String,
    },
    ModelsReloaded(Result<ModelListing>),
    Ask {
        request: crate::interaction::AskRequest,
        reply: mpsc::SyncSender<Vec<crate::interaction::AskAnswer>>,
        cancellation: CancellationToken,
    },
    Progress(AgentEvent),
    ShellCompleted(Result<()>),
    Completed {
        result: Result<RunOutcome>,
        elapsed: Duration,
        finished_at: chrono::DateTime<chrono::Local>,
    },
    SessionStarted(String),
    // 压缩终态：与普通任务 Completed 是不同的判别值，不触发任务收尾展示。
    Compacted(Result<CompactionOutcome>),
    Reset,
    ModelSwitched(Result<(ModelChoice, ThinkLevel)>),
    ThinkLevelSet(Result<ThinkLevel>),
    DefaultThinkLevelSaved(Result<ThinkLevel>),
    DefaultModelSaved(Result<String>),
    ModelRolesSaved(Result<(crate::config::ModelRoles, String, ThinkLevel)>),
    ModelState(ModelChoice, ThinkLevel),
    FavoriteModelsSaved(Result<Vec<String>>),
    SessionsListed(Result<crate::agent::persistence::SessionListing>),
    SessionDeleted(String, Result<()>),
    SessionResumed(Result<ResumeState>),
    TreeRead(Result<crate::agent::persistence::tree::View>),
    TreeNavigated(Result<crate::agent::TreeNavigation>),
    ChangesRead(ChangesView),
    FileUndone(Result<UndoResult>),
    // /plan-list 管理结果；失败保留旧列表与输入，只展示可见错误。
    PlansListed(Result<PlanListing>),
    ListedPlanRead(Result<PlanSelection>),
    // 携带完整执行准备：UI 安装清理句柄并采纳 replay，但不自动请求模型。
    ListedPlanOpened(Result<ExecutionPreparation>),
    ListedPlanRenamed(Result<PlanListing>),
    ListedPlanDeleted(Result<PlanListing>),
    // 计划模式转换结果；Err 时 worker 会紧随一份真实 workflow 快照，
    // 避免 UI 在“持久化成功但 MCP 关闭失败”的失败中显示 EXEC。
    WorkflowChanged(Result<WorkflowState>),
    PlanRead(Result<PlanReview>),
    // 审查进度：只进独立 review transcript，不进主对话、用量或派发。
    ReviewProgress(AgentEvent),
    // reviewer 终态：成功携带最终报告；失败/取消保留上次完整报告。
    PlanReviewed(Result<PlanReviewReport>),
    // 无专属结果事件的动作（普通运行/压缩/切换模型/树导航）之后发布的快照。
    Workflow(WorkflowState),
    // 转换失败后重取当前 registry 的退出清理句柄；覆盖被保留的 prepared registry。
    Cleanup(crate::mcp::Cleanup),
    // 执行准备一次性交接：UI 安装清理句柄并经 reply 确认后 worker 才继续。
    ExecutionPrepared {
        workflow: WorkflowState,
        session_id: Option<String>,
        replay: Option<Vec<crate::agent::persistence::ReplayRecord>>,
        cleanup: crate::mcp::Cleanup,
        /// 交接快照：UI 必须在采纳 replay/替换 workflow 之后、ACK 之前渲染，
        /// Fresh 清理后才入卡，源会话不能先 emit 再被清掉。
        todo_snapshot: Option<AgentEvent>,
        reply: mpsc::SyncSender<()>,
    },
}

/// 发布 ExecutionPrepared 并等待 UI 安装清理句柄后的 ACK。
/// ACK 失败或取消时返回错误；事件随通道消失时其 Cleanup Drop 会关闭新 slots。
/// 转换失败后的纠正快照：发布真实 workflow，并刷新退出清理句柄以覆盖
/// 可能被保留的 registry；所有 Plan 转换失败路径共用。
fn publish_transition_failure(agent: &Agent, snapshots: &mut Vec<WorkerEvent>) {
    push_workflow_snapshot(agent, snapshots);
    if let Ok(cleanup) = agent.mcp_cleanup() {
        snapshots.push(WorkerEvent::Cleanup(cleanup));
    }
}

/// Publish the workflow snapshot so the UI never keeps a stale plan/mode.
fn push_workflow_snapshot(agent: &Agent, snapshots: &mut Vec<WorkerEvent>) {
    snapshots.push(WorkerEvent::ModelState(
        agent.current_model().clone(),
        agent.think_level(),
    ));
    snapshots.push(WorkerEvent::Workflow(agent.workflow().clone()));
}

/// Status-bar usage plus workflow snapshot after a task or state boundary.
fn push_runtime_snapshot(agent: &Agent, snapshots: &mut Vec<WorkerEvent>) {
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
        return Err(crate::cancellation::Cancelled.into());
    }
    loop {
        crate::cancellation::check(cancellation)?;
        match ack.recv_timeout(Duration::from_millis(50)) {
            Ok(value) => return Ok(value),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                cancellation.cancel();
                return Err(crate::cancellation::Cancelled.into());
            }
        }
    }
}

fn publish_prepared(
    events: &mpsc::SyncSender<WorkerEvent>,
    preparation: ExecutionPreparation,
    cancellation: &CancellationToken,
) -> Result<()> {
    send_and_wait_ack(
        events,
        |reply| WorkerEvent::ExecutionPrepared {
            workflow: preparation.workflow,
            session_id: preparation.session_id,
            replay: preparation.replay,
            cleanup: preparation.cleanup,
            todo_snapshot: preparation.todo_snapshot,
            reply,
        },
        cancellation,
    )
}

fn request_ask(
    events: &mpsc::SyncSender<WorkerEvent>,
    request: crate::interaction::AskRequest,
    cancellation: &CancellationToken,
) -> Result<Vec<crate::interaction::AskAnswer>> {
    crate::cancellation::check(cancellation)?;
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
    crate::cancellation::check(cancellation)?;
    answers
}

impl WorkerEvent {
    fn completed(started: Instant, result: Result<RunOutcome>) -> Self {
        Self::Completed {
            result,
            elapsed: started.elapsed(),
            finished_at: chrono::Local::now(),
        }
    }
}

/// worker 完整加载并验证后的模型目录快照。
struct ModelListing {
    roles: crate::config::ModelRoles,
    default_effort: ThinkLevel,
    choices: Vec<ModelChoice>,
    default_reference: String,
    favorites: Vec<String>,
}

impl ModelListing {
    fn from_agent(agent: &Agent) -> Self {
        Self {
            choices: agent.model_choices(),
            default_reference: agent.default_model_reference().to_owned(),
            favorites: agent.favorite_models().to_vec(),
            roles: agent.model_roles().clone(),
            default_effort: agent.default_think_level(),
        }
    }
}

/// 由 worker 一次性准备好的会话切换产物；全部就绪后才替换 UI 状态。
/// 历史 replay 通过 ExecutionPrepared 交接，这里只保留会话 ID。
struct ResumeState {
    id: String,
}

#[derive(Clone, Copy)]
enum State {
    Ready,
    Running,
    RunningShell,
    Cancelling,
    Compacting,
    Resetting,
    SwitchingModel,
    SavingModelSettings,
    SettingThinkLevel,
    ListingSessions,
    Resuming,
    ReadingTree,
    NavigatingTree,
    ReadingChanges,
    UndoingFile,
    ListingPlans,
    ReadingListedPlan,
    OpeningListedPlan,
    RenamingPlan,
    DeletingPlan,
    Unavailable,
    ChangingPlan,
    ReadingPlan,
    PreparingExecution,
    // 独立 reviewer 运行中：审批/派发/命令全部暂停，草稿仍可编辑。
    ReviewingPlan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuAction {
    Command(usize),
    FileCommand(usize),
    Model(usize),
    Effort(ThinkLevel),
    File(usize),
    FileDirectory(usize),
    FileParent,
    PendingInput(u64),
}

struct ModelPickerDraft {
    input: TextArea<'static>,
    shell_mode: bool,
    history_index: Option<usize>,
    history_draft: Option<(TextArea<'static>, bool)>,
    menu: Option<SelectionMenu<MenuAction>>,
    menu_dismissed: bool,
}

#[derive(Default)]
struct EscapeTap(Option<Instant>);

impl EscapeTap {
    fn reset(&mut self) {
        self.0 = None;
    }
    fn press(&mut self, now: Instant) -> bool {
        if self
            .0
            .take()
            .is_some_and(|last| now.saturating_duration_since(last) <= Duration::from_millis(500))
        {
            true
        } else {
            self.0 = Some(now);
            false
        }
    }
}

struct App {
    auth_worker: auth::Worker,
    auth_panel: Option<auth::Panel>,
    auth_cancellation: Option<CancellationToken>,
    auth_pending: bool,
    bootstrap: Option<crate::agent::Bootstrap>,
    model_refresh_pending: bool,
    conversation: Transcript,
    todo_panel: todo::Panel,
    native_render: bool,
    normal_snapshot: Option<ratatui::buffer::Buffer>,
    history_browser: bool,
    notifications: Notifications,
    show_welcome: bool,
    input: TextArea<'static>,
    shell_mode: bool,
    input_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: Option<(TextArea<'static>, bool)>,
    state: State,
    next_shell_id: u64,
    active_cancellation: Option<CancellationToken>,
    input_queue: InputQueue,
    queue_paused: bool,
    queue_menu: bool,
    queue_menu_revision: Option<u64>,
    queue_snapshot: Option<QueueSnapshot>,
    ask_panel: Option<ask::Panel>,
    ask_reply: Option<mpsc::SyncSender<Vec<crate::interaction::AskAnswer>>>,
    ask_id: Option<String>,
    ask_cancellation: Option<CancellationToken>,
    commands: Option<Sender<WorkerCommand>>,
    events: Option<Receiver<WorkerEvent>>,
    unavailable: Option<String>,
    choices: Vec<ModelChoice>,
    current_model: Option<ModelChoice>,
    default_model_reference: Option<String>,
    favorite_models: Vec<String>,
    think_level: ThinkLevel,
    default_think_level: ThinkLevel,
    menu: Option<SelectionMenu<MenuAction>>,
    menu_dismissed: bool,
    model_picker_draft: Option<ModelPickerDraft>,
    model_panel: Option<model::Panel>,
    roles_panel: Option<roles::Panel>,
    model_roles: crate::config::ModelRoles,
    secondary_status: Option<String>,
    skill_worker: skills::Worker,
    skill_panel: Option<skills::Panel>,
    skills_pending: bool,
    session_panel: Option<sessions::Panel>,
    tree_panel: Option<tree::Panel>,
    changes_panel: Option<changes::Panel>,
    subagent_panel: Option<subagents::Panel>,
    subagent_autowake: bool,
    subagent_control: std::sync::Arc<std::sync::Mutex<Option<crate::subagent::Binding>>>,
    plan_list_panel: Option<plan_list::Panel>,
    // 列表动作读取 selection 后继续执行的后续步骤。
    plan_list_pending: Option<plan_list::PendingOp>,
    // `n` 在 Plan 模式下先解除绑定（LeavePlan），成功后立即重新进入 Plan 待命。
    plan_reenter_standby: bool,
    plan_leave_confirmation: Option<plan_leave::Panel>,
    escape_tap: EscapeTap,
    session_id: Option<String>,
    // 最近一次 ContextUpdated 快照；渲染直接使用，不逐帧重新估算。
    usage: Option<crate::context::ContextUsage>,
    token_speed: statusline::TokenSpeed,
    mcp_cleanup: Option<crate::mcp::Cleanup>,
    background_cleanup: Option<crate::tools::BackgroundCleanup>,
    file_worker: files::Worker,
    file_paths: Vec<String>,
    file_directory: String,
    file_query: Option<files::Query>,
    files_pending: bool,
    file_commands: commands::FileCommands,
    // worker 发布的 workflow 快照；UI 不是权限权威，只用于展示与派发守卫。
    workflow: WorkflowState,
    plan_panel: Option<plan::Panel>,
    // 仅在正文变化时重算的 (completed, total) 进度缓存。
    plan_progress: (usize, usize),
    // ExecutionPrepared 中 replay 重绘产生的显示错误，等待 SessionResumed 一并通知。
    resume_note: Option<String>,
    // EnterPlan 因 MCP 清理失败而 Err：下一次 /plan 优先重发 EnterPlan 重试。
    plan_retry_enter: bool,
    // EnterPlan 成功后再提示待命或发送提示词，失败时不得发送。
    plan_after_enter: Option<PlanEnterAction>,
    // 独立审查页的临时 transcript；审查进度不进主对话、不更新用量。
    review_transcript: Option<Transcript>,
    // 审查成功后待下一次 ReadPlan 消费的最终报告（刷新面板或降级展示）。
    plan_review_pending: Option<PlanReviewReport>,
}

enum PlanEnterAction {
    // 待命提示：进入 Plan 不绑定旧稿，也不打开审阅面板。
    Standby,
    Prompt(String),
}

// worker 内部把 Agent 用量快照包装为进度事件；估算失败时跳过，不中断命令。
fn usage_snapshot(agent: &Agent) -> Option<WorkerEvent> {
    agent
        .context_usage()
        .ok()
        .map(|usage| WorkerEvent::Progress(AgentEvent::ContextUpdated { usage }))
}

// 合并启动弹窗摘要：保留最高严重程度，摘要始终显示在既有提示之前。
fn merge_startup_notice(startup_notice: &mut Option<(Level, String)>, summary: String) {
    *startup_notice = Some(match startup_notice.take() {
        None => (Level::Important, summary),
        Some((Level::Error, message)) => (Level::Error, format!("{summary}\n{message}")),
        Some((_, message)) => (Level::Important, format!("{summary}\n{message}")),
    });
}

fn editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_wrap_mode(WrapMode::WordOrGlyph);
    editor.set_cursor_line_style(Style::default());
    editor.set_cursor_style(Style::default());
    editor
}

/// 限流等待时长的展示形式：整秒不带小数，非整秒保留一位小数。
pub(crate) fn format_wait(wait_ms: u64) -> String {
    if wait_ms.is_multiple_of(1000) {
        format!("{}s", wait_ms / 1000)
    } else {
        format!("{:.1}s", wait_ms as f64 / 1000.0)
    }
}

/// 从登记的相对路径推导生命周期目录；仅用于只读展示，不是授权依据。
fn plan_location_from_path(path: &str) -> Option<PlanLocation> {
    let rest = path.strip_prefix(".pp/")?;
    let (directory, _) = rest.split_once('/')?;
    Some(match directory {
        "drafts" => PlanLocation::Drafts,
        "plans" => PlanLocation::Plans,
        "done" => PlanLocation::Done,
        _ => return None,
    })
}

impl App {
    #[cfg(test)]
    fn receive_event_for_test(&mut self, event: WorkerEvent) {
        let (events, receiver) = mpsc::channel();
        events.send(event).expect("test channel");
        let original = self.events.replace(receiver);
        self.receive();
        self.events = original;
    }
}

#[cfg(test)]
mod tests;

fn dialog_guard<'a>(
    queue: &'a crate::interaction::DialogQueue,
    token: &CancellationToken,
) -> Result<crate::interaction::DialogPermit<'a>> {
    queue.acquire(token)
}
