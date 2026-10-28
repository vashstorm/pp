mod ask;
mod changes;
mod files;
mod markdown;
mod menu;
mod model;
mod notification;
mod plan;
mod plan_list;
mod scrollback;
mod sessions;
mod skills;
mod statusline;
mod surface;
mod tips;
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
    " · ↑↓ move/history/menu · Shift + Tab cycle thinking · Ctrl + F favorite selected model · Ctrl + S save selected model as default · ⌥ + S save selected effort as default · ⌥ + P choose model · F1 help · Tab complete · Esc cancel task/dismiss · Esc Esc session tree (empty input) · Ctrl + A / Ctrl + E line start/end · Ctrl + K delete to end of line · Ctrl + J newline · Ctrl + O expand/collapse all tool output · Wheel/PgUp/PgDn scroll · Ctrl + C quit",
    " · ↑↓ move/history/menu · Shift + Tab cycle thinking · Ctrl + F favorite selected model · Ctrl + S save selected model/effort as default in its menu · Ctrl + P choose model · F1 help · Tab complete · Esc cancel task/dismiss · Esc Esc session tree (empty input) · Ctrl + A / Ctrl + E line start/end · Ctrl + K delete to end of line · Ctrl + J newline · Ctrl + O expand/collapse all tool output · Wheel/PgUp/PgDn scroll · Ctrl + C quit",
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
) -> Result<()> {
    // 初始化中途失败和 panic 同样恢复终端模式。
    let _guard = TerminalGuard;
    surface::initialize().context("initializing terminal")?;
    let mut surface = surface::Surface::new(io::stdout());
    let mut canvas = ratatui::Terminal::new(ratatui::backend::TestBackend::new(1, 1))?;
    let mut app = App::new_with_skills(agent, skills, file_commands, cwd, notification_options)?;
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
                return surface
                    .finish(canvas.backend().buffer().area.height)
                    .context("restoring shell cursor");
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
        let available = height.saturating_sub(u16::from(height >= 2));
        let bordered = available >= 3 && width >= 3;
        let input = available
            .min(app.input.lines().len().clamp(1, 6) as u16 + if bordered { 2 } else { 0 });
        let conversation_height = available.saturating_sub(input);
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
    Ask {
        request: crate::interaction::AskRequest,
        reply: mpsc::SyncSender<Vec<crate::interaction::AskAnswer>>,
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
    let answers = send_and_wait_ack(
        events,
        |reply| WorkerEvent::Ask { request, reply },
        cancellation,
    )?;
    crate::cancellation::check(cancellation)?;
    Ok(answers)
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
    conversation: Transcript,
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
    secondary_status: Option<String>,
    skill_worker: skills::Worker,
    skill_panel: Option<skills::Panel>,
    skills_pending: bool,
    session_panel: Option<sessions::Panel>,
    tree_panel: Option<tree::Panel>,
    changes_panel: Option<changes::Panel>,
    plan_list_panel: Option<plan_list::Panel>,
    // 列表动作读取 selection 后继续执行的后续步骤。
    plan_list_pending: Option<plan_list::PendingOp>,
    // `n` 在 Plan 模式下先解除绑定（LeavePlan），成功后立即重新进入 Plan 待命。
    plan_reenter_standby: bool,
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

impl App {
    #[cfg(test)]
    fn new(agent: Result<Agent>) -> Result<Self> {
        let skills = agent
            .as_ref()
            .map(|agent| agent.skills())
            .unwrap_or_else(|_| crate::skills::Skills::default().shared());
        Self::new_with_skills(
            agent,
            skills,
            commands::FileCommands::default(),
            Path::new("."),
            NotificationOptions::default(),
        )
    }

    fn new_with_skills(
        agent: Result<Agent>,
        skills: crate::skills::SharedSkills,
        file_commands: commands::FileCommands,
        cwd: &Path,
        notification_options: NotificationOptions,
    ) -> Result<Self> {
        let diagnostics = skills
            .lock()
            .map_err(|_| anyhow::anyhow!("skill registry unavailable"))?
            .diagnostics
            .clone();
        let mut app = Self {
            native_render: false,
            normal_snapshot: None,
            history_browser: false,
            conversation: Transcript::with_read_preview(
                agent.as_ref().is_ok_and(Agent::read_tool_result_preview),
            ),
            notifications: Notifications::new(notification_options),
            show_welcome: true,
            input: editor(),
            shell_mode: false,
            input_history: Vec::new(),
            history_index: None,
            history_draft: None,
            state: State::Unavailable,
            next_shell_id: 0,
            active_cancellation: None,
            input_queue: InputQueue::default(),
            queue_paused: false,
            queue_menu: false,
            queue_menu_revision: None,
            queue_snapshot: None,
            ask_panel: None,
            ask_reply: None,
            commands: None,
            events: None,
            unavailable: None,
            choices: Vec::new(),
            current_model: None,
            default_model_reference: None,
            favorite_models: Vec::new(),
            think_level: ThinkLevel::None,
            default_think_level: ThinkLevel::default(),
            menu: None,
            menu_dismissed: false,
            model_picker_draft: None,
            model_panel: None,
            secondary_status: None,
            skill_worker: skills::Worker::new(skills)?,
            skill_panel: None,
            skills_pending: false,
            session_panel: None,
            tree_panel: None,
            changes_panel: None,
            plan_list_panel: None,
            plan_list_pending: None,
            plan_reenter_standby: false,
            escape_tap: EscapeTap::default(),
            session_id: None,
            usage: None,
            token_speed: statusline::TokenSpeed::default(),
            mcp_cleanup: None,
            background_cleanup: None,
            file_commands,
            file_worker: files::Worker::new(cwd)?,
            file_paths: Vec::new(),
            file_directory: String::new(),
            file_query: None,
            files_pending: false,
            workflow: WorkflowState::default(),
            plan_panel: None,
            plan_progress: (0, 0),
            resume_note: None,
            plan_retry_enter: false,
            plan_after_enter: None,
            review_transcript: None,
            plan_review_pending: None,
        };
        let mut startup_notice = None;
        match agent {
            Ok(mut agent) => {
                // 取 replay、模型字段与 MCP 清理句柄必须发生在 agent move 进 worker 之前。
                let session_id = agent.session_id().map(str::to_owned);
                let replay = agent.take_replay();
                let current_model = agent.current_model().clone();
                let think_level = agent.think_level();
                let default_think_level = agent.default_think_level();
                let mcp_cleanup = agent.mcp_cleanup()?;
                app.background_cleanup = Some(agent.background_cleanup());
                app.choices = agent.model_choices();
                app.favorite_models = agent.favorite_models().to_vec();
                app.default_model_reference = Some(agent.default_model_reference().to_owned());
                let (commands, receiver) = mpsc::channel();
                let (sender, events) = mpsc::sync_channel(256);
                let ask_events = sender.clone();
                agent.set_ask_handler(Box::new(move |request, cancellation| {
                    request_ask(&ask_events, request, cancellation)
                }));
                let usage = agent.context_usage().ok();
                // 经 set_workflow 走统一入口：恢复的计划正文必须同步种子进度缓存，
                // 否则 /plan view 正文未变时不重算，标题停留 0/0。
                app.set_workflow(agent.workflow().clone());
                // Deliberately detached: quitting must not wait for HTTP or foreground tools.
                thread::Builder::new()
                    .name("pp-agent".into())
                    .spawn(move || {
                        while let Ok(command) = receiver.recv() {
                            let started = Instant::now();
                            let mut emit = |event| {
                                let _ = sender.send(WorkerEvent::Progress(event));
                            };
                            // 状态切换成功后紧随一份新用量快照，与动作保持 FIFO 顺序。
                            let mut snapshots: Vec<WorkerEvent> = Vec::new();
                            let event = match command {
                                WorkerCommand::Shell {
                                    command,
                                    call_id,
                                    cancellation,
                                } => {
                                    let result = agent.run_shell(
                                        &command,
                                        &call_id,
                                        &cancellation,
                                        &mut emit,
                                    );
                                    let cleanup_failed = result.as_ref().is_err_and(|error| {
                                        error.chain().any(|cause| {
                                            cause.is::<crate::cancellation::CleanupError>()
                                        })
                                    });
                                    if sender.send(WorkerEvent::ShellCompleted(result)).is_err()
                                        || cleanup_failed
                                    {
                                        break;
                                    }
                                    continue;
                                }
                                WorkerCommand::Run {
                                    prompt,
                                    cancellation,
                                    input,
                                } => {
                                    let outcome =
                                        agent.run(&prompt, &cancellation, Some(&input), &mut emit);
                                    // 计划工具可能修改 workflow；终态后同步一份快照。
                                    push_workflow_snapshot(&agent, &mut snapshots);
                                    WorkerEvent::completed(started, outcome)
                                }
                                WorkerCommand::Continue {
                                    cancellation,
                                    input,
                                } => {
                                    let outcome = agent.continue_session(
                                        &cancellation,
                                        Some(&input),
                                        &mut emit,
                                    );
                                    push_workflow_snapshot(&agent, &mut snapshots);
                                    WorkerEvent::completed(started, outcome)
                                }
                                WorkerCommand::Compact { cancellation } => {
                                    let result = agent.compact(&cancellation, &mut emit);
                                    // 终态事件之前先发布最终用量快照；
                                    // 成功、失败、取消、未变化都会刷新状态栏。
                                    push_runtime_snapshot(&agent, &mut snapshots);
                                    WorkerEvent::Compacted(result)
                                }
                                WorkerCommand::Reset => {
                                    agent.reset();
                                    push_runtime_snapshot(&agent, &mut snapshots);
                                    WorkerEvent::Reset
                                }
                                WorkerCommand::SetThinkLevel(level) => {
                                    let result =
                                        agent.set_think_level(level).map(|()| agent.think_level());
                                    if result.is_ok() {
                                        push_runtime_snapshot(&agent, &mut snapshots);
                                    }
                                    WorkerEvent::ThinkLevelSet(result)
                                }
                                WorkerCommand::SaveDefaultThinkLevel(level) => {
                                    WorkerEvent::DefaultThinkLevelSaved(
                                        agent.save_default_think_level(level).map(|()| level),
                                    )
                                }
                                WorkerCommand::SaveDefaultModel(reference) => {
                                    WorkerEvent::DefaultModelSaved(
                                        agent.save_default_model(&reference).map(|()| reference),
                                    )
                                }
                                WorkerCommand::ToggleFavoriteModel(reference) => {
                                    WorkerEvent::FavoriteModelsSaved(
                                        agent.toggle_favorite_model(&reference),
                                    )
                                }
                                WorkerCommand::SwitchModel(reference) => {
                                    let result = agent.switch_model(&reference).map(|()| {
                                        (agent.current_model().clone(), agent.think_level())
                                    });
                                    if result.is_ok() {
                                        push_runtime_snapshot(&agent, &mut snapshots);
                                    }
                                    WorkerEvent::ModelSwitched(result)
                                }
                                WorkerCommand::ReadChanges => {
                                    WorkerEvent::ChangesRead(agent.changes())
                                }
                                WorkerCommand::UndoFile(token) => {
                                    let result = agent.undo_file(&token);
                                    if result.is_err() {
                                        snapshots.push(WorkerEvent::ChangesRead(agent.changes()));
                                    }
                                    WorkerEvent::FileUndone(result)
                                }
                                WorkerCommand::ReadTree => {
                                    WorkerEvent::TreeRead(agent.session_tree())
                                }
                                WorkerCommand::NavigateTree(id) => {
                                    let result = agent.navigate_tree(&id);
                                    if result.is_ok() {
                                        push_runtime_snapshot(&agent, &mut snapshots);
                                    }
                                    WorkerEvent::TreeNavigated(result)
                                }
                                WorkerCommand::ListSessions => WorkerEvent::SessionsListed(
                                    crate::config::global_dir()
                                        .map(|dir| dir.join("sessions"))
                                        .and_then(|dir| crate::agent::persistence::list(&dir)),
                                ),
                                WorkerCommand::DeleteSession(id) => {
                                    let result = crate::config::global_dir()
                                        .map(|dir| dir.join("sessions"))
                                        .and_then(|dir| {
                                            crate::agent::persistence::delete(&dir, &id)
                                        });
                                    WorkerEvent::SessionDeleted(id, result)
                                }
                                WorkerCommand::Resume { id, cancellation } => {
                                    let result = agent.resume(&id, &cancellation);
                                    match result {
                                        Ok(preparation) => {
                                            // 先交接清理句柄并等待 UI 确认，再补会话通知。
                                            let prepared = publish_prepared(
                                                &sender,
                                                preparation,
                                                &cancellation,
                                            );
                                            WorkerEvent::SessionResumed(match prepared {
                                                Ok(()) => Ok(ResumeState { id }),
                                                Err(error) => Err(error),
                                            })
                                        }
                                        Err(error) => WorkerEvent::SessionResumed(Err(error)),
                                    }
                                }
                                WorkerCommand::EnterPlan { cancellation } => {
                                    let result = agent.enter_plan(&cancellation);
                                    if result.is_ok() {
                                        if let Some(event) = usage_snapshot(&agent) {
                                            snapshots.push(event);
                                        }
                                    } else {
                                        // 转换失败也可能已持久化 Plan；发布真实模式避免假 EXEC，
                                        // 并刷新退出清理句柄以覆盖可能被保留的 registry。
                                        publish_transition_failure(&agent, &mut snapshots);
                                    }
                                    WorkerEvent::WorkflowChanged(result)
                                }
                                WorkerCommand::ReadPlan => WorkerEvent::PlanRead(agent.read_plan()),
                                WorkerCommand::ListPlans => {
                                    WorkerEvent::PlansListed(agent.list_plans())
                                }
                                WorkerCommand::ReadListedPlan { id } => {
                                    WorkerEvent::ListedPlanRead(agent.read_listed_plan(&id))
                                }
                                WorkerCommand::OpenListedPlan {
                                    selection,
                                    intent,
                                    cancellation,
                                } => WorkerEvent::ListedPlanOpened(agent.open_listed_plan(
                                    &selection,
                                    intent,
                                    &cancellation,
                                )),
                                WorkerCommand::RenameListedPlan {
                                    selection,
                                    file_name,
                                } => WorkerEvent::ListedPlanRenamed(
                                    agent.rename_listed_plan(selection, file_name),
                                ),
                                WorkerCommand::DeleteListedPlan { selection } => {
                                    WorkerEvent::ListedPlanDeleted(
                                        agent.delete_listed_plan(selection),
                                    )
                                }
                                command @ (WorkerCommand::LeavePlan { .. }
                                | WorkerCommand::SavePlanAndLeave { .. }) => {
                                    let (result, cancellation) = match command {
                                        WorkerCommand::LeavePlan { cancellation } => {
                                            (agent.leave_plan(&cancellation), cancellation)
                                        }
                                        WorkerCommand::SavePlanAndLeave {
                                            token,
                                            cancellation,
                                            input,
                                        } => (
                                            agent.save_plan_and_leave(token, &input, &cancellation),
                                            cancellation,
                                        ),
                                        _ => unreachable!(),
                                    };
                                    match result {
                                        Ok(preparation) => {
                                            match publish_prepared(
                                                &sender,
                                                preparation,
                                                &cancellation,
                                            ) {
                                                Ok(()) => WorkerEvent::WorkflowChanged(Ok(agent
                                                    .workflow()
                                                    .clone())),
                                                Err(error) => {
                                                    publish_transition_failure(
                                                        &agent,
                                                        &mut snapshots,
                                                    );
                                                    WorkerEvent::WorkflowChanged(Err(error))
                                                }
                                            }
                                        }
                                        Err(error) => {
                                            publish_transition_failure(&agent, &mut snapshots);
                                            WorkerEvent::WorkflowChanged(Err(error))
                                        }
                                    }
                                }
                                WorkerCommand::ApprovePlan {
                                    token,
                                    context,
                                    cancellation,
                                    input,
                                } => {
                                    match agent.approve_plan(
                                        token,
                                        context,
                                        &input,
                                        &cancellation,
                                        &mut emit,
                                    ) {
                                        Ok(preparation) => {
                                            match publish_prepared(
                                                &sender,
                                                preparation,
                                                &cancellation,
                                            ) {
                                                // UI 确认后才启动模型执行。
                                                Ok(()) => WorkerEvent::completed(
                                                    started,
                                                    agent.continue_session(
                                                        &cancellation,
                                                        Some(&input),
                                                        &mut emit,
                                                    ),
                                                ),
                                                Err(error) => {
                                                    publish_transition_failure(
                                                        &agent,
                                                        &mut snapshots,
                                                    );
                                                    WorkerEvent::WorkflowChanged(Err(error))
                                                }
                                            }
                                        }
                                        Err(error) => {
                                            // 审批失败可能已在 worker 内导入新修订（计划变更）。
                                            publish_transition_failure(&agent, &mut snapshots);
                                            WorkerEvent::WorkflowChanged(Err(error))
                                        }
                                    }
                                }
                                WorkerCommand::ReviewPlan {
                                    token,
                                    cancellation,
                                    input,
                                } => {
                                    // 审查进度走独立事件流；不发布普通 Progress/用量快照。
                                    let mut review_emit = |event| {
                                        let _ = sender.send(WorkerEvent::ReviewProgress(event));
                                    };
                                    let result = agent.review_plan(
                                        token,
                                        &input,
                                        &cancellation,
                                        &mut review_emit,
                                    );
                                    // 成功时报告已由 write_workflow 登记；
                                    // 失败/取消保留旧报告。快照纠正显示不引入新权限。
                                    push_workflow_snapshot(&agent, &mut snapshots);
                                    WorkerEvent::PlanReviewed(result)
                                }
                            };
                            // Report new IDs even on task failure, before Completed.
                            if let Some(id) = agent.take_session_notice()
                                && sender.send(WorkerEvent::SessionStarted(id)).is_err()
                            {
                                break;
                            }
                            if sender.send(event).is_err() {
                                break;
                            }
                            for event in snapshots {
                                if sender.send(event).is_err() {
                                    break;
                                }
                            }
                        }
                    })
                    .context("starting agent worker")?;
                app.commands = Some(commands);
                app.events = Some(events);
                app.state = State::Ready;
                app.mcp_cleanup = Some(mcp_cleanup);
                app.usage = usage;
                if let Some(id) = session_id {
                    let choices = std::mem::take(&mut app.choices);
                    let error = app.adopt_session(
                        replay,
                        choices,
                        current_model,
                        think_level,
                        default_think_level,
                    );
                    app.session_id = Some(id.clone());
                    startup_notice = Some(match error {
                        Some(error) => (Level::Error, format!("Resumed session: {id}\n{error}")),
                        None => (Level::Normal, format!("Resumed session: {id}")),
                    });
                } else {
                    app.current_model = Some(current_model);
                    app.think_level = think_level;
                    app.default_think_level = default_think_level;
                }
            }
            Err(error) => {
                let error = format!("{error:#}");
                startup_notice = Some((
                    Level::Error,
                    format!(
                        "{error}\nConfiguration unavailable; model tasks are disabled. Fix the config files and restart pp. /quit still exits cleanly."
                    ),
                ));
                app.unavailable = Some(error);
            }
        }
        if !diagnostics.is_empty() {
            merge_startup_notice(
                &mut startup_notice,
                format!(
                    "Skill scan issues: {}. Use /skills for details.",
                    diagnostics.len()
                ),
            );
        }
        if !app.file_commands.diagnostics.is_empty() {
            merge_startup_notice(
                &mut startup_notice,
                format!(
                    "Command scan issues: {}. Use /help for details.",
                    app.file_commands.diagnostics.len()
                ),
            );
        }
        if let Some((level, message)) = startup_notice {
            app.notify(level, message);
        }
        Ok(app)
    }

    fn notify(&mut self, level: Level, message: String) {
        self.notifications.show(level, message, Instant::now());
    }

    fn append(&mut self, label: &str, text: &str) {
        self.show_welcome = false;
        self.conversation.append(label, text);
    }

    /// 启动恢复时初始化模型字段，再接管可见对话。
    fn adopt_session(
        &mut self,
        replay: Vec<crate::agent::persistence::ReplayRecord>,
        choices: Vec<ModelChoice>,
        current_model: ModelChoice,
        think_level: ThinkLevel,
        default_think_level: ThinkLevel,
    ) -> Option<String> {
        self.choices = choices;
        self.current_model = Some(current_model);
        self.think_level = think_level;
        self.default_think_level = default_think_level;
        self.adopt_history(replay)
    }

    fn adopt_history(
        &mut self,
        replay: Vec<crate::agent::persistence::ReplayRecord>,
    ) -> Option<String> {
        self.close_ask_on_exit();
        self.token_speed.clear();
        self.history_browser = false;
        self.conversation = self.conversation.fresh();
        self.input_history = Vec::new();
        let error = self.conversation.replay(replay, &mut self.input_history);
        self.input = editor();
        self.shell_mode = false;
        self.history_index = None;
        self.history_draft = None;
        self.model_picker_draft = None;
        self.model_panel = None;
        self.file_paths.clear();
        self.file_directory.clear();
        self.file_query = None;
        self.menu = None;
        self.menu_dismissed = true;
        self.show_welcome = false;
        self.refresh_candidates(false);
        error
    }

    fn fail_worker(&mut self) {
        self.close_ask_on_exit();
        if self.token_speed.is_active() {
            self.token_speed.clear();
        }
        let message = "Execution unavailable: agent worker channel closed; restart pp.";
        self.state = State::Unavailable;
        self.queue_paused = true;
        let _ = self.input_queue.close();
        self.active_cancellation = None;
        self.secondary_status = None;
        self.commands = None;
        self.events = None;
        self.unavailable = Some(message.into());
        self.tree_panel = None;
        self.changes_panel = None;
        self.plan_list_panel = None;
        self.plan_list_pending = None;
        self.plan_reenter_standby = false;
        self.plan_panel = None;
        self.plan_after_enter = None;
        self.review_transcript = None;
        self.plan_review_pending = None;
        self.escape_tap.reset();
        // worker 已死时不再等待恢复回复，会话面板不能继续吞掉按键。
        self.session_panel = None;
        self.close_model_panel();
        self.conversation.fail();
        self.notify(Level::Error, message.into());
    }

    /// 应用 worker 发布的 workflow 快照；仅在正文变化时重算进度。
    fn set_workflow(&mut self, workflow: WorkflowState) {
        if let Some(document) = &workflow.plan {
            self.update_plan_progress_cache(document);
        }
        self.workflow = workflow;
        if let (Some(panel), Some(document)) = (&mut self.plan_panel, &self.workflow.plan) {
            panel.sync_document(document);
        }
    }

    /// 进度 (completed, total) 只随正文变化重算；所有调用点都在把
    /// `document` 写入 `self.workflow.plan` 之前，因此直接与当前绑定比较。
    fn update_plan_progress_cache(&mut self, document: &crate::plan::PlanDocument) {
        if self
            .workflow
            .plan
            .as_ref()
            .is_none_or(|plan| plan.content != document.content)
        {
            self.plan_progress = crate::plan::task_progress(&document.content);
        }
    }

    /// 执行期进度事件：更新匹配计划的快照与只读面板，不追加 Transcript。
    /// 正文 revision/content 变化重算进度；旧事件缺省的名称/位置/状态
    /// 字段保留接收端当前值，归档等纯状态变化在正文相同时也立即生效。
    fn apply_plan_progress(
        &mut self,
        plan_id: &str,
        revision: u64,
        content: String,
        file_name: Option<String>,
        location: Option<crate::plan::PlanLocation>,
        status: Option<crate::plan::PlanStatus>,
    ) {
        let Some(document) = self.workflow.plan.as_ref() else {
            return;
        };
        if document.id != plan_id {
            return;
        }
        let body_changed = document.revision != revision || document.content != content;
        let state_changed = file_name
            .as_ref()
            .is_some_and(|file_name| Some(file_name) != document.file_name.as_ref())
            || location.is_some_and(|location| location != document.location)
            || status.is_some_and(|status| status != document.status);
        if !body_changed && !state_changed {
            return;
        }
        let mut document = self.workflow.plan.take().expect("plan checked above");
        document.revision = revision;
        document.content = content;
        if let Some(file_name) = file_name {
            document.file_name = Some(file_name);
        }
        if let Some(location) = location {
            document.location = location;
        }
        if let Some(status) = status {
            document.status = status;
        }
        self.update_plan_progress_cache(&document);
        if let Some(panel) = &mut self.plan_panel {
            panel.set_progress(&document);
        }
        self.workflow.plan = Some(document);
    }

    fn open_plan_panel(&mut self, review: PlanReview) {
        // ReadPlan 的登记结果是权威快照；同步 workflow 中的文档与报告字段。
        let PlanReview {
            token: _,
            document,
            report,
        } = &review;
        self.update_plan_progress_cache(document);
        self.workflow.plan = Some(document.clone());
        self.workflow.review = report.clone();
        match &mut self.plan_panel {
            Some(panel) => panel.set_review(review),
            None => self.plan_panel = Some(plan::Panel::review(review)),
        }
        self.menu = None;
        self.menu_dismissed = true;
        self.file_query = None;
    }

    /// Execute 运行中的只读进度视图：使用 UI 已缓存的最后同步正文，不发 worker 命令。
    fn open_plan_progress_view(&mut self) {
        let Some(document) = self.workflow.plan.clone() else {
            self.notify(Level::Normal, "No saved plan.".into());
            return;
        };
        if document.content.is_empty() {
            self.notify(Level::Normal, "No saved plan.".into());
            return;
        }
        match &mut self.plan_panel {
            Some(panel) if panel.document().id == document.id => panel.set_progress(&document),
            _ => self.plan_panel = Some(plan::Panel::progress(document)),
        }
    }

    fn request_plan_read(&mut self) {
        if !matches!(self.state, State::Ready) || self.changes_panel.is_some() {
            return;
        }
        self.send(WorkerCommand::ReadPlan, State::ReadingPlan);
    }

    fn append_execution_echo(&mut self) {
        let text = match &self.workflow.plan {
            Some(plan) => {
                let path = plan.path().unwrap_or_else(|| "Not saved yet".to_owned());
                format!("Execute the approved plan {path} (r{}).", plan.revision)
            }
            None => "Execute the approved plan.".to_owned(),
        };
        self.append("You", &text);
    }

    /// 模式切换/审批仅允许 Ready、无 pending、无其它 modal；忙碌明确报错，不排队。
    fn plan_transition_ready(&mut self, pending_message: &str) -> bool {
        if !matches!(self.state, State::Ready) {
            self.notify(
                Level::Error,
                "Cancel the current task before changing Plan mode.".into(),
            );
            return false;
        }
        if self.has_pending() {
            self.notify(Level::Error, pending_message.into());
            return false;
        }
        if self.ask_panel.is_some()
            || self.skill_panel.is_some()
            || self.skills_pending
            || self.session_panel.is_some()
            || self.tree_panel.is_some()
            || self.changes_panel.is_some()
            || self.plan_list_panel.is_some()
            || self.model_panel.is_some()
            || self.queue_menu
        {
            self.notify(
                Level::Error,
                "Close the open panel before changing Plan mode.".into(),
            );
            return false;
        }
        true
    }

    fn start_plan_transition(
        &mut self,
        command: impl FnOnce(CancellationToken) -> WorkerCommand,
        state: State,
    ) {
        let cancellation = CancellationToken::new();
        if self.send(command(cancellation.clone()), state) {
            self.active_cancellation = Some(cancellation);
        }
    }

    fn apply_plan_command(&mut self, command: PlanCommand<'_>) {
        const PENDING_MODE: &str = "Apply pending input before changing Plan mode.";
        match command {
            // 裸 /plan 切换：Execute 进入 Plan 待命；已在 Plan 则退出并解除绑定。
            PlanCommand::Toggle => {
                if !self.plan_transition_ready(PENDING_MODE) {
                    return;
                }
                if self.workflow.mode == WorkMode::Execute || self.plan_retry_enter {
                    // Execute 进入 Plan，或上一次进入失败（MCP 清理残留）时重试进入。
                    self.plan_after_enter = Some(PlanEnterAction::Standby);
                    self.start_plan_transition(
                        |cancellation| WorkerCommand::EnterPlan { cancellation },
                        State::ChangingPlan,
                    );
                    return;
                }
                // revision-0 且已提交任务（origin 非空）的草稿还没有可保存的
                // 正文：退出会静默丢失工作，必须先 plan_write 保存。
                if let Some(plan) = &self.workflow.plan
                    && plan.revision == 0
                    && !plan.origin_prompt.trim().is_empty()
                {
                    self.notify(
                        Level::Error,
                        "Save the draft with plan_write before leaving Plan mode.".into(),
                    );
                    return;
                }
                self.start_plan_transition(
                    |cancellation| WorkerCommand::LeavePlan { cancellation },
                    State::ChangingPlan,
                );
            }
            PlanCommand::Prompt(prompt) => {
                if !self.plan_transition_ready(PENDING_MODE) {
                    return;
                }
                self.plan_after_enter = Some(PlanEnterAction::Prompt(prompt.to_owned()));
                self.start_plan_transition(
                    |cancellation| WorkerCommand::EnterPlan { cancellation },
                    State::ChangingPlan,
                );
            }
            PlanCommand::View => {
                if matches!(self.state, State::Running | State::Cancelling) {
                    // 运行中特许：只读进度视图，不向执行中的 worker 发阻塞读取。
                    self.open_plan_progress_view();
                    return;
                }
                if !self.plan_transition_ready(PENDING_MODE) {
                    return;
                }
                if self.has_saved_plan() {
                    self.request_plan_read();
                } else {
                    self.notify(Level::Normal, "No saved plan.".into());
                }
            }
        }
    }

    /// 审批/审查共用的前置条件：Ready、无 pending、文档 Ready 且持 token，
    /// 并开启输入队列；成功返回 token 与本次操作的取消令牌。
    fn plan_action_guard(
        &mut self,
        action: &str,
        ready_only: bool,
    ) -> Option<(ReviewToken, CancellationToken)> {
        if !matches!(self.state, State::Ready) {
            self.notify(
                Level::Error,
                format!("Cancel the current task before {action} the plan."),
            );
            return None;
        }
        if self.has_pending() {
            self.notify(
                Level::Error,
                format!("Apply pending input before {action} the plan."),
            );
            return None;
        }
        let token = {
            let panel = self.plan_panel.as_ref()?;
            if ready_only && panel.document().status != PlanStatus::Ready {
                return None;
            }
            panel.token().cloned()?
        };
        if let Err(error) = self.input_queue.begin_run() {
            self.queue_paused = true;
            self.notify(Level::Error, error.to_string());
            return None;
        }
        Some((token, CancellationToken::new()))
    }

    /// 审批动作：仅在 Ready、无 pending、文档 Ready 时发送；失败经 ReadPlan 安全刷新。
    fn approve_plan(&mut self, context: ExecutionContext) {
        let Some((token, cancellation)) = self.plan_action_guard("executing", true) else {
            return;
        };
        if self.send(
            WorkerCommand::ApprovePlan {
                token,
                context,
                cancellation: cancellation.clone(),
                input: self.input_queue.clone(),
            },
            State::PreparingExecution,
        ) {
            self.active_cancellation = Some(cancellation);
            self.secondary_status = Some(
                if context == ExecutionContext::Compact {
                    "Compacting context before execution..."
                } else {
                    "Preparing execution..."
                }
                .into(),
            );
        } else {
            let _ = self.input_queue.close();
        }
    }

    fn save_plan_and_leave(&mut self) {
        let Some((token, cancellation)) = self.plan_action_guard("saving", false) else {
            return;
        };
        if self.send(
            WorkerCommand::SavePlanAndLeave {
                token,
                cancellation: cancellation.clone(),
                input: self.input_queue.clone(),
            },
            State::ChangingPlan,
        ) {
            self.active_cancellation = Some(cancellation);
        } else {
            let _ = self.input_queue.close();
        }
    }

    /// 第 4 项：独立上下文 reviewer。仅 Ready、无 pending、文档 Ready 时发送；
    /// 进度只进独立 review transcript；完成或取消后经 ReadPlan 安全刷新回菜单。
    fn review_plan(&mut self) {
        let Some((token, cancellation)) = self.plan_action_guard("reviewing", true) else {
            return;
        };
        if self.send(
            WorkerCommand::ReviewPlan {
                token,
                cancellation: cancellation.clone(),
                input: self.input_queue.clone(),
            },
            State::ReviewingPlan,
        ) {
            self.active_cancellation = Some(cancellation);
            // 独立审查页替代审阅面板；主草稿、历史与会话保持不变。
            self.plan_panel = None;
            self.plan_list_panel = None;
            self.plan_list_pending = None;
            self.menu_dismissed = true;
            self.secondary_status = Some("Reviewing plan...".into());
            let mut review = self.conversation.fresh();
            review.begin_run();
            self.review_transcript = Some(review);
        } else {
            let _ = self.input_queue.close();
        }
    }

    fn review_transcript_tick(&mut self, now: Instant) -> bool {
        self.review_transcript
            .as_mut()
            .is_some_and(|review| review.tick(now))
    }

    /// 已登记至少一个修订的计划：/plan 打开审阅的前提条件。
    fn has_saved_plan(&self) -> bool {
        self.workflow
            .plan
            .as_ref()
            .is_some_and(|plan| plan.revision >= 1)
    }

    /// Plan/Ready：待审阅的计划会暂停自动派发，需要显式审阅或恢复队列后继续。
    fn plan_awaiting_review(&self) -> bool {
        self.workflow.mode == WorkMode::Plan
            && self
                .workflow
                .plan
                .as_ref()
                .is_some_and(|plan| plan.status == PlanStatus::Ready)
    }

    /// 当前显示的转录：独立审查页优先于主对话。
    fn needs_alternate(&self) -> bool {
        self.history_browser
            || self.skill_panel.is_some()
            || self.session_panel.is_some()
            || self.changes_panel.is_some()
            || self.tree_panel.is_some()
            || self.plan_list_panel.is_some()
            || self.plan_panel.is_some()
            || self.review_transcript.is_some()
            || self.ask_panel.is_some()
            || self.menu.is_some()
            || self.model_panel.is_some()
    }

    fn active_transcript(&mut self) -> &mut Transcript {
        self.review_transcript
            .as_mut()
            .unwrap_or(&mut self.conversation)
    }

    fn receive(&mut self) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.skill_worker.replies.try_recv() {
            self.skills_pending = false;
            match reply.result {
                Ok(snapshot) if !reply.saved => {
                    if reply.preview
                        && let Some(panel) = &mut self.skill_panel
                    {
                        panel.apply_preview(snapshot);
                    } else {
                        self.skill_panel = Some(skills::Panel::new(snapshot));
                        if reply.open_sources
                            && let Some(panel) = &mut self.skill_panel
                        {
                            panel.show_sources();
                        }
                    }
                    self.notifications.clear();
                }
                Ok(_) => {
                    self.skill_panel = None;
                    self.menu = None;
                    self.menu_dismissed = true;
                }
                Err(error) => {
                    if let Some(panel) = &mut self.skill_panel {
                        panel.busy = false;
                    }
                    self.notify(Level::Error, format!("{error:#}"));
                }
            }
            changed = true;
        }
        if self.files_pending {
            match self.file_worker.replies.try_recv() {
                Ok(result) => {
                    self.files_pending = false;
                    self.file_paths.clear();
                    match result {
                        Ok(paths) => self.file_paths = paths,
                        Err(error) if self.file_query.is_some() => self.notify(
                            Level::Error,
                            format!("Failed to list project files: {error:#}"),
                        ),
                        Err(_) => {}
                    }
                    // File actions index this snapshot, never the previous list.
                    if self.file_query.is_some() {
                        self.menu = None;
                    }
                    changed = true;
                }
                Err(TryRecvError::Disconnected) => {
                    self.files_pending = false;
                    self.file_paths.clear();
                    self.notify(Level::Error, "File picker unavailable; restart pp.".into());
                    changed = true;
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        for _ in 0..256 {
            let Some(events) = &self.events else {
                break;
            };
            match events.try_recv() {
                Ok(WorkerEvent::Ask { request, reply }) => {
                    if matches!(self.state, State::Running)
                        && self
                            .active_cancellation
                            .as_ref()
                            .is_some_and(|token| !token.is_cancelled())
                        && self.ask_panel.is_none()
                        && self.conversation.has_running_ask(&request.call_id)
                    {
                        self.menu = None;
                        self.menu_dismissed = true;
                        self.queue_menu = false;
                        // 进度视图不能挡住 ask 的键盘路由；先让位给 ask 面板。
                        self.plan_panel = None;
                        self.ask_panel = Some(ask::Panel::new(request));
                        self.ask_reply = Some(reply);
                        self.conversation.waiting_for_input();
                    }
                    changed = true;
                }
                Ok(WorkerEvent::Progress(event)) => {
                    // 用量快照由 App 缓存，供状态栏渲染；不进入 Transcript。
                    match event {
                        AgentEvent::ContextUpdated { usage } => self.usage = Some(usage),
                        AgentEvent::ModelRequestStarted => self.token_speed.start(Instant::now()),
                        AgentEvent::ModelOutputProgress { output_bytes } => {
                            self.token_speed.progress(output_bytes, Instant::now())
                        }
                        AgentEvent::ModelRequestFinished {
                            elapsed_ms,
                            usage,
                            success,
                        } => {
                            self.token_speed.finish(
                                elapsed_ms,
                                usage.map(|usage| usage.output_tokens),
                                success,
                            );
                            if success && let Some(snapshot) = &mut self.usage {
                                snapshot.last_request = usage;
                            }
                        }
                        event @ AgentEvent::CompactionFinished { .. } => {
                            self.token_speed.clear();
                            self.conversation.progress(event);
                        }
                        // 请求重试只显示脱敏原因与等待时长，不进入 Transcript。
                        AgentEvent::RequestRetrying {
                            reason,
                            attempt,
                            wait_ms,
                        } => {
                            self.notify(
                                Level::Important,
                                format!(
                                    "Retrying in {} (attempt {attempt}; {reason})",
                                    format_wait(wait_ms)
                                ),
                            );
                        }
                        // 输入确认由 App 直接回显；不进 Transcript progress，不重复记录历史。
                        AgentEvent::UserInputAccepted { text, .. } => {
                            self.append("You", &text);
                        }
                        // 执行期计划进度：只更新缓存与只读面板，不追加整篇正文到 Transcript。
                        AgentEvent::PlanProgressUpdated {
                            plan_id,
                            revision,
                            content,
                            file_name,
                            location,
                            status,
                        } => self.apply_plan_progress(
                            &plan_id, revision, content, file_name, location, status,
                        ),
                        event => self.conversation.progress(event),
                    }
                    changed = true;
                }
                Ok(WorkerEvent::SessionStarted(id)) => {
                    self.notify(Level::Normal, format!("Session: {id}"));
                    self.session_id = Some(id);
                    changed = true;
                }
                Ok(WorkerEvent::ShellCompleted(result)) => {
                    self.active_cancellation = None;
                    self.secondary_status = None;
                    self.state = State::Ready;
                    if let Err(error) = result {
                        if error
                            .chain()
                            .any(|cause| cause.is::<crate::cancellation::CleanupError>())
                        {
                            self.commands = None;
                            self.events = None;
                            self.state = State::Unavailable;
                            self.unavailable = Some(
                                "Execution unavailable: shell cleanup failed; restart pp.".into(),
                            );
                        }
                        self.notify(Level::Error, format!("{error:#}"));
                    }
                    changed = true;
                }
                Ok(WorkerEvent::Completed {
                    result,
                    elapsed,
                    finished_at,
                }) => {
                    if self.token_speed.is_active() {
                        self.token_speed.clear();
                    }
                    self.ask_reply = None;
                    self.ask_panel = None;
                    self.active_cancellation = None;
                    let succeeded = matches!(&result, Ok(RunOutcome::Completed));
                    if !succeeded {
                        self.queue_paused = self.has_pending();
                    }
                    let awaiting_review = matches!(&result, Ok(RunOutcome::AwaitingPlanReview));
                    let label = match result {
                        Ok(RunOutcome::Completed) => "Completed",
                        Ok(RunOutcome::AwaitingPlanReview) => "Plan ready",
                        Ok(RunOutcome::Cancelled) => {
                            self.conversation.cancel();
                            "Cancelled"
                        }
                        Err(error) => {
                            self.conversation.fail();
                            self.notify(Level::Error, format!("{error:#}"));
                            "Failed"
                        }
                    };
                    self.conversation.finish_run(label, elapsed, finished_at);
                    self.state = State::Ready;
                    if awaiting_review {
                        // 模型提交后自动打开审阅；worker 此时已空闲，安全读取磁盘。
                        self.request_plan_read();
                    }
                    changed = true;
                }
                Ok(WorkerEvent::Compacted(result)) => {
                    // 压缩不是普通任务：不收尾 Transcript、不显示任务 Completed。
                    self.active_cancellation = None;
                    self.secondary_status = None;
                    self.state = State::Ready;
                    match result {
                        Ok(CompactionOutcome::Compacted { .. }) => {}
                        Ok(CompactionOutcome::Unchanged) => {
                            self.notify(Level::Normal, "Nothing to compact.".into());
                        }
                        Ok(CompactionOutcome::Cancelled) => {
                            self.notify(Level::Normal, "Context compaction cancelled.".into());
                        }
                        Err(error) => self.notify(Level::Error, format!("{error:#}")),
                    }
                    changed = true;
                }
                Ok(WorkerEvent::Reset) => {
                    self.close_ask_on_exit();
                    self.history_browser = false;
                    self.conversation = self.conversation.fresh();
                    self.clear_input();
                    self.notifications.clear();
                    self.show_welcome = true;
                    self.state = State::Ready;
                    self.session_id = None;
                    // /new 清掉计划绑定资格；旧计划文件保留，面板不再指向它。
                    self.plan_panel = None;
                    // 新快照紧随其后；先丢弃旧会话用量，避免短暂显示旧占比。
                    self.usage = None;
                    self.token_speed.clear();
                    changed = true;
                }
                Ok(WorkerEvent::ModelSwitched(result)) => {
                    match result {
                        Ok((choice, level)) => {
                            self.current_model = Some(choice);
                            self.think_level = level;
                            // 容量随模型变化；等待紧随的新快照，不显示旧分母。
                            self.usage = None;
                            self.token_speed.clear();
                        }
                        Err(error) => self.notify(Level::Error, format!("{error:#}")),
                    }
                    self.state = State::Ready;
                    changed = true;
                }
                Ok(WorkerEvent::ThinkLevelSet(result)) => {
                    match result {
                        Ok(level) => {
                            self.think_level = level;
                            self.usage = None;
                            self.token_speed.clear();
                        }
                        Err(error) => self.notify(Level::Error, format!("{error:#}")),
                    }
                    self.state = State::Ready;
                    changed = true;
                }
                Ok(WorkerEvent::DefaultThinkLevelSaved(result)) => {
                    match result {
                        Ok(level) => {
                            self.default_think_level = level;
                            self.notify(
                                Level::Normal,
                                format!(
                                    "Default thinking effort saved: {} (takes effect next launch)",
                                    level.label()
                                ),
                            );
                        }
                        Err(error) => self.notify(
                            Level::Error,
                            format!("Failed to save default thinking effort: {error:#}"),
                        ),
                    }
                    self.state = State::Ready;
                    changed = true;
                }
                Ok(WorkerEvent::FavoriteModelsSaved(result)) => {
                    match result {
                        Ok(favorites) => self.favorite_models = favorites,
                        Err(error) => self.notify(
                            Level::Error,
                            format!("Failed to save favorite models: {error:#}"),
                        ),
                    }
                    self.state = State::Ready;
                    changed = true;
                }
                Ok(WorkerEvent::DefaultModelSaved(result)) => {
                    match result {
                        Ok(reference) => {
                            self.notify(
                                Level::Normal,
                                format!(
                                    "Default model saved: {reference} (takes effect next launch)"
                                ),
                            );
                            self.default_model_reference = Some(reference);
                        }
                        Err(error) => self.notify(
                            Level::Error,
                            format!("Failed to save default model: {error:#}"),
                        ),
                    }
                    self.state = State::Ready;
                    changed = true;
                }
                Ok(WorkerEvent::SessionsListed(result)) => {
                    // 扫描结束，worker 空闲；若面板已取消则只丢弃迟到的列表。
                    self.state = State::Ready;
                    match result {
                        Ok(listing) => {
                            let skipped = listing.diagnostics.len();
                            if let Some(panel) = &mut self.session_panel
                                && panel.is_loading()
                            {
                                panel.set_listing(listing);
                                if skipped > 0 {
                                    self.notify(
                                        Level::Error,
                                        format!(
                                            "Skipped {skipped} unreadable saved session file(s)."
                                        ),
                                    );
                                }
                            }
                        }
                        Err(error) => {
                            self.session_panel = None;
                            self.notify(
                                Level::Error,
                                format!("Failed to list saved sessions: {error:#}"),
                            );
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::ChangesRead(view)) => {
                    self.state = State::Ready;
                    if let Some(panel) = &mut self.changes_panel {
                        panel.set_view(view);
                    }
                    changed = true;
                }
                Ok(WorkerEvent::FileUndone(result)) => {
                    match result {
                        Ok(result) => {
                            self.state = State::Ready;
                            if let Some(panel) = &mut self.changes_panel {
                                panel.set_view(result.view);
                            }
                            if let Some(warning) = result.warning {
                                self.notify(Level::Error, warning);
                            }
                        }
                        Err(error) => {
                            // The worker appends an authoritative ChangesRead in FIFO order.
                            // Stay busy until that snapshot arrives; set_view keeps this error.
                            if let Some(panel) = &mut self.changes_panel {
                                panel.set_error(format!("{error:#}"));
                            }
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::TreeRead(result)) => {
                    self.state = State::Ready;
                    match result {
                        Ok(view) if view.tree.nodes.is_empty() => {
                            self.tree_panel = None;
                            self.notify(Level::Normal, "No entries found in session tree.".into());
                        }
                        Ok(view) => {
                            if let Some(panel) = &mut self.tree_panel {
                                panel.set_tree(view);
                            }
                        }
                        Err(error) => {
                            self.tree_panel = None;
                            self.notify(
                                Level::Error,
                                format!("Failed to read session tree: {error:#}"),
                            );
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::TreeNavigated(result)) => {
                    self.state = State::Ready;
                    match result {
                        Ok(navigation) => {
                            let draft = std::mem::replace(&mut self.input, editor());
                            let empty = draft.lines().iter().all(|line| line.trim().is_empty());
                            let error = self.adopt_history(navigation.replay);
                            self.input = draft;
                            if empty && let Some(text) = navigation.draft {
                                self.input.insert_str(text);
                            }
                            self.tree_panel = None;
                            self.usage = None;
                            self.token_speed.clear();
                            if let Some(error) = error {
                                self.notify(Level::Error, error);
                            }
                        }
                        Err(error) => {
                            if let Some(panel) = &mut self.tree_panel {
                                panel.pending = false;
                            }
                            self.notify(
                                Level::Error,
                                format!("Failed to navigate session tree: {error:#}"),
                            );
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::SessionDeleted(id, result)) => {
                    self.state = State::Ready;
                    if let Some(panel) = &mut self.session_panel {
                        panel.delete_finished(&id, result.is_ok());
                    }
                    if let Err(error) = result {
                        self.notify(Level::Error, format!("Failed to delete session: {error:#}"));
                    }
                    changed = true;
                }
                Ok(WorkerEvent::SessionResumed(result)) => {
                    self.state = State::Ready;
                    match result {
                        Ok(state) => {
                            self.session_id = Some(state.id.clone());
                            self.session_panel = None;
                            // 历史/清理句柄已由 ExecutionPrepared 接管；这里只补通知。
                            self.usage = None;
                            self.token_speed.clear();
                            match self.resume_note.take() {
                                Some(error) => self.notify(
                                    Level::Error,
                                    format!("Resumed session: {}\n{error}", state.id),
                                ),
                                None => self.notify(
                                    Level::Normal,
                                    format!("Resumed session: {}", state.id),
                                ),
                            }
                        }
                        Err(error) => {
                            // 恢复失败：保留列表、选中项与旧对话，可重新选择或取消。
                            if let Some(panel) = &mut self.session_panel {
                                panel.restore_failed();
                            }
                            self.notify(
                                Level::Error,
                                format!("Failed to resume session: {error:#}"),
                            );
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::PlansListed(result)) => {
                    self.state = State::Ready;
                    if let Some(panel) = &mut self.plan_list_panel {
                        match result {
                            Ok(listing) => panel.set_listing(listing),
                            Err(error) => panel.set_error(format!("{error:#}")),
                        }
                    }
                    // 面板在加载中关闭：迟到的列表回复直接丢弃。
                    changed = true;
                }
                Ok(WorkerEvent::ListedPlanRead(result)) => {
                    self.state = State::Ready;
                    let pending = self.plan_list_pending.take();
                    match (result, pending) {
                        (Ok(selection), Some(plan_list::PendingOp::Preview))
                            if self.plan_list_panel.is_some() =>
                        {
                            // 复位面板阶段：预览关闭后列表必须继续接收按键。
                            if let Some(panel) = &mut self.plan_list_panel {
                                panel.set_phase(plan_list::Phase::Ready);
                            }
                            if plan_location_from_path(&selection.path) == Some(PlanLocation::Done)
                            {
                                self.open_plan_list_preview(&selection);
                            } else {
                                self.open_listed_plan_selection(selection, PlanOpenIntent::Execute);
                            }
                        }
                        (Ok(selection), Some(plan_list::PendingOp::Open(intent)))
                            if self.plan_list_panel.is_some() =>
                        {
                            self.open_listed_plan_selection(selection, intent);
                        }
                        (Ok(selection), Some(plan_list::PendingOp::Rename(file_name)))
                            if self.plan_list_panel.is_some() =>
                        {
                            if self.send(
                                WorkerCommand::RenameListedPlan {
                                    selection,
                                    file_name,
                                },
                                State::RenamingPlan,
                            ) && let Some(panel) = &mut self.plan_list_panel
                            {
                                panel.set_phase(plan_list::Phase::Renaming);
                            }
                        }
                        (Ok(selection), Some(plan_list::PendingOp::Delete))
                            if self.plan_list_panel.is_some() =>
                        {
                            if self.send(
                                WorkerCommand::DeleteListedPlan { selection },
                                State::DeletingPlan,
                            ) && let Some(panel) = &mut self.plan_list_panel
                            {
                                panel.set_phase(plan_list::Phase::Deleting);
                            }
                        }
                        // 面板在读取中关闭：丢弃迟到的 selection，不继续任何操作。
                        (Ok(_), _) => {}
                        (Err(error), _) => {
                            if let Some(panel) = &mut self.plan_list_panel {
                                panel.set_error(format!("{error:#}"));
                            }
                            self.notify(Level::Error, format!("{error:#}"));
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::ListedPlanOpened(result)) => {
                    self.state = State::Ready;
                    let execute_review = matches!(
                        self.plan_list_pending.take(),
                        Some(plan_list::PendingOp::Open(PlanOpenIntent::Execute))
                    );
                    match result {
                        Ok(preparation) => {
                            // 与 ExecutionPrepared 相同的安装顺序：先清理句柄与
                            // 会话状态，再展示；不自动请求模型。
                            let path = preparation
                                .workflow
                                .plan
                                .as_ref()
                                .and_then(|plan| plan.path())
                                .unwrap_or_else(|| "Not saved yet".to_owned());
                            self.mcp_cleanup = Some(preparation.cleanup);
                            self.plan_panel = None;
                            self.set_workflow(preparation.workflow);
                            let mut note = None;
                            if let Some(records) = preparation.replay {
                                note = self.adopt_history(records);
                            }
                            // 与 ExecutionPrepared 相同顺序：采纳会话状态后渲染
                            // 新候选的 Preview 快照，不被 replay 清理吞掉。
                            if let Some(snapshot) = preparation.todo_snapshot {
                                self.conversation.progress(snapshot);
                            }
                            self.session_id = preparation.session_id;
                            self.usage = None;
                            self.token_speed.clear();
                            if execute_review {
                                // 执行前审阅：重新安全读取并签发 token；
                                // 列表保持打开，以便关闭菜单时返回列表。
                                // 先复位面板阶段，否则 Opening 会吞掉后续按键。
                                if let Some(panel) = &mut self.plan_list_panel {
                                    panel.set_phase(plan_list::Phase::Ready);
                                }
                                self.request_plan_read();
                            } else {
                                // 继续规划：关闭列表，提示继续输入。
                                self.plan_list_panel = None;
                                self.plan_list_pending = None;
                                if let Some(error) = note {
                                    self.notify(Level::Error, error);
                                }
                                self.notify(
                                    Level::Normal,
                                    format!("Opened plan draft {path}; describe the next planning input."),
                                );
                            }
                        }
                        Err(error) => {
                            if let Some(panel) = &mut self.plan_list_panel {
                                panel.set_error(format!("{error:#}"));
                            }
                            self.notify(Level::Error, format!("{error:#}"));
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::ListedPlanRenamed(result)) => {
                    self.state = State::Ready;
                    if let Some(panel) = &mut self.plan_list_panel {
                        match result {
                            Ok(listing) => panel.set_listing(listing),
                            Err(error) => panel.set_error(format!("{error:#}")),
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::ListedPlanDeleted(result)) => {
                    self.state = State::Ready;
                    if let Some(panel) = &mut self.plan_list_panel {
                        match result {
                            Ok(listing) => panel.set_listing(listing),
                            Err(error) => panel.set_error(format!("{error:#}")),
                        }
                    }
                    // 删除当前绑定只解除绑定：worker 清空 plan/review 后同步 UI。
                    if self.workflow.plan.is_none() {
                        self.plan_panel = None;
                        self.plan_progress = (0, 0);
                    }
                    changed = true;
                }
                Ok(WorkerEvent::WorkflowChanged(result)) => {
                    let from_approval = matches!(self.state, State::PreparingExecution);
                    let from_enter = matches!(self.state, State::ChangingPlan);
                    if from_enter || from_approval {
                        let _ = self.input_queue.close();
                        self.active_cancellation = None;
                        self.secondary_status = None;
                    }
                    let after_enter = self.plan_after_enter.take();
                    // 进入转换会携带 after_enter；退出转换保持 None，
                    // 据此区分进入失败（重试进入）与退出失败（保持退出方向）。
                    let was_enter = after_enter.is_some();
                    self.state = State::Ready;
                    match result {
                        Ok(workflow) => {
                            self.plan_retry_enter = false;
                            self.set_workflow(workflow);
                            // `n` 在 Plan 模式下解除绑定后的重进：离开成功立即
                            // 重新进入 Plan 待命，不发送模型请求。
                            let reentered = self.plan_reenter_standby
                                && self.workflow.mode == WorkMode::Execute;
                            if reentered {
                                self.plan_reenter_standby = false;
                                self.plan_after_enter = Some(PlanEnterAction::Standby);
                                self.start_plan_transition(
                                    |cancellation| WorkerCommand::EnterPlan { cancellation },
                                    State::ChangingPlan,
                                );
                            }
                            if !reentered {
                                match after_enter {
                                    // 进入成功只提示待命；首条用户任务才会建立绑定。
                                    Some(PlanEnterAction::Standby) => self.notify(
                                        Level::Normal,
                                        "Plan mode is active. Describe the task to research and plan."
                                            .into(),
                                    ),
                                    Some(PlanEnterAction::Prompt(prompt)) => {
                                        self.queue_paused = false;
                                        if self.send_task(Some(prompt.clone())) {
                                            self.append("You", &prompt);
                                        }
                                    }
                                    None => {}
                                }
                            }
                        }
                        Err(error) => {
                            self.plan_reenter_standby = false;
                            self.notify(Level::Error, format!("{error:#}"));
                            if let Some(PlanEnterAction::Prompt(prompt)) = after_enter
                                && self.input.is_empty()
                            {
                                self.input.insert_str(format!("/plan {prompt}"));
                                self.refresh_candidates(true);
                            }
                            // 失败也可能已持久化 Plan（随后的 Workflow 快照纠正显示）；
                            // 仅进入失败时下一次 /plan 优先重试关闭 MCP；退出失败
                            // 保持离开方向，不把重试误导成再次进入。
                            if from_enter && was_enter {
                                self.plan_retry_enter = true;
                            }
                            if from_approval || (from_enter && self.plan_panel.is_some()) {
                                // 审批或保存退出失败均安全刷新正文与新 token。
                                self.request_plan_read();
                            }
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::PlanRead(result)) => {
                    self.state = State::Ready;
                    match result {
                        Ok(review) => {
                            let reviewed = self.plan_review_pending.take().is_some();
                            self.open_plan_panel(review);
                            // 审查完成后的刷新：焦点落在正文 Review 标签；
                            // 外部修订则面板标注旧版报告。绝不自动启动执行。
                            if reviewed && let Some(panel) = &mut self.plan_panel {
                                panel.focus_review_body();
                            }
                        }
                        Err(error) => {
                            if let Some(report) = self.plan_review_pending.take() {
                                // 报告已生成但安全读取失败：展示最终报告与错误；
                                // 无 token 的只读面板禁用旧执行 token，直到再次安全读取成功。
                                if let Some(document) = self.workflow.plan.clone() {
                                    self.plan_panel = Some(plan::Panel::degraded(document, report));
                                }
                            }
                            self.notify(Level::Error, format!("{error:#}"));
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::ReviewProgress(event)) => {
                    // 审查进度只进独立 review transcript：不更新用量/主对话/派发。
                    if let Some(review) = &mut self.review_transcript {
                        review.progress(event);
                    }
                    changed = true;
                }
                Ok(WorkerEvent::PlanReviewed(result)) => {
                    // 审查终态：无论成败都保持 Plan/Ready 并回六项菜单；
                    // 报告只在成功时替换，失败/取消保留上次完整报告。
                    self.state = State::Ready;
                    self.active_cancellation = None;
                    self.secondary_status = None;
                    self.review_transcript = None;
                    let _ = self.input_queue.close();
                    match result {
                        Ok(report) => {
                            // 报告先入 UI 快照，随后 ReadPlan 安全刷新正文/token/报告绑定。
                            self.workflow.review = Some(report.clone());
                            self.plan_review_pending = Some(report);
                            self.request_plan_read();
                        }
                        Err(error) => {
                            self.notify(Level::Error, format!("{error:#}"));
                            self.request_plan_read();
                        }
                    }
                    changed = true;
                }
                Ok(WorkerEvent::Workflow(workflow)) => {
                    self.set_workflow(workflow);
                    changed = true;
                }
                Ok(WorkerEvent::Cleanup(cleanup)) => {
                    // 错误路径上 registry 可能被保留/部分更新；刷新退出清理句柄。
                    self.mcp_cleanup = Some(cleanup);
                    changed = true;
                }
                Ok(WorkerEvent::ExecutionPrepared {
                    workflow,
                    session_id,
                    replay,
                    cleanup,
                    todo_snapshot,
                    reply,
                }) => {
                    // 一次性交接：先安装退出清理句柄并更新模式/会话，再 ACK；
                    // ACK 后 worker 才允许 continue_session。
                    self.mcp_cleanup = Some(cleanup);
                    self.plan_panel = None;
                    self.set_workflow(workflow);
                    let mut note = None;
                    if let Some(records) = replay {
                        // None：Keep/off 不重绘；Some(空)：Fresh 新会话；Some(records)：恢复。
                        note = self.adopt_history(records);
                    }
                    // 快照在 adopt_history/替换 workflow 之后、ACK 之前渲染，
                    // 保证 Fresh 清理后的新 transcript 立即携带交接卡片。
                    if let Some(snapshot) = todo_snapshot {
                        self.conversation.progress(snapshot);
                    }
                    self.session_id = session_id;
                    self.usage = None;
                    self.token_speed.clear();
                    let confirmed = reply.send(()).is_ok();
                    match self.state {
                        State::PreparingExecution if confirmed => {
                            self.secondary_status = None;
                            self.state = State::Running;
                            self.conversation.begin_run();
                            self.append_execution_echo();
                            // 列表审批进入执行：关闭列表与待处理操作。
                            self.plan_list_panel = None;
                            self.plan_list_pending = None;
                        }
                        State::Resuming => {
                            self.resume_note = note;
                            self.state = State::Ready;
                        }
                        State::ChangingPlan => {
                            let _ = self.input_queue.close();
                            self.active_cancellation = None;
                            self.secondary_status = None;
                            if self.workflow.mode == WorkMode::Execute {
                                self.plan_list_panel = None;
                                self.plan_list_pending = None;
                            }
                            self.state = State::Ready;
                        }
                        _ => {
                            self.state = State::Ready;
                        }
                    }
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.fail_worker();
                    return true;
                }
            }
        }
        changed |= self.refresh_queue();
        changed |= self.dispatch_next(false);
        if changed {
            self.escape_tap.reset();
            self.refresh_candidates(false);
        }
        changed
    }

    fn refresh_candidates(&mut self, edited: bool) {
        if let Some(panel) = &mut self.model_panel {
            panel.refresh(
                &self.choices,
                self.current_model
                    .as_ref()
                    .map(|choice| choice.reference.as_str()),
                self.default_model_reference.as_deref(),
                &self.favorite_models,
                edited,
            );
            return;
        }
        if self.queue_menu {
            self.refresh_pending_menu();
            return;
        }
        if self.shell_mode {
            self.file_query = None;
            self.menu = None;
            return;
        }
        if edited {
            self.menu_dismissed = false;
        }
        if self.skill_panel.is_some()
            || self.skills_pending
            || self.session_panel.is_some()
            || self.tree_panel.is_some()
            || self.changes_panel.is_some()
            || self.plan_list_panel.is_some()
            || self.menu_dismissed
            || !matches!(
                self.state,
                State::Ready | State::Unavailable | State::SavingModelSettings
            )
        {
            self.file_query = None;
            self.menu = None;
            return;
        }
        // Only slash menus require a single-line draft.
        let input = if self.input.lines().len() == 1 {
            self.input.lines()[0].trim_start()
        } else {
            ""
        };
        let argument = input.split_once(char::is_whitespace);
        if let Some(("/model", query)) = argument
            && matches!(self.state, State::Ready)
        {
            let query = query.to_owned();
            self.open_model_panel(&query, false);
            return;
        }
        let mut preferred = 0;
        let items = match argument {
            Some(("/effort", argument)) if matches!(self.state, State::Ready) => {
                let thinking = self
                    .current_model
                    .as_ref()
                    .map(|model| model.thinking)
                    .unwrap_or_default();
                let default_level = thinking.effective_default(self.default_think_level);
                let items: Vec<_> = thinking
                    .levels()
                    .filter(|level| level.as_str().starts_with(argument.trim()))
                    .map(|level| MenuItem {
                        value: MenuAction::Effort(level),
                        label: if level == default_level {
                            format!("{} (default)", level.label())
                        } else {
                            level.label().into()
                        },
                        description: match level {
                            ThinkLevel::None => "Disable thinking",
                            ThinkLevel::Low => "Light thinking",
                            ThinkLevel::Medium => "Moderate thinking",
                            ThinkLevel::High => "Deep thinking",
                            ThinkLevel::Xhigh => "Extra-deep thinking",
                            ThinkLevel::Max => "Highest level the model supports",
                        }
                        .into(),
                        icon: None,
                        current: level == self.think_level,
                    })
                    .collect();
                preferred = items.iter().position(|item| item.current).unwrap_or(0);
                items
            }
            None if input.trim().starts_with('/') => COMMANDS
                .iter()
                .enumerate()
                .filter(|(_, spec)| spec.name.starts_with(input.trim()))
                .map(|(index, spec)| MenuItem {
                    value: MenuAction::Command(index),
                    label: spec.name.into(),
                    description: spec.description.into(),
                    icon: None,
                    current: false,
                })
                .chain(
                    self.file_commands
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, command)| command.name.starts_with(input.trim()))
                        .map(|(index, command)| MenuItem {
                            value: MenuAction::FileCommand(index),
                            label: command.name.clone(),
                            description: command.description.clone(),
                            icon: None,
                            current: false,
                        }),
                )
                .collect(),
            _ => {
                if self.model_picker_draft.is_some() {
                    self.file_query = None;
                    self.menu = None;
                    return;
                }
                let query = files::query(&self.input);
                if query.is_some() && self.file_query.is_none() {
                    self.file_directory.clear();
                    self.file_paths.clear();
                    self.menu = None;
                    if !self.files_pending {
                        match self.file_worker.requests.send(()) {
                            Ok(()) => self.files_pending = true,
                            Err(_) => self.notify(
                                Level::Error,
                                "File picker unavailable; restart pp.".into(),
                            ),
                        }
                    }
                }
                if self
                    .file_query
                    .as_ref()
                    .zip(query.as_ref())
                    .is_some_and(|(old, new)| old.row != new.row || old.start != new.start)
                {
                    self.file_directory.clear();
                }
                let changed = self.file_query != query;
                self.file_query = query;
                self.refresh_file_menu(changed);
                return;
            }
        };
        self.file_query = None;
        if items.is_empty() {
            self.menu = None;
        } else if edited || self.menu.as_ref().is_none_or(|menu| menu.items() != items) {
            let menu = SelectionMenu::new(items, preferred);
            self.menu = Some(
                if matches!(
                    menu.items().first().map(|item| item.value),
                    Some(MenuAction::Effort(_))
                ) {
                    menu.with_hint(EFFORT_HINT)
                } else {
                    menu
                },
            );
        }
    }

    fn refresh_file_menu(&mut self, changed: bool) {
        let Some(query) = &self.file_query else {
            self.menu = None;
            return;
        };
        if !changed && self.menu.is_some() {
            return;
        }
        let prefix = query
            .prefix
            .strip_prefix(&self.file_directory)
            .unwrap_or(&query.prefix);
        let entries = files::entries(&self.file_paths, &self.file_directory, prefix);
        let mut items =
            Vec::with_capacity(entries.len() + usize::from(!self.file_directory.is_empty()));
        if !self.file_directory.is_empty() {
            items.push(MenuItem {
                value: MenuAction::FileParent,
                label: "../".into(),
                description: format!("Parent of {}", self.file_directory),
                icon: None,
                current: false,
            });
        }
        items.extend(entries.into_iter().map(|entry| MenuItem {
            value: if entry.directory {
                MenuAction::FileDirectory(entry.index)
            } else {
                MenuAction::File(entry.index)
            },
            label: entry.path[self.file_directory.len()..].into(),
            description: entry.path.into(),
            icon: None,
            current: false,
        }));
        if items.is_empty() {
            self.menu = None;
        } else if changed || self.menu.as_ref().is_none_or(|menu| menu.items() != items) {
            self.menu = Some(
                SelectionMenu::new(items, usize::from(!self.file_directory.is_empty()))
                    .with_hint("Enter/Tab open/insert · ../ parent · Esc cancel"),
            );
        }
    }

    fn complete_file_directory(&mut self) {
        files::insert(
            &mut self.input,
            self.file_query.as_ref().expect("checked current query"),
            &self.file_directory,
        );
        self.file_query = files::query(&self.input);
        self.refresh_file_menu(true);
    }

    fn at_input_prefix(&self) -> bool {
        let ratatui_textarea::DataCursor(row, column) = self.input.cursor();
        row == 0
            && !self.input.is_selecting()
            && self.input.lines()[0]
                .chars()
                .take(column)
                .all(char::is_whitespace)
    }

    fn insert_input(&mut self, mut text: String) {
        if !self.shell_mode && self.at_input_prefix() {
            let trimmed = text.trim_start_matches(|ch: char| ch.is_whitespace() && ch != '\n');
            if trimmed.starts_with('!') {
                // 在交给编辑器前消费前缀，撤销与重做也不会重新插入切换键。
                text.remove(text.len() - trimmed.len());
                self.shell_mode = true;
            }
        }
        self.input.insert_str(text);
    }

    fn input_text(&self) -> String {
        let text = self.input.lines().join("\n");
        if self.shell_mode {
            format!("!{text}")
        } else {
            text
        }
    }

    fn clear_input(&mut self) {
        self.input = editor();
        self.shell_mode = false;
        self.history_index = None;
        self.history_draft = None;
        self.refresh_candidates(true);
    }

    fn remember_input(&mut self) {
        let text = self.input_text();
        if !text.trim().is_empty() {
            self.input_history.push(text);
        }
        self.history_index = None;
        self.history_draft = None;
    }

    fn navigate_history(&mut self, previous: bool) {
        let index = match (self.history_index, previous) {
            (None, true) if !self.input_history.is_empty() => {
                self.history_draft = Some((self.input.clone(), self.shell_mode));
                self.input_history.len() - 1
            }
            (Some(0), true) => return,
            (Some(index), true) => index - 1,
            (Some(index), false) if index + 1 < self.input_history.len() => index + 1,
            (Some(_), false) => {
                (self.input, self.shell_mode) = self
                    .history_draft
                    .take()
                    .unwrap_or_else(|| (editor(), false));
                self.history_index = None;
                self.file_query = None;
                self.menu = None;
                self.menu_dismissed = true;
                return;
            }
            _ => return,
        };
        self.file_query = None;
        self.input = editor();
        self.shell_mode = false;
        self.insert_input(self.input_history[index].clone());
        self.history_index = Some(index);
        // 回填命令时先保持菜单关闭，让上下键可以连续浏览历史。
        self.menu = None;
        self.menu_dismissed = true;
    }

    fn send(&mut self, command: WorkerCommand, state: State) -> bool {
        if self
            .commands
            .as_ref()
            .is_some_and(|sender| sender.send(command).is_ok())
        {
            self.escape_tap.reset();
            self.state = state;
            self.refresh_candidates(false);
            true
        } else {
            self.fail_worker();
            false
        }
    }

    fn close_pending_menu(&mut self) {
        self.queue_menu = false;
        self.queue_menu_revision = None;
        self.menu = None;
        self.menu_dismissed = true;
    }

    fn refresh_pending_menu(&mut self) {
        self.refresh_queue();
        let Some(snapshot) = &self.queue_snapshot else {
            return;
        };
        if self.queue_menu_revision == Some(snapshot.revision) && self.menu.is_some() {
            return;
        }
        self.queue_menu_revision = Some(snapshot.revision);
        let selected = self.menu.as_ref().and_then(|menu| {
            menu.selected_index()
                .map(|index| (menu.items()[index].value, index))
        });
        let items: Vec<_> = snapshot
            .items
            .iter()
            .filter(|item| item.kind == PendingKind::Steering)
            .chain(
                snapshot
                    .items
                    .iter()
                    .filter(|item| item.kind == PendingKind::FollowUp),
            )
            .map(|item| MenuItem {
                value: MenuAction::PendingInput(item.id),
                label: format!(
                    "{} #{}",
                    if item.kind == PendingKind::Steering {
                        "Steer"
                    } else {
                        "Queued"
                    },
                    item.id
                ),
                description: transcript_style::display_text(&item.preview).replace('\t', " "),
                icon: None,
                current: false,
            })
            .collect();
        if items.is_empty() {
            self.close_pending_menu();
        } else if self.menu.as_ref().is_none_or(|menu| menu.items() != items) {
            let preferred = selected
                .and_then(|(value, index)| {
                    items
                        .iter()
                        .position(|item| item.value == value)
                        .or(Some(index.min(items.len() - 1)))
                })
                .unwrap_or(0);
            self.menu = Some(
                SelectionMenu::new(items, preferred).with_hint(platform_text(
                    "Enter recall · ⌥ + R resume queue · Esc close",
                    "Enter recall · Ctrl + R resume queue · Esc close",
                )),
            );
        }
    }

    fn toggle_pending_menu(&mut self) {
        if self.queue_menu {
            self.close_pending_menu();
        } else if self.has_pending() {
            self.queue_menu = true;
            self.file_query = None;
            self.menu = None;
            self.refresh_pending_menu();
        } else {
            self.notify(Level::Normal, "No pending messages.".into());
        }
    }

    fn recall_pending(&mut self, id: u64) {
        if !self.input.lines().iter().all(String::is_empty) {
            self.notify(
                Level::Error,
                "Clear the draft before recalling a pending message.".into(),
            );
            return;
        }
        match self.input_queue.recall(id) {
            Ok(Some(message)) => {
                self.close_pending_menu();
                self.clear_input();
                self.input.insert_str(message.source);
                if !self.has_pending() {
                    self.queue_paused = false;
                }
            }
            Ok(None) => {
                self.refresh_pending_menu();
                self.notify(Level::Normal, "Message is already being applied.".into());
            }
            Err(error) => self.notify(Level::Error, error.to_string()),
        }
    }

    fn resume_queue(&mut self) {
        if !matches!(self.state, State::Ready) || !self.has_pending() {
            return;
        }
        // Plan/Ready 暂停了自动派发；恢复队列操作是显式继续，后端会把 Ready 降为 Draft。
        if !self.queue_paused && !self.plan_awaiting_review() {
            return;
        }
        self.close_pending_menu();
        self.queue_paused = false;
        self.dispatch_next(true);
    }

    fn refresh_queue(&mut self) -> bool {
        let revision = self
            .queue_snapshot
            .as_ref()
            .map(|snapshot| snapshot.revision);
        match self.input_queue.snapshot_since(revision) {
            Ok(Some(snapshot)) => {
                self.queue_snapshot = Some(snapshot);
                true
            }
            Ok(None) => false,
            Err(error) => {
                self.queue_paused = true;
                self.notify(Level::Error, error.to_string());
                false
            }
        }
    }

    fn has_pending(&mut self) -> bool {
        self.refresh_queue();
        self.queue_snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.items.is_empty())
    }

    /// 派发下一条待处理消息；返回是否真的派发（状态离开 Ready）。
    /// Plan/Ready 自动派发直接拒绝；显式提交/恢复队列操作仍可派发（后端先降 Draft）。
    fn dispatch_next(&mut self, explicit: bool) -> bool {
        if !matches!(self.state, State::Ready)
            || self.queue_paused
            || self.ask_panel.is_some()
            || self.skill_panel.is_some()
            || self.skills_pending
            || self.session_panel.is_some()
            || self.tree_panel.is_some()
            || self.changes_panel.is_some()
            || self.plan_list_panel.is_some()
            || self.plan_panel.is_some()
            || self.model_panel.is_some()
        {
            return false;
        }
        if !explicit && self.plan_awaiting_review() {
            return false;
        }
        let message = match self.input_queue.claim_next() {
            Ok(Some(message)) => message,
            Ok(None) => return false,
            Err(error) => {
                self.queue_paused = true;
                self.notify(Level::Error, error.to_string());
                return false;
            }
        };
        let dispatched = self.send_task(Some(message.prompt().to_owned()));
        if dispatched {
            self.append("You", &message.into_display());
        } else {
            if let Err(error) = self.input_queue.restore_front([message].into()) {
                self.notify(Level::Error, error.to_string());
            }
            self.queue_paused = true;
        }
        self.refresh_queue();
        !matches!(self.state, State::Ready)
    }

    fn send_task(&mut self, prompt: Option<String>) -> bool {
        if let Err(error) = self.input_queue.begin_run() {
            self.queue_paused = true;
            self.notify(Level::Error, error.to_string());
            return false;
        }
        let cancellation = CancellationToken::new();
        let command = match prompt {
            Some(prompt) => WorkerCommand::Run {
                prompt,
                cancellation: cancellation.clone(),
                input: self.input_queue.clone(),
            },
            None => WorkerCommand::Continue {
                cancellation: cancellation.clone(),
                input: self.input_queue.clone(),
            },
        };
        if self.send(command, State::Running) {
            self.active_cancellation = Some(cancellation);
            self.conversation.begin_run();
            true
        } else {
            let _ = self.input_queue.close();
            false
        }
    }

    fn send_submission(&mut self, command: WorkerCommand, state: State) -> bool {
        if self.send(command, state) {
            self.remember_input();
            true
        } else {
            false
        }
    }

    fn send_shell(&mut self, command: String) {
        let cancellation = CancellationToken::new();
        let call_id = format!("local-shell-{}", self.next_shell_id);
        if self.send_submission(
            WorkerCommand::Shell {
                command,
                call_id,
                cancellation: cancellation.clone(),
            },
            State::RunningShell,
        ) {
            self.next_shell_id += 1;
            self.active_cancellation = Some(cancellation);
            self.clear_input();
            self.show_welcome = false;
            self.history_browser = false;
            self.conversation.follow();
            self.secondary_status = Some("Running shell...".into());
        }
    }

    /// 手动压缩入口：命令本身不进对话、不伪造普通任务；沿用 slash 命令的
    /// 提交语义（成功后清空初始输入），压缩期间新输入的草稿不受影响。
    fn send_compaction(&mut self) {
        let cancellation = CancellationToken::new();
        if self.send_submission(
            WorkerCommand::Compact {
                cancellation: cancellation.clone(),
            },
            State::Compacting,
        ) {
            self.active_cancellation = Some(cancellation);
            self.clear_input();
            self.secondary_status = Some("Compacting context...".into());
        }
    }

    // 有待处理消息时禁止改动会话历史；/tree 与 submit 的 New/Resume/Tree 入口共用。
    fn pending_blocks_session_change(&mut self) -> bool {
        if !self.has_pending() {
            return false;
        }
        self.notify(
            Level::Error,
            "Recall pending messages before changing session history.".into(),
        );
        true
    }

    fn open_tree(&mut self) {
        self.escape_tap.reset();
        if self.pending_blocks_session_change() {
            return;
        }
        if !matches!(self.state, State::Ready)
            || self.skills_pending
            || self.skill_panel.is_some()
            || self.session_panel.is_some()
            || self.ask_panel.is_some()
            || self.tree_panel.is_some()
            || self.changes_panel.is_some()
        {
            return;
        }
        self.menu = None;
        self.file_query = None;
        self.tree_panel = Some(tree::Panel::new());
        self.send(WorkerCommand::ReadTree, State::ReadingTree);
    }

    fn open_changes(&mut self) {
        self.escape_tap.reset();
        if !matches!(self.state, State::Ready)
            || self.skills_pending
            || self.skill_panel.is_some()
            || self.session_panel.is_some()
            || self.ask_panel.is_some()
            || self.tree_panel.is_some()
            || self.changes_panel.is_some()
            || self.plan_panel.is_some()
            || self.model_panel.is_some()
        {
            return;
        }
        self.close_pending_menu();
        self.menu = None;
        self.file_query = None;
        self.changes_panel = Some(changes::Panel::new(self.workflow.mode == WorkMode::Plan));
        self.send(WorkerCommand::ReadChanges, State::ReadingChanges);
    }

    fn apply_changes_action(&mut self, action: changes::Action) {
        match action {
            changes::Action::None => {}
            changes::Action::Close => {
                self.changes_panel = None;
                self.menu_dismissed = true;
            }
            changes::Action::Refresh if matches!(self.state, State::Ready) => {
                self.send(WorkerCommand::ReadChanges, State::ReadingChanges);
            }
            changes::Action::Undo(token) if matches!(self.state, State::Ready) => {
                self.send(WorkerCommand::UndoFile(token), State::UndoingFile);
            }
            changes::Action::Message(message) => {
                if let Some(panel) = &mut self.changes_panel {
                    panel.set_error(message);
                }
            }
            changes::Action::Refresh | changes::Action::Undo(_) => {}
        }
    }

    /// /plan-list 打开前提：Ready、无 pending、无其它 modal；被拒时保留输入。
    fn plan_list_entry_ready(&mut self) -> bool {
        if !matches!(self.state, State::Ready) {
            self.notify(
                Level::Error,
                "Cancel the current task before changing Plan mode.".into(),
            );
            return false;
        }
        if self.has_pending() {
            self.notify(
                Level::Error,
                "Apply pending input before changing Plan mode.".into(),
            );
            return false;
        }
        if self.ask_panel.is_some()
            || self.skill_panel.is_some()
            || self.skills_pending
            || self.session_panel.is_some()
            || self.tree_panel.is_some()
            || self.changes_panel.is_some()
            || self.plan_panel.is_some()
            || self.model_panel.is_some()
            || self.queue_menu
        {
            self.notify(
                Level::Error,
                "Close the open panel before changing Plan mode.".into(),
            );
            return false;
        }
        true
    }

    fn open_plan_list(&mut self) {
        self.escape_tap.reset();
        self.file_query = None;
        self.menu = None;
        self.menu_dismissed = true;
        self.plan_list_pending = None;
        self.plan_list_panel = Some(plan_list::Panel::new(
            self.workflow.plan.as_ref().map(|plan| plan.id.clone()),
        ));
        if !self.send(WorkerCommand::ListPlans, State::ListingPlans) {
            self.plan_list_panel = None;
        }
    }

    fn close_plan_list(&mut self) {
        self.plan_list_panel = None;
        self.plan_list_pending = None;
        self.menu_dismissed = true;
    }

    fn apply_plan_list_action(&mut self, action: plan_list::Action) {
        match action {
            plan_list::Action::None => {}
            plan_list::Action::Close => self.close_plan_list(),
            plan_list::Action::Refresh => {
                if matches!(self.state, State::Ready)
                    && let Some(panel) = &mut self.plan_list_panel
                {
                    panel.set_loading();
                    self.send(WorkerCommand::ListPlans, State::ListingPlans);
                }
            }
            plan_list::Action::Preview(id) => {
                if !matches!(self.state, State::Ready) {
                    return;
                }
                self.plan_list_pending = Some(plan_list::PendingOp::Preview);
                if !self.send(
                    WorkerCommand::ReadListedPlan { id },
                    State::ReadingListedPlan,
                ) {
                    self.plan_list_pending = None;
                } else if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_reading();
                }
            }
            plan_list::Action::Open(id, intent) => {
                if !matches!(self.state, State::Ready) {
                    return;
                }
                self.plan_list_pending = Some(plan_list::PendingOp::Open(intent));
                if !self.send(
                    WorkerCommand::ReadListedPlan { id },
                    State::ReadingListedPlan,
                ) {
                    self.plan_list_pending = None;
                } else if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_reading();
                }
            }
            plan_list::Action::Rename(id, file_name) => {
                if !matches!(self.state, State::Ready) {
                    return;
                }
                self.plan_list_pending = Some(plan_list::PendingOp::Rename(file_name));
                if !self.send(
                    WorkerCommand::ReadListedPlan { id },
                    State::ReadingListedPlan,
                ) {
                    self.plan_list_pending = None;
                } else if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_reading();
                }
            }
            plan_list::Action::Delete(id) => {
                if !matches!(self.state, State::Ready) {
                    return;
                }
                self.plan_list_pending = Some(plan_list::PendingOp::Delete);
                if !self.send(
                    WorkerCommand::ReadListedPlan { id },
                    State::ReadingListedPlan,
                ) {
                    self.plan_list_pending = None;
                } else if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_reading();
                }
            }
            plan_list::Action::NewPlan => self.plan_list_new_plan(),
            plan_list::Action::Message(message) => {
                if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_error(message);
                }
            }
        }
    }

    fn open_listed_plan_selection(&mut self, selection: PlanSelection, intent: PlanOpenIntent) {
        let cancellation = CancellationToken::new();
        if self.send(
            WorkerCommand::OpenListedPlan {
                selection,
                intent,
                cancellation: cancellation.clone(),
            },
            State::OpeningListedPlan,
        ) {
            self.plan_list_pending = Some(plan_list::PendingOp::Open(intent));
            self.active_cancellation = Some(cancellation);
            if let Some(panel) = &mut self.plan_list_panel {
                panel.set_opening();
            }
        }
    }

    /// `n` 新建规划：保留已保存草稿，关闭列表并进入 Plan+None 待命；
    /// 不分配 ID、不写正文、不调模型；未落盘草稿复用裸 /plan 防丢失门禁。
    fn plan_list_new_plan(&mut self) {
        if self.workflow.mode == WorkMode::Plan
            && let Some(plan) = &self.workflow.plan
            && plan.revision == 0
            && !plan.origin_prompt.trim().is_empty()
        {
            self.notify(
                Level::Error,
                "Save the draft with plan_write before leaving Plan mode.".into(),
            );
            return;
        }
        self.close_plan_list();
        if self.workflow.mode == WorkMode::Execute || self.plan_retry_enter {
            self.plan_after_enter = Some(PlanEnterAction::Standby);
            self.start_plan_transition(
                |cancellation| WorkerCommand::EnterPlan { cancellation },
                State::ChangingPlan,
            );
            return;
        }
        // 已在 Plan 模式：先解除当前绑定（LeavePlan→Execute+None），
        // 成功后由 WorkflowChanged 立即重新进入 Plan 待命。
        self.plan_reenter_standby = true;
        self.start_plan_transition(
            |cancellation| WorkerCommand::LeavePlan { cancellation },
            State::ChangingPlan,
        );
    }

    /// Done 只读预览：以 selection 构造展示用文档，不发审批 token。
    fn open_plan_list_preview(&mut self, selection: &PlanSelection) {
        let location = plan_location_from_path(&selection.path).unwrap_or(PlanLocation::Drafts);
        let file_name = selection
            .path
            .rsplit('/')
            .next()
            .unwrap_or("plan.md")
            .to_owned();
        let status = match location {
            PlanLocation::Drafts => PlanStatus::Draft,
            PlanLocation::Plans => PlanStatus::Ready,
            PlanLocation::Done => PlanStatus::Completed,
        };
        let document = crate::plan::PlanDocument {
            id: selection.id.clone(),
            revision: 1,
            title: file_name.trim_end_matches(".md").to_owned(),
            content: selection.content.clone(),
            status,
            origin_prompt: String::new(),
            file_name: Some(file_name),
            location,
            approved_tasks: None,
        };
        self.plan_progress = crate::plan::task_progress(&document.content);
        match &mut self.plan_panel {
            Some(panel) if panel.document().id == document.id => panel.set_progress(&document),
            _ => self.plan_panel = Some(plan::Panel::progress(document)),
        }
        self.menu = None;
        self.menu_dismissed = true;
        self.file_query = None;
    }

    fn show_help(&mut self) {
        self.append("Help", &commands::help(&self.file_commands));
        if !self.file_commands.diagnostics.is_empty() {
            self.append(
                "Help",
                &format!(
                    "Command load issues:\n{}",
                    self.file_commands.diagnostics.join("\n")
                ),
            );
        }
        self.append("Tips", &format!("{STEERING_HINT}{SHORTCUTS_TAIL}"));
        self.append("Help", WARNING);
    }

    fn shortcut_ready(&mut self) -> bool {
        if self.skills_pending || self.changes_panel.is_some() || self.plan_list_panel.is_some() {
            return false;
        }
        match self.state {
            State::Ready => true,
            State::Unavailable => {
                if let Some(error) = self.unavailable.clone() {
                    self.notify(Level::Error, error);
                }
                false
            }
            _ => false,
        }
    }

    fn open_model_picker(&mut self) {
        self.open_model_panel("", true);
    }

    fn open_model_panel(&mut self, query: &str, preserve_draft: bool) {
        if self.model_panel.is_some() || !self.shortcut_ready() {
            return;
        }
        if preserve_draft {
            self.model_picker_draft = Some(ModelPickerDraft {
                input: std::mem::replace(&mut self.input, editor()),
                shell_mode: std::mem::take(&mut self.shell_mode),
                history_index: self.history_index.take(),
                history_draft: self.history_draft.take(),
                menu: self.menu.take(),
                menu_dismissed: self.menu_dismissed,
            });
        } else {
            self.input = editor();
            self.shell_mode = false;
            self.history_index = None;
            self.history_draft = None;
        }
        self.menu = None;
        self.file_query = None;
        self.menu_dismissed = true;
        self.model_panel = Some(model::Panel::new(query, self.choices.len()));
        self.refresh_candidates(true);
    }

    fn close_model_panel(&mut self) {
        if self.model_panel.take().is_none() {
            return;
        }
        self.menu = None;
        self.menu_dismissed = true;
        self.restore_model_picker_draft();
    }

    fn apply_model_action(&mut self, action: model::Action) {
        match action {
            model::Action::Cancel => self.close_model_panel(),
            model::Action::Select(index) if matches!(self.state, State::Ready) => {
                let reference = self.choices[index].reference.clone();
                let current = self
                    .current_model
                    .as_ref()
                    .is_some_and(|choice| choice.reference == reference);
                if !current {
                    self.send(WorkerCommand::SwitchModel(reference), State::SwitchingModel);
                }
                self.close_model_panel();
            }
            model::Action::Favorite(index) if matches!(self.state, State::Ready) => {
                self.send(
                    WorkerCommand::ToggleFavoriteModel(self.choices[index].reference.clone()),
                    State::SavingModelSettings,
                );
            }
            model::Action::Default(index) if matches!(self.state, State::Ready) => {
                self.send(
                    WorkerCommand::SaveDefaultModel(self.choices[index].reference.clone()),
                    State::SavingModelSettings,
                );
            }
            _ => {}
        }
    }

    fn restore_model_picker_draft(&mut self) {
        if let Some(draft) = self.model_picker_draft.take() {
            self.input = draft.input;
            self.shell_mode = draft.shell_mode;
            self.history_index = draft.history_index;
            self.history_draft = draft.history_draft;
            self.menu = draft.menu;
            self.menu_dismissed = draft.menu_dismissed;
            self.refresh_candidates(false);
        }
    }

    fn switch_model(&mut self, reference: String) {
        if self.send_submission(WorkerCommand::SwitchModel(reference), State::SwitchingModel) {
            self.clear_input();
        }
    }

    fn set_think_level(&mut self, level: ThinkLevel) {
        if self.send_submission(
            WorkerCommand::SetThinkLevel(level),
            State::SettingThinkLevel,
        ) {
            self.clear_input();
        }
    }

    // /init 回显用户命令本身；其余展开命令回显展开正文。
    fn echoes_source(prompt: &str) -> bool {
        prompt.trim() == "/init"
    }

    fn submit(&mut self, kind: PendingKind) -> bool {
        if self.skills_pending
            || self.skill_panel.is_some()
            || self.changes_panel.is_some()
            || self.model_panel.is_some()
        {
            return false;
        }
        let prompt = self.input.lines().join("\n");
        if prompt.trim().is_empty() {
            return false;
        }
        let command = if self.shell_mode {
            Some(Command::Shell(prompt.as_str()))
        } else if prompt.trim_start().starts_with('/') {
            match commands::parse(&prompt, &self.file_commands) {
                Ok(command) => command,
                Err(error) => {
                    self.notify(Level::Error, format!("{error:#}"));
                    return false;
                }
            }
        } else {
            None
        };
        if matches!(command, Some(Command::Changes))
            && matches!(self.state, State::Running | State::Cancelling)
        {
            self.notify(
                Level::Error,
                "Finish or cancel the current task before opening /changes.".into(),
            );
            return false;
        }
        if matches!(command, Some(Command::PlanList)) && !matches!(self.state, State::Ready) {
            self.notify(
                Level::Error,
                "Cancel the current task before changing Plan mode.".into(),
            );
            return false;
        }
        if matches!(self.state, State::ReviewingPlan) {
            match command {
                // 退出、帮助与提示不经过 worker；其余提交（含 steering/followup）
                // 一律拒绝，草稿原样保留。
                Some(Command::Quit | Command::Help | Command::Tips) => {}
                _ => {
                    self.notify(
                        Level::Error,
                        "Plan review is running; press Esc to cancel it.".into(),
                    );
                    return false;
                }
            }
        }
        match command {
            Some(Command::Quit) => return true,
            Some(Command::Plan(sub)) => {
                let consumed = matches!(self.state, State::Ready) && !self.has_pending()
                    || sub == PlanCommand::View
                        && matches!(self.state, State::Running | State::Cancelling);
                self.apply_plan_command(sub);
                if consumed {
                    self.remember_input();
                    self.clear_input();
                }
                return false;
            }
            Some(Command::PlanList) => {
                if self.plan_list_entry_ready() {
                    self.remember_input();
                    self.clear_input();
                    self.open_plan_list();
                }
                return false;
            }
            Some(Command::Help) => {
                self.show_help();
                self.remember_input();
                self.clear_input();
                return false;
            }
            Some(Command::Tips) => {
                self.show_welcome = false;
                self.conversation.append_tips();
                self.remember_input();
                self.clear_input();
                return false;
            }
            _ => {}
        }
        if let Some(Command::Shell(command)) = &command {
            if matches!(self.state, State::Unavailable) {
                if let Some(error) = self.unavailable.clone() {
                    self.notify(Level::Error, error);
                }
            } else if self.workflow.mode != crate::plan::WorkMode::Execute {
                self.notify(
                    Level::Error,
                    "Shell commands are unavailable in Plan mode.".into(),
                );
            } else if !matches!(self.state, State::Ready) {
                self.notify(
                    Level::Error,
                    "Finish or cancel the current task before running a shell command.".into(),
                );
            } else if self.has_pending() {
                self.notify(
                    Level::Error,
                    "Resolve pending messages before running a shell command.".into(),
                );
            } else {
                self.send_shell((*command).to_owned());
            }
            return false;
        }
        if let Some(Command::Skills(selection)) = command {
            if !matches!(self.state, State::Ready | State::Unavailable) {
                return false;
            }
            let request = match selection {
                commands::SkillCommand::List => skills::Request::Open,
                commands::SkillCommand::Sources => skills::Request::Sources,
                commands::SkillCommand::Toggle(name, enabled) => {
                    skills::Request::Toggle(name.to_owned(), enabled)
                }
                commands::SkillCommand::Source(source, enabled) => {
                    skills::Request::Source(source, enabled)
                }
            };
            self.request_skills(request);
            return false;
        }
        if matches!(
            &command,
            Some(Command::New | Command::Resume | Command::Tree)
        ) && self.pending_blocks_session_change()
        {
            return false;
        }
        let pending = self.has_pending();
        let model_message = matches!(&command, None | Some(Command::Prompt(_)));
        if model_message
            && (matches!(self.state, State::Running)
                || (matches!(self.state, State::Ready) && pending))
        {
            let expansion = match command {
                Some(Command::Prompt(expanded)) => Some(expanded),
                _ => None,
            };
            let echo_source = Self::echoes_source(&prompt);
            match self
                .input_queue
                .enqueue(kind, prompt, expansion, echo_source)
            {
                Ok(_) => {
                    self.remember_input();
                    self.clear_input();
                    self.dispatch_next(true);
                }
                Err(error) => self.notify(Level::Error, error.to_string()),
            }
            return false;
        }
        match self.state {
            State::Running
            | State::RunningShell
            | State::Cancelling
            | State::Compacting
            | State::Resetting
            | State::SwitchingModel
            | State::SavingModelSettings
            | State::SettingThinkLevel
            | State::ListingSessions
            | State::Resuming
            | State::ReadingTree
            | State::NavigatingTree
            | State::ReadingChanges
            | State::UndoingFile
            | State::ListingPlans
            | State::ReadingListedPlan
            | State::OpeningListedPlan
            | State::RenamingPlan
            | State::DeletingPlan
            | State::ChangingPlan
            | State::ReadingPlan
            | State::PreparingExecution
            | State::ReviewingPlan => {}
            State::Unavailable => {
                if let Some(error) = self.unavailable.take() {
                    self.notify(Level::Error, error.clone());
                    self.unavailable = Some(error);
                }
            }
            State::Ready => match command {
                Some(Command::Shell(_)) => unreachable!("handled before the state match"),
                Some(Command::Think(Some(level))) => {
                    self.set_think_level(level);
                }
                Some(Command::Think(None)) => {
                    self.input = editor();
                    self.input.insert_str("/effort ");
                    self.refresh_candidates(true);
                }
                Some(Command::New) => {
                    self.send_submission(WorkerCommand::Reset, State::Resetting);
                }
                Some(Command::Compact) => self.send_compaction(),
                Some(Command::Tree) => {
                    self.remember_input();
                    self.clear_input();
                    self.open_tree();
                }
                Some(Command::Changes) => {
                    self.remember_input();
                    self.clear_input();
                    self.open_changes();
                }
                Some(Command::Resume) => {
                    if self.skill_panel.is_some() || self.skills_pending {
                        return false;
                    }
                    // 记入输入历史并清空输入；保留当前 Transcript 与欢迎页状态。
                    self.remember_input();
                    self.clear_input();
                    self.file_directory.clear();
                    self.file_paths.clear();
                    self.file_query = None;
                    self.menu = None;
                    self.session_panel = Some(sessions::Panel::new(
                        self.session_id.clone(),
                        std::env::current_dir().unwrap_or_default(),
                    ));
                    if !self.send(WorkerCommand::ListSessions, State::ListingSessions) {
                        self.session_panel = None;
                    }
                }
                Some(Command::Model(Some(reference))) => self.switch_model(reference.to_owned()),
                Some(Command::Model(None)) => self.open_model_panel("", false),
                None => {
                    self.queue_paused = false;
                    if self.send_task(Some(prompt.clone())) {
                        self.remember_input();
                        self.append("You", &prompt);
                        self.clear_input();
                    }
                }
                Some(Command::Prompt(expanded)) => {
                    self.queue_paused = false;
                    // /init 回显用户命令；文件命令继续回显展开正文，输入历史均保留原文。
                    if self.send_task(Some(expanded.clone())) {
                        self.remember_input();
                        self.append(
                            "You",
                            if Self::echoes_source(&prompt) {
                                &prompt
                            } else {
                                &expanded
                            },
                        );
                        self.clear_input();
                    }
                }
                Some(Command::Help | Command::Tips | Command::Quit | Command::Skills(_)) => {
                    unreachable!()
                }
                // Plan 子命令在状态检查前已处理。
                Some(Command::Plan(_)) => unreachable!("handled before the state match"),
                Some(Command::PlanList) => unreachable!("handled before the state match"),
            },
        }
        false
    }

    fn request_skills(&mut self, request: skills::Request) {
        match self.skill_worker.requests.send(request) {
            Ok(()) => {
                self.skills_pending = true;
                self.file_query = None;
                self.menu = None;
                if let Some(panel) = &mut self.skill_panel {
                    panel.busy = true;
                }
            }
            Err(_) => self.notify(Level::Error, "Skill management worker unavailable.".into()),
        }
    }

    fn close_session_panel(&mut self) {
        self.session_panel = None;
        self.menu_dismissed = true;
        // 加载中取消时扫描仍占用 worker：保持 ListingSessions，
        // 迟到的列表回复发现面板已关闭即丢弃，状态随之恢复 Ready。
    }

    fn apply_menu_action(&mut self, action: MenuAction, confirm: bool) -> bool {
        if confirm && matches!(action, MenuAction::Model(_)) && !matches!(self.state, State::Ready)
        {
            return false;
        }
        if matches!(
            action,
            MenuAction::File(_) | MenuAction::FileDirectory(_) | MenuAction::FileParent
        ) && (self.file_query.is_none() || files::query(&self.input) != self.file_query)
        {
            self.refresh_candidates(false);
            return false;
        }
        match action {
            MenuAction::PendingInput(id) => {
                if confirm {
                    self.recall_pending(id);
                }
            }
            MenuAction::FileDirectory(index) => {
                if let Some(path) = self.file_paths.get(index)
                    && let Some((directory, true)) = files::child(path, &self.file_directory)
                {
                    self.file_directory = directory.to_owned();
                }
                self.complete_file_directory();
            }
            MenuAction::FileParent => {
                let parent_end = self
                    .file_directory
                    .trim_end_matches(std::path::MAIN_SEPARATOR)
                    .rfind(std::path::MAIN_SEPARATOR)
                    .map_or(0, |end| end + 1);
                self.file_directory.truncate(parent_end);
                self.complete_file_directory();
            }
            MenuAction::File(index) => {
                let Some(path) = self.file_paths.get(index) else {
                    self.refresh_file_menu(true);
                    return false;
                };
                files::insert(
                    &mut self.input,
                    self.file_query.as_ref().expect("checked current query"),
                    path,
                );
                self.menu = None;
                self.file_query = None;
                self.menu_dismissed = true;
            }
            MenuAction::Command(index) => {
                self.input = editor();
                self.input.insert_str(COMMANDS[index].name);
                self.refresh_candidates(true);
                return confirm && self.submit(PendingKind::Steering);
            }
            MenuAction::FileCommand(index) => {
                self.input = editor();
                self.input
                    .insert_str(self.file_commands.entries[index].name.as_str());
                self.refresh_candidates(true);
                return confirm && self.submit(PendingKind::Steering);
            }
            MenuAction::Model(index) if confirm => {
                self.apply_model_action(model::Action::Select(index))
            }
            MenuAction::Effort(level) if confirm => self.set_think_level(level),
            MenuAction::Model(index) => {
                let reference = self.choices[index].reference.clone();
                self.open_model_panel(&reference, false);
            }
            MenuAction::Effort(level) => {
                self.input = editor();
                self.input.insert_str(format!("/effort {}", level.as_str()));
                self.refresh_candidates(true);
            }
        }
        false
    }

    fn shutdown_background_tasks(&mut self) -> Result<()> {
        self.changes_panel = None;
        self.plan_list_panel = None;
        self.plan_list_pending = None;
        self.plan_reenter_standby = false;
        if let Some(token) = &self.active_cancellation {
            token.cancel();
        }
        self.close_ask_on_exit();
        if let Some(cleanup) = &self.background_cleanup {
            cleanup.shutdown()?;
        }
        Ok(())
    }

    fn close_ask_on_exit(&mut self) {
        if self.ask_panel.is_some() || self.ask_reply.is_some() {
            if let Some(token) = &self.active_cancellation {
                token.cancel();
            }
            self.ask_reply = None;
            self.ask_panel = None;
        }
    }

    fn apply_ask_action(&mut self, action: ask::Action) {
        match action {
            ask::Action::None => {}
            ask::Action::Cancel => {
                self.close_ask_on_exit();
                self.state = State::Cancelling;
                self.conversation.cancelling();
            }
            ask::Action::Submit(answers) => {
                self.ask_panel = None;
                if let Some(reply) = self.ask_reply.take()
                    && reply.send(answers).is_err()
                {
                    if let Some(token) = &self.active_cancellation {
                        token.cancel();
                    }
                    self.state = State::Cancelling;
                    self.conversation.cancelling();
                }
            }
        }
    }

    fn handle(&mut self, event: Event, dirty: &mut bool) -> bool {
        if !matches!(&event, Event::Key(key) if key.code == KeyCode::Esc
            && key.modifiers.is_empty() && key.kind == KeyEventKind::Press)
        {
            self.escape_tap.reset();
        }
        if let Event::Mouse(mouse) = event {
            let (changed, action) = self.notifications.mouse(mouse, Instant::now());
            *dirty |= changed;
            match action {
                MouseAction::Consumed => return false,
                MouseAction::Ignored => {}
            }
        }
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                if control && key.code == KeyCode::Char('c') {
                    return key.kind == KeyEventKind::Press;
                }
                *dirty = true;
                if let Some(panel) = &mut self.changes_panel {
                    let action = panel.key(key);
                    self.escape_tap.reset();
                    self.apply_changes_action(action);
                    return false;
                }
                if let Some(panel) = &mut self.tree_panel {
                    let action = panel.key(key);
                    self.escape_tap.reset();
                    match action {
                        tree::Action::None => {}
                        tree::Action::Cancel => {
                            self.tree_panel = None;
                            self.menu_dismissed = true;
                        }
                        tree::Action::Navigate(id) => {
                            self.send(WorkerCommand::NavigateTree(id), State::NavigatingTree);
                        }
                    }
                    return false;
                }
                if self.plan_panel.is_some()
                    && matches!(self.state, State::PreparingExecution | State::ChangingPlan)
                {
                    if key.code == KeyCode::Esc
                        && key.modifiers.is_empty()
                        && let Some(cancellation) = &self.active_cancellation
                    {
                        cancellation.cancel();
                        self.secondary_status = Some("Cancelling plan transition...".into());
                    }
                    return false;
                }
                if let Some(panel) = &mut self.plan_panel {
                    self.escape_tap.reset();
                    match panel.key(key) {
                        plan::Action::None => {}
                        plan::Action::Close => {
                            // Esc 仅关闭面板：保留模式、正文与 pending 暂停。
                            self.plan_panel = None;
                            self.menu_dismissed = true;
                            // 列表只读预览可能改写过进度缓存；恢复当前绑定计划。
                            if let Some(plan) = &self.workflow.plan {
                                self.plan_progress = crate::plan::task_progress(&plan.content);
                            }
                        }
                        plan::Action::Approve(context) => self.approve_plan(context),
                        plan::Action::Discuss(text) => {
                            // 讨论经普通 Plan run 送回原上下文；主草稿与光标保留。
                            self.plan_panel = None;
                            self.menu_dismissed = true;
                            self.queue_paused = false;
                            if self.send_task(Some(text.clone())) {
                                self.plan_list_panel = None;
                                self.plan_list_pending = None;
                                self.append("You", &text);
                                self.input_history.push(text);
                                self.history_index = None;
                                self.history_draft = None;
                            }
                        }
                        plan::Action::Review => self.review_plan(),
                        plan::Action::SaveAndLeave => self.save_plan_and_leave(),
                        plan::Action::EditInput(key) => {
                            if key.code == KeyCode::Char('j')
                                && key.modifiers == KeyModifiers::CONTROL
                            {
                                self.input.insert_newline();
                            } else {
                                self.input.input(key);
                            }
                        }
                        plan::Action::DiscussInput => {
                            let text = self.input.lines().join("\n");
                            if !text.trim().is_empty() && matches!(self.state, State::Ready) {
                                let text = panel.compose_discussion(text);
                                self.queue_paused = false;
                                if self.send_task(Some(text.clone())) {
                                    self.plan_panel = None;
                                    self.plan_list_panel = None;
                                    self.plan_list_pending = None;
                                    self.input = editor();
                                    self.append("You", &text);
                                    self.input_history.push(text);
                                    self.history_index = None;
                                    self.history_draft = None;
                                }
                            }
                        }
                    }
                    return false;
                }
                if let Some(panel) = &mut self.plan_list_panel {
                    self.escape_tap.reset();
                    let action = panel.key(key);
                    self.apply_plan_list_action(action);
                    return false;
                }
                if self.ask_panel.is_some()
                    || self.skill_panel.is_some()
                    || self.session_panel.is_some()
                    || self.menu.is_some()
                    || self.model_panel.is_some()
                    || self.file_query.is_some()
                {
                    self.escape_tap.reset();
                }
                // 显示切换先于普通菜单和 ask，但不穿透全屏面板。
                if key.code == KeyCode::Char('o')
                    && key.modifiers == KeyModifiers::CONTROL
                    && self.skill_panel.is_none()
                    && self.session_panel.is_none()
                {
                    if key.kind == KeyEventKind::Press {
                        if self.ask_panel.is_none() && self.review_transcript.is_none() {
                            if !self.history_browser {
                                self.conversation.begin_browse();
                            }
                            self.history_browser = true;
                        }
                        self.active_transcript().toggle_tool_output();
                        let enabled = self.active_transcript().tools_expanded();
                        self.notify(
                            Level::Normal,
                            format!(
                                "Tool output expansion: {}",
                                if enabled { "enabled" } else { "disabled" }
                            ),
                        );
                    }
                    return false;
                }
                if let Some(panel) = &mut self.ask_panel {
                    let action = panel.key(key);
                    self.apply_ask_action(action);
                    return false;
                }
                if let Some(panel) = &mut self.skill_panel {
                    match panel.key(key) {
                        skills::Action::None => {}
                        skills::Action::Cancel => {
                            self.skill_panel = None;
                            self.menu_dismissed = true;
                        }
                        skills::Action::Save(selection) => {
                            self.request_skills(skills::Request::Save(selection))
                        }
                        skills::Action::Reload => self.request_skills(skills::Request::Reload),
                        skills::Action::Preview(selection) => {
                            self.request_skills(skills::Request::Preview(selection))
                        }
                    }
                    return false;
                }
                // 会话面板打开时优先消费键盘输入，编辑器、历史翻阅与快捷操作不穿透。
                if let Some(panel) = &mut self.session_panel {
                    match panel.key(key) {
                        sessions::Action::None => {}
                        sessions::Action::Cancel => self.close_session_panel(),
                        sessions::Action::Resume(index) => {
                            let id = panel
                                .id(index)
                                .expect("selection indexes the session list")
                                .to_owned();
                            if self.session_id.as_deref() == Some(id.as_str()) {
                                // 当前会话：只关闭列表，不重锁自己的 journal。
                                self.close_session_panel();
                            } else {
                                panel.restore_started();
                                let cancellation = CancellationToken::new();
                                if !self.send(
                                    WorkerCommand::Resume {
                                        id,
                                        cancellation: cancellation.clone(),
                                    },
                                    State::Resuming,
                                ) {
                                    self.session_panel = None;
                                } else {
                                    self.active_cancellation = Some(cancellation);
                                }
                            }
                        }
                        sessions::Action::Delete(index) => {
                            let id = panel
                                .id(index)
                                .expect("selection indexes the session list")
                                .to_owned();
                            if !self.send(WorkerCommand::DeleteSession(id), State::ListingSessions)
                            {
                                self.session_panel = None;
                            }
                        }
                    }
                    return false;
                }
                if let Some(panel) = &mut self.model_panel {
                    let (action, edited) = panel.key(key, &self.choices);
                    self.apply_model_action(action);
                    if self.model_panel.is_some() {
                        self.refresh_candidates(edited);
                    }
                    return false;
                }
                if key.modifiers == FUNCTION_MODIFIER
                    && matches!(key.code, KeyCode::Char('q' | 'r'))
                {
                    if key.kind == KeyEventKind::Press
                        && self.model_picker_draft.is_none()
                        && !self.skills_pending
                    {
                        if key.code == KeyCode::Char('r') {
                            self.resume_queue();
                        } else if matches!(
                            self.state,
                            State::Running | State::Ready | State::Cancelling | State::Unavailable
                        ) {
                            self.toggle_pending_menu();
                        }
                    }
                    return false;
                }
                if self.queue_menu {
                    if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
                        return false;
                    }
                    if key.code == KeyCode::Enter && !key.modifiers.is_empty() {
                        return false;
                    }
                    if let Some(menu) = &mut self.menu {
                        match menu.handle_key(key) {
                            MenuEvent::Cancel => {
                                self.close_pending_menu();
                                return false;
                            }
                            MenuEvent::Confirm(MenuAction::PendingInput(id)) => {
                                self.recall_pending(id);
                                return false;
                            }
                            MenuEvent::Handled | MenuEvent::Complete(_) | MenuEvent::Confirm(_) => {
                                return false;
                            }
                            MenuEvent::Ignored => {}
                        }
                    }
                    // Normal editing closes the list before following the existing editor path.
                    self.close_pending_menu();
                }
                if key.code == KeyCode::Enter
                    && !matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::ALT)
                {
                    return false;
                }
                if key.code == KeyCode::Char('s')
                    && (key.modifiers == KeyModifiers::CONTROL
                        || key.modifiers == FUNCTION_MODIFIER)
                {
                    if key.kind == KeyEventKind::Press
                        && let Some(action) = self.menu.as_ref().and_then(|menu| {
                            menu.selected_index().map(|index| menu.items()[index].value)
                        })
                        && self.shortcut_ready()
                    {
                        let request = match action {
                            MenuAction::Effort(level) if key.modifiers == FUNCTION_MODIFIER => {
                                Some((
                                    WorkerCommand::SaveDefaultThinkLevel(level),
                                    State::SettingThinkLevel,
                                ))
                            }
                            MenuAction::Model(index) if key.modifiers == KeyModifiers::CONTROL => {
                                Some((
                                    WorkerCommand::SaveDefaultModel(
                                        self.choices[index].reference.clone(),
                                    ),
                                    State::SavingModelSettings,
                                ))
                            }
                            _ => None,
                        };
                        if let Some((command, state)) = request
                            && self.send(command, state)
                            && matches!(action, MenuAction::Effort(_))
                        {
                            self.clear_input();
                        }
                    }
                    return false;
                }
                let shift_tab = (key.code == KeyCode::BackTab
                    && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT))
                    || (key.code == KeyCode::Tab && key.modifiers == KeyModifiers::SHIFT);
                if shift_tab {
                    if key.kind == KeyEventKind::Press
                        && self.menu.is_none()
                        && self.model_picker_draft.is_none()
                        && self.shortcut_ready()
                    {
                        let thinking = self
                            .current_model
                            .as_ref()
                            .map(|model| model.thinking)
                            .unwrap_or_default();
                        let next = thinking.next(self.think_level);
                        if next != self.think_level {
                            self.send(WorkerCommand::SetThinkLevel(next), State::SettingThinkLevel);
                        }
                    }
                    return false;
                }
                if key.code == KeyCode::Char('p') && key.modifiers == FUNCTION_MODIFIER {
                    if key.kind == KeyEventKind::Press {
                        self.open_model_picker();
                    }
                    return false;
                }
                if key.code == KeyCode::F(1) && key.modifiers.is_empty() {
                    if key.kind == KeyEventKind::Press {
                        self.show_help();
                    }
                    return false;
                }
                if key.code == KeyCode::Esc && self.file_query.is_some() {
                    self.file_query = None;
                    self.menu = None;
                    self.menu_dismissed = true;
                    return false;
                }
                if self.files_pending
                    && self.file_query.is_some()
                    && matches!(key.code, KeyCode::Enter | KeyCode::Tab)
                {
                    return false;
                }
                if control && key.code == KeyCode::Char('j') {
                    self.input.insert_newline();
                    self.refresh_candidates(true);
                    return false;
                }
                if control && !key.modifiers.contains(KeyModifiers::ALT) {
                    let ratatui_textarea::DataCursor(row, column) = self.input.cursor();
                    match key.code {
                        KeyCode::Char('a') => {
                            if column > 0 {
                                self.input.move_cursor(CursorMove::Head);
                            }
                            self.refresh_candidates(false);
                            return false;
                        }
                        KeyCode::Char('e') => {
                            let end = self.input.lines()[row].chars().count();
                            if column < end {
                                self.input.move_cursor(CursorMove::End);
                            }
                            self.refresh_candidates(false);
                            return false;
                        }
                        KeyCode::Char('k') => {
                            if column < self.input.lines()[row].chars().count() {
                                self.input.cancel_selection();
                                self.input.delete_line_by_end();
                                if self.input.lines().iter().all(String::is_empty) {
                                    self.shell_mode = false;
                                }
                                self.refresh_candidates(true);
                            }
                            return false;
                        }
                        _ => {}
                    }
                }
                if let Some(menu) = &mut self.menu {
                    match menu.handle_key(key) {
                        MenuEvent::Ignored => {}
                        MenuEvent::Handled => return false,
                        MenuEvent::Cancel => {
                            self.menu = None;
                            self.menu_dismissed = true;
                            return false;
                        }
                        MenuEvent::Confirm(action) => return self.apply_menu_action(action, true),
                        MenuEvent::Complete(action) => {
                            return self.apply_menu_action(action, false);
                        }
                    }
                }
                if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                    if self.history_browser {
                        self.history_browser = false;
                        self.conversation.follow();
                        self.escape_tap.reset();
                        return false;
                    }
                    if self.shell_mode
                        && !matches!(
                            self.state,
                            State::RunningShell
                                | State::Running
                                | State::Compacting
                                | State::ReviewingPlan
                        )
                    {
                        self.shell_mode = false;
                        self.menu_dismissed = true;
                        self.escape_tap.reset();
                        return false;
                    }
                    if key.kind == KeyEventKind::Press && matches!(self.state, State::RunningShell)
                    {
                        if let Some(cancellation) = &self.active_cancellation {
                            cancellation.cancel();
                            self.secondary_status = Some("Cancelling shell...".into());
                        }
                        self.escape_tap.reset();
                        return false;
                    }
                    if key.kind == KeyEventKind::Press
                        && matches!(
                            self.state,
                            State::Running | State::Compacting | State::ReviewingPlan
                        )
                        && let Some(cancellation) = &self.active_cancellation
                    {
                        cancellation.cancel();
                        self.queue_paused = true;
                        match self.state {
                            State::Compacting => {
                                self.secondary_status = Some("Cancelling context...".into());
                            }
                            State::ReviewingPlan => {
                                // 取消 reviewer；保持 ReviewingPlan，等待 worker
                                // 的 PlanReviewed 受控收尾后回六项菜单。
                                if let Some(review) = &mut self.review_transcript {
                                    review.cancelling();
                                }
                                self.secondary_status = Some("Cancelling review...".into());
                            }
                            _ => self.conversation.cancelling(),
                        }
                        if !matches!(self.state, State::ReviewingPlan) {
                            self.state = State::Cancelling;
                        }
                        self.escape_tap.reset();
                    } else if key.kind == KeyEventKind::Press
                        && matches!(self.state, State::Ready)
                        && !self.skills_pending
                        && self.input.lines().iter().all(String::is_empty)
                    {
                        if self.escape_tap.press(Instant::now()) {
                            self.open_tree();
                        }
                    } else {
                        self.escape_tap.reset();
                    }
                    return false;
                }
                if key.code == KeyCode::Char('!')
                    && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
                    && !self.shell_mode
                    && self.at_input_prefix()
                {
                    self.shell_mode = true;
                    self.refresh_candidates(false);
                    return false;
                }
                if key.code == KeyCode::Backspace
                    && key.modifiers.is_empty()
                    && self.shell_mode
                    && self.input.lines().iter().all(String::is_empty)
                {
                    self.shell_mode = false;
                    self.refresh_candidates(false);
                    return false;
                }
                match key.code {
                    KeyCode::Enter => {
                        if key.kind == KeyEventKind::Press
                            && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::ALT)
                            && self.submit(if key.modifiers == KeyModifiers::ALT {
                                PendingKind::FollowUp
                            } else {
                                PendingKind::Steering
                            })
                        {
                            return true;
                        }
                    }
                    KeyCode::PageUp => {
                        if !self.history_browser {
                            self.active_transcript().begin_browse();
                        }
                        self.history_browser = true;
                        self.active_transcript().page_up();
                    }
                    KeyCode::PageDown => {
                        self.active_transcript().page_down();
                        if self.conversation.following() {
                            self.history_browser = false;
                        }
                    }
                    KeyCode::Up | KeyCode::Down if key.modifiers.is_empty() => {
                        let cursor = self.input.cursor();
                        self.input.input(key);
                        // 先在显示行之间移动，越过首末显示行时才浏览历史。
                        if self.input.cursor() == cursor && self.model_picker_draft.is_none() {
                            self.navigate_history(key.code == KeyCode::Up);
                        } else {
                            self.refresh_candidates(false);
                        }
                    }
                    _ => {
                        let edited = self.input.input(key);
                        if edited && self.input.lines().iter().all(String::is_empty) {
                            self.shell_mode = false;
                        }
                        self.refresh_candidates(edited);
                    }
                }
            }
            Event::Paste(_) if self.changes_panel.is_some() => {
                *dirty = true;
            }
            Event::Mouse(mouse) if self.changes_panel.is_some() => {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    self.changes_panel
                        .as_mut()
                        .expect("active changes panel")
                        .wheel(
                            matches!(mouse.kind, MouseEventKind::ScrollUp),
                            MOUSE_SCROLL_LINES,
                        );
                    *dirty = true;
                }
            }
            Event::Paste(_) if self.tree_panel.is_some() => {
                *dirty = true;
            }
            Event::Mouse(mouse) if self.tree_panel.is_some() => {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    self.tree_panel.as_mut().expect("active tree panel").wheel(
                        matches!(mouse.kind, MouseEventKind::ScrollUp),
                        MOUSE_SCROLL_LINES,
                    );
                    *dirty = true;
                }
            }
            Event::Paste(text) if self.plan_panel.is_some() => {
                if !self
                    .plan_panel
                    .as_mut()
                    .expect("active plan panel")
                    .paste(&text)
                {
                    self.input
                        .insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
                }
                *dirty = true;
            }
            Event::Paste(text) if self.plan_list_panel.is_some() => {
                if let Some(panel) = &mut self.plan_list_panel {
                    panel.paste(&text);
                }
                *dirty = true;
            }
            Event::Mouse(mouse) if self.plan_panel.is_some() => {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                    self.plan_panel
                        .as_mut()
                        .expect("active plan panel")
                        .wheel(up, MOUSE_SCROLL_LINES);
                    *dirty = true;
                }
            }
            Event::Mouse(mouse) if self.plan_list_panel.is_some() => {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                    if let Some(panel) = &mut self.plan_list_panel {
                        panel.wheel(up);
                    }
                    *dirty = true;
                }
            }
            Event::Paste(text) if self.ask_panel.is_some() => {
                let action = self
                    .ask_panel
                    .as_mut()
                    .expect("active ask panel")
                    .paste(&text);
                self.apply_ask_action(action);
                *dirty = true;
            }
            Event::Mouse(mouse) if self.ask_panel.is_some() => {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                    if !self.ask_panel.as_mut().expect("active ask panel").wheel(
                        up,
                        mouse.column,
                        mouse.row,
                    ) {
                        if up {
                            self.conversation.scroll_up(MOUSE_SCROLL_LINES);
                        } else {
                            self.conversation.scroll_down(MOUSE_SCROLL_LINES);
                        }
                    }
                    *dirty = true;
                }
            }
            Event::Paste(_) if self.skill_panel.is_some() => {}
            Event::Mouse(_) if self.skill_panel.is_some() => {}
            Event::Paste(text) if self.session_panel.is_some() => {
                if let Some(panel) = &mut self.session_panel {
                    panel.paste(&text);
                }
                *dirty = true;
            }
            Event::Mouse(mouse) if self.session_panel.is_some() => {
                // 滚轮只改变列表选择，不滚动背后的对话。
                if let MouseEventKind::ScrollUp | MouseEventKind::ScrollDown = mouse.kind {
                    let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                    if let Some(panel) = &mut self.session_panel {
                        panel.wheel(up);
                    }
                    *dirty = true;
                }
            }
            Event::Paste(text) if self.model_panel.is_some() => {
                self.model_panel
                    .as_mut()
                    .expect("active model panel")
                    .paste(&text);
                self.refresh_candidates(true);
                *dirty = true;
            }
            Event::Mouse(mouse)
                if self
                    .model_panel
                    .as_mut()
                    .is_some_and(|panel| panel.wheel(mouse, MOUSE_SCROLL_LINES)) =>
            {
                *dirty = true;
            }
            Event::Paste(text) => {
                if self.queue_menu {
                    self.close_pending_menu();
                }
                self.insert_input(text.replace("\r\n", "\n").replace('\r', "\n"));
                self.refresh_candidates(true);
                *dirty = true;
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    if !self.history_browser {
                        self.active_transcript().begin_browse();
                    }
                    self.history_browser = true;
                    self.active_transcript().scroll_up(MOUSE_SCROLL_LINES);
                    *dirty = true;
                }
                MouseEventKind::ScrollDown => {
                    self.active_transcript().scroll_down(MOUSE_SCROLL_LINES);
                    if self.conversation.following() {
                        self.history_browser = false;
                    }
                    *dirty = true;
                }
                _ => {}
            },
            Event::Resize(_, _) => *dirty = true,
            _ => {}
        }
        false
    }

    fn draw(&mut self, frame: &mut Frame, context: &StatusContext) {
        self.conversation.set_terminal_rows(frame.area().height);
        if let Some(review) = &mut self.review_transcript {
            review.set_terminal_rows(frame.area().height);
        }
        let area = frame.area();
        if area.width == 0 || area.height == 0 {
            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.skill_panel
        {
            panel.draw(frame, area);
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
            );
            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.session_panel
        {
            panel.draw(frame, area);
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
            );
            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.changes_panel
        {
            let available = area.height.saturating_sub(1);
            let height = available.saturating_sub(1);
            panel.draw(frame, Rect::new(area.x, area.y, area.width, height));
            if available > 0 {
                frame.render_widget(
                    Line::styled(changes::HINT, Style::default().fg(transcript_style::MUTED)),
                    Rect::new(area.x, area.y + height, area.width, 1),
                );
            }
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    context,
                    area.width,
                ),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
            );
            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.tree_panel
            && !panel.is_loading()
        {
            let available = area.height.saturating_sub(1);
            let height = available.saturating_sub(1);
            panel.draw(frame, Rect::new(area.x, area.y, area.width, height));
            if available > 0 {
                frame.render_widget(
                    Line::styled(tree::HINT, Style::default().fg(transcript_style::MUTED)),
                    Rect::new(area.x, area.y + height, area.width, 1),
                );
            }
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    context,
                    area.width,
                ),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
            );
            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.plan_panel
            && panel.token().is_none()
        {
            // 全屏审阅/进度视图：底部保留状态栏；Esc 由面板处理，只关闭面板。
            // 从 /plan-list 打开的详情/审阅额外显示一行列表快捷键。
            let with_hint = self.plan_list_panel.is_some();
            let available = area.height.saturating_sub(1);
            let panel_height = available.saturating_sub(u16::from(with_hint));
            panel.draw(
                frame,
                Rect::new(area.x, area.y, area.width, panel_height),
                self.plan_progress,
            );
            if with_hint && panel_height < available {
                frame.render_widget(
                    Line::styled(
                        format!(" {}", plan_list::DETAIL_HINT),
                        Style::default().fg(transcript_style::MUTED),
                    ),
                    Rect::new(area.x, area.y + panel_height, area.width, 1),
                );
            }
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    context,
                    area.width,
                ),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
            );
            return;
        }
        if !self.native_render
            && let Some(panel) = &mut self.plan_list_panel
            && self.plan_panel.is_none()
        {
            // 全屏计划管理页：底部保留状态栏；Esc 由面板处理，只关闭面板。
            let available = area.height.saturating_sub(1);
            panel.draw(frame, Rect::new(area.x, area.y, area.width, available));
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    context,
                    area.width,
                ),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
            );
            return;
        }
        let status_height = u16::from(area.height >= 2);
        let available = area.height.saturating_sub(status_height);
        let bordered = available >= 3 && area.width >= 3;
        let lines = self.input.lines().len().clamp(1, 6) as u16;
        let input_height = available.min(lines.saturating_add(if bordered { 2 } else { 0 }));
        // 菜单覆盖输入框上方，候选数量不参与对话区和输入框的布局。
        let conversation_height = available.saturating_sub(input_height);
        let menu_height = if self.native_render {
            0
        } else {
            self.menu
                .as_ref()
                .map_or(0, |menu| menu.height(conversation_height))
                .max(
                    self.model_panel
                        .as_ref()
                        .map_or(0, |panel| panel.height(conversation_height)),
                )
        };
        let menu_area = Rect::new(
            area.x,
            area.y + conversation_height - menu_height,
            area.width,
            menu_height,
        );
        let conversation = Rect::new(area.x, area.y, area.width, conversation_height);
        let input = Rect::new(
            area.x,
            area.y.saturating_add(conversation_height),
            area.width,
            input_height,
        );
        let status = Rect::new(area.x, input.bottom(), area.width, status_height);
        let conversation = if self.show_welcome && self.plan_panel.is_none() {
            welcome::draw(frame, conversation)
        } else {
            conversation
        };
        // 独立审查页优先渲染：底部输入与状态栏照常可用，草稿可继续编辑但提交被拒。
        if self.native_render {
            self.conversation.draw_native(frame, conversation);
        } else if let Some(panel) = &mut self.plan_panel {
            panel.draw(frame, conversation, self.plan_progress);
        } else {
            self.active_transcript().draw(frame, conversation);
        }
        if !self.native_render
            && let Some(panel) = &self.tree_panel
        {
            if input.height > 0 {
                panel.draw_status(frame, Rect::new(input.x, input.y, input.width, 1));
            }
            if input.height > 1 {
                frame.render_widget(
                    Line::styled(tree::HINT, Style::default().fg(transcript_style::MUTED)),
                    Rect::new(input.x, input.y + 1, input.width, 1),
                );
            }
            if status.height > 0 {
                frame.render_widget(
                    statusline::primary(
                        self.current_model.as_ref(),
                        self.think_level,
                        self.workflow.mode,
                        context,
                        status.width,
                    ),
                    status,
                );
            }
            self.notifications.draw(frame, conversation);
            return;
        }
        let editor_area = if bordered {
            let block = Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(
                    Style::default().fg(
                        if self
                            .plan_panel
                            .as_ref()
                            .is_some_and(|panel| !panel.input_focused())
                        {
                            transcript_style::MUTED
                        } else if self.menu.is_some() {
                            menu::SEPARATOR
                        } else if self.shell_mode {
                            // 模式独立于正文，首个切换用的 `!` 不在输入框显示。
                            statusline::SHELL_MODE
                        } else {
                            statusline::ACCENT
                        },
                    ),
                );
            let inner = block.inner(input);
            frame.render_widget(block, input);
            self.draw_input_status(frame, Rect::new(input.x, input.y, input.width, 1));
            inner
        } else {
            input
        };
        let prompt_width = editor_area.width.saturating_sub(1).min(2);
        if prompt_width > 0 && editor_area.height > 0 {
            frame.render_widget(
                Line::from("❯"),
                Rect::new(editor_area.x, editor_area.y, prompt_width, 1),
            );
        }
        let editor_area = Rect::new(
            editor_area.x + prompt_width,
            editor_area.y,
            editor_area.width - prompt_width,
            editor_area.height,
        );
        if editor_area.width > 0 && editor_area.height > 0 {
            frame.render_widget(&self.input, editor_area);
            // 渲染后获取折行和滚动偏移，让输入法与终端原生光标共享插入位置。
            let cursor = self.input.screen_cursor();
            let (top, left) = self.input.scroll_offset();
            if let (Some(row), Some(column)) = (
                cursor.row.checked_sub(usize::from(top)),
                cursor.col.checked_sub(usize::from(left)),
            ) && self.ask_panel.is_none()
                && self.model_panel.is_none()
                && self
                    .plan_panel
                    .as_ref()
                    .is_none_or(|panel| panel.input_focused())
                && row < usize::from(editor_area.height)
                && column < usize::from(editor_area.width)
            {
                frame.set_cursor_position((
                    editor_area.x + column as u16,
                    editor_area.y + row as u16,
                ));
            }
        }
        if menu_height > 0
            && let Some(menu) = &mut self.menu
        {
            menu.draw(frame, menu_area);
        }
        if !self.native_render
            && let Some(panel) = &mut self.model_panel
        {
            panel.draw(frame, menu_area);
        }
        if status.height > 0 {
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    self.workflow.mode,
                    context,
                    status.width,
                ),
                status,
            );
        }
        if self.native_render {
            self.normal_snapshot = Some(frame.buffer_mut().clone());
        }
        if !self.native_render
            && let Some(panel) = &mut self.ask_panel
        {
            panel.draw(
                frame,
                Rect::new(area.x, area.y, area.width, input.y.saturating_sub(area.y)),
            );
        }
        self.notifications.draw(
            frame,
            Rect::new(area.x, area.y, area.width, menu_area.y - area.y),
        );
    }

    fn draw_input_status(&self, frame: &mut Frame, area: Rect) {
        // 上下文固定在右端；压缩等临时状态显示在它左侧，空间不足时优先保留上下文。
        let usage_style = Style::default().bg(Color::Rgb(26, 27, 38));
        let mut right = area.right();
        if let Some(usage) = self.usage.as_ref() {
            let (text, color) = statusline::context_field(usage);
            let line = Line::styled(format!(" {text} "), usage_style.fg(color))
                .alignment(Alignment::Right);
            let width = u16::try_from(line.width())
                .unwrap_or(u16::MAX)
                .min(area.width);
            right -= width;
            frame.render_widget(line, Rect::new(right, area.y, width, 1));
        }
        let extra = self
            .secondary_status
            .as_deref()
            .filter(|text| !text.is_empty())
            .map(|extra| {
                Line::styled(format!(" {extra} "), Style::default().fg(Color::DarkGray))
                    .alignment(Alignment::Right)
            });
        let extra_width = extra.as_ref().map_or(0, |line| {
            u16::try_from(line.width())
                .unwrap_or(u16::MAX)
                .min((right - area.x).min(area.width / 2))
        });
        let mut left = area.x;
        if let Some(snapshot) = &self.queue_snapshot
            && !snapshot.items.is_empty()
        {
            let paused = if self.queue_paused {
                platform_text(" · Paused · ⌥ + R resume", " · Paused · Ctrl + R resume")
            } else {
                ""
            };
            let line = Line::styled(
                format!(
                    " Steering: {} · Queued: {}{paused} ",
                    snapshot.steering(),
                    snapshot.follow_up()
                ),
                Style::default().fg(Color::DarkGray),
            );
            let width = u16::try_from(line.width())
                .unwrap_or(u16::MAX)
                .min((right - left).saturating_sub(extra_width));
            frame.render_widget(line, Rect::new(left, area.y, width, 1));
            left += width;
        }
        if let Some(usage) = self.usage.as_ref() {
            let metrics = statusline::metric_fields(
                usage.last_request.as_ref(),
                self.token_speed.value(),
                (right - left).saturating_sub(extra_width),
            )
            .style(usage_style);
            let width = u16::try_from(metrics.width()).unwrap_or(u16::MAX);
            right -= width;
            frame.render_widget(metrics, Rect::new(right, area.y, width, 1));
        }
        if let Some(extra) = extra {
            right -= extra_width;
            frame.render_widget(extra, Rect::new(right, area.y, extra_width, 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, MouseButton, MouseEvent};
    use ratatui::{Terminal, backend::TestBackend, style::Modifier};

    fn app() -> App {
        let mut app = App::new(Err(anyhow::anyhow!("fixture configuration unavailable"))).unwrap();
        app.notifications = Notifications::default();
        app
    }

    fn draw_buffer(app: &mut App, width: u16, height: u16) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                app.draw(
                    frame,
                    &StatusContext {
                        workdir: "fixture-cwd".into(),
                        ..StatusContext::default()
                    },
                )
            })
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        let buffer = draw_buffer(app, width, height);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn changes_view(blocked: bool) -> ChangesView {
        let path = std::path::PathBuf::from("demo.txt");
        ChangesView {
            task_id: 1,
            revision: 2,
            files: vec![crate::tools::ChangedFile {
                path: path.clone(),
                diff: crate::events::FileDiff::Text {
                    created: false,
                    added: 1,
                    removed: 1,
                    hunks: vec![crate::events::DiffHunk {
                        lines: vec![
                            crate::events::DiffLine {
                                kind: crate::events::DiffLineKind::Removed,
                                old_line: Some(1),
                                new_line: None,
                                text: "user-local\n".into(),
                            },
                            crate::events::DiffLine {
                                kind: crate::events::DiffLineKind::Added,
                                old_line: None,
                                new_line: Some(1),
                                text: "agent-final\n".into(),
                            },
                        ],
                    }],
                },
                undo: if blocked {
                    Err("File changed outside this task; undo refused.".into())
                } else {
                    Ok(UndoToken {
                        task_id: 1,
                        revision: 2,
                        path,
                    })
                },
            }],
        }
    }

    fn changes_empty_view() -> ChangesView {
        ChangesView {
            task_id: 1,
            revision: 3,
            files: vec![],
        }
    }

    #[test]
    fn changes_loading_escape_waits_for_reply_and_never_reopens_panel() {
        let (mut app, requests, events) = plan_app();
        app.input.insert_str("existing draft");
        let cursor = app.input.cursor();
        let before = draw(&mut app, 100, 30);
        app.open_changes();
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::ReadChanges)
        ));
        let loading = draw(&mut app, 100, 30);
        assert!(loading.contains("Latest task changes"));
        assert!(!loading.contains("existing draft"));
        let mut dirty = false;
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.changes_panel.is_none());
        assert!(matches!(app.state, State::ReadingChanges));
        app.open_model_picker();
        app.open_tree();
        assert!(requests.try_recv().is_err());
        events
            .send(WorkerEvent::ChangesRead(changes_view(false)))
            .unwrap();
        app.receive();
        assert!(matches!(app.state, State::Ready));
        assert!(app.changes_panel.is_none());
        assert_eq!(app.input.lines(), &["existing draft"]);
        assert_eq!(app.input.cursor(), cursor);
        assert_eq!(draw(&mut app, 100, 30), before);
    }

    #[test]
    fn changes_panel_preserves_draft_history_viewport_and_blocks_shortcuts() {
        let (mut app, requests, events) = plan_app();
        for index in 0..40 {
            app.append("You", &format!("original message {index}"));
        }
        app.input_history = vec!["old prompt".into()];
        app.input.insert_str("existing draft\nsecond line");
        app.input.move_cursor(CursorMove::Head);
        let cursor = app.input.cursor();
        draw(&mut app, 100, 30);
        app.conversation.page_up();
        let before = draw(&mut app, 100, 30);
        app.open_changes();
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::ReadChanges)
        ));
        events
            .send(WorkerEvent::ChangesRead(changes_view(false)))
            .unwrap();
        app.receive();
        let panel = draw(&mut app, 100, 30);
        assert!(panel.contains("demo.txt"));
        assert!(panel.contains("user-local") && panel.contains("agent-final"));
        assert!(panel.contains("fixture-cwd"));
        assert!(!panel.contains("existing draft"));
        let mut dirty = false;
        for event in [
            key(KeyCode::Up),
            key(KeyCode::Down),
            key(KeyCode::PageUp),
            key(KeyCode::PageDown),
            function_key('p'),
            function_key('q'),
            function_key('r'),
            control('o'),
            control('a'),
            control('k'),
            key(KeyCode::F(1)),
            key(KeyCode::BackTab),
            Event::Paste("must not edit".into()),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 3,
                row: 4,
                modifiers: KeyModifiers::NONE,
            }),
            Event::Resize(50, 12),
        ] {
            app.handle(event, &mut dirty);
        }
        draw(&mut app, 50, 12);
        draw(&mut app, 100, 30);
        assert!(requests.try_recv().is_err());
        assert!(app.model_picker_draft.is_none());
        assert!(!app.queue_menu);
        assert_eq!(app.input.lines(), &["existing draft", "second line"]);
        assert_eq!(app.input.cursor(), cursor);
        assert_eq!(app.input_history, ["old prompt"]);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert_eq!(draw(&mut app, 100, 30), before);
    }

    #[test]
    fn changes_pending_followup_stays_queued_through_loading_view_and_confirmation() {
        let (mut app, requests, events) = plan_app();
        app.input_queue
            .enqueue(PendingKind::FollowUp, "next task".into(), None, false)
            .unwrap();
        app.input.insert_str("/changes");
        app.submit(PendingKind::Steering);
        assert!(app.input.is_empty());
        assert_eq!(app.input_history, ["/changes"]);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::ReadChanges)
        ));
        assert!(!app.queue_paused);
        assert!(!app.dispatch_next(true));
        events
            .send(WorkerEvent::ChangesRead(changes_view(false)))
            .unwrap();
        app.receive();
        assert!(app.has_pending());
        assert!(!app.dispatch_next(false));
        let mut dirty = false;
        app.handle(key(KeyCode::Char('u')), &mut dirty);
        assert!(draw(&mut app, 100, 30).contains("Undo file"));
        app.handle(function_key('r'), &mut dirty);
        app.handle(function_key('q'), &mut dirty);
        assert!(!app.dispatch_next(true));
        assert!(requests.try_recv().is_err());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.changes_panel.is_some());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.changes_panel.is_none());
        app.receive();
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt, .. }) if prompt == "next task")
        );
        assert!(!app.has_pending());
    }

    #[test]
    fn changes_running_command_is_rejected_without_steering_or_history() {
        for state in [State::Running, State::Cancelling] {
            let (mut app, requests, _events) = plan_app();
            app.state = state;
            app.input_queue.begin_run().unwrap();
            app.input.insert_str("/changes");
            let mut dirty = false;
            app.handle(key(KeyCode::Enter), &mut dirty);
            assert_eq!(app.input.lines(), &["/changes"]);
            assert!(app.input_history.is_empty());
            assert!(!app.has_pending());
            assert!(requests.try_recv().is_err());
            let screen = draw(&mut app, 110, 30);
            assert!(screen.contains("Finish or cancel the current task before opening"));
            assert!(screen.contains("/changes."));
        }
    }

    #[test]
    fn changes_plan_mode_never_sends_undo_and_mode_or_tree_cannot_bypass_panel() {
        let (mut app, requests, events) = plan_app();
        app.workflow.mode = WorkMode::Plan;
        app.open_changes();
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::ReadChanges)
        ));
        events
            .send(WorkerEvent::ChangesRead(changes_view(false)))
            .unwrap();
        app.receive();
        let mut dirty = false;
        app.handle(key(KeyCode::Char('u')), &mut dirty);
        app.handle(key(KeyCode::Tab), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(draw(&mut app, 100, 30).contains("Undo is unavailable in Plan mode."));
        assert!(!app.plan_transition_ready("pending input"));
        app.open_tree();
        app.open_model_picker();
        app.request_plan_read();
        assert!(app.tree_panel.is_none());
        assert!(app.model_picker_draft.is_none());
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn changes_undo_error_refresh_keeps_reason_and_success_removes_file() {
        let (mut app, requests, events) = plan_app();
        app.open_changes();
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::ReadChanges)
        ));
        events
            .send(WorkerEvent::ChangesRead(changes_view(false)))
            .unwrap();
        app.receive();
        let mut dirty = false;
        app.handle(key(KeyCode::Char('u')), &mut dirty);
        // Confirmation starts on Cancel: Enter never dispatches an undo.
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(requests.try_recv().is_err());
        app.handle(key(KeyCode::Char('u')), &mut dirty);
        app.handle(key(KeyCode::Tab), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::UndoFile(token)) if token.path == std::path::Path::new("demo.txt"))
        );
        assert!(matches!(app.state, State::UndoingFile));
        app.handle(key(KeyCode::Enter), &mut dirty);
        app.handle(key(KeyCode::Char('r')), &mut dirty);
        assert!(requests.try_recv().is_err());
        assert!(draw(&mut app, 100, 30).contains("Undoing file"));
        events
            .send(WorkerEvent::FileUndone(Err(anyhow::anyhow!(
                "specific refusal reason"
            ))))
            .unwrap();
        app.receive();
        assert!(matches!(app.state, State::UndoingFile));
        events
            .send(WorkerEvent::ChangesRead(changes_view(true)))
            .unwrap();
        app.receive();
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("specific refusal reason"));
        assert!(screen.contains("Blocked"));
        assert!(screen.contains("demo.txt"));
        assert!(matches!(app.state, State::Ready));
        app.handle(key(KeyCode::Char('r')), &mut dirty);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::ReadChanges)
        ));
        events
            .send(WorkerEvent::ChangesRead(changes_view(false)))
            .unwrap();
        app.receive();
        assert!(!draw(&mut app, 100, 30).contains("specific refusal reason"));
        app.handle(key(KeyCode::Char('u')), &mut dirty);
        app.handle(key(KeyCode::Tab), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::UndoFile(_))
        ));
        events
            .send(WorkerEvent::FileUndone(Ok(UndoResult {
                view: changes_empty_view(),
                warning: None,
            })))
            .unwrap();
        app.receive();
        assert!(app.changes_panel.is_some());
        assert!(draw(&mut app, 100, 30).contains("No edit/write changes in the latest task."));
        assert!(matches!(app.state, State::Ready));
    }

    #[test]
    fn changes_worker_disconnect_clears_panel_and_preserves_draft() {
        let (mut app, requests, events) = plan_app();
        app.input.insert_str("preserved draft");
        app.open_changes();
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::ReadChanges)
        ));
        drop(events);
        app.receive();
        assert!(matches!(app.state, State::Unavailable));
        assert!(app.changes_panel.is_none());
        assert_eq!(app.input.lines(), &["preserved draft"]);
    }

    #[test]
    fn tree_double_escape_deadline_and_reset_are_deterministic() {
        let now = Instant::now();
        let mut tap = EscapeTap::default();
        assert!(!tap.press(now));
        assert!(tap.press(now + Duration::from_millis(500)));
        assert!(!tap.press(now + Duration::from_secs(1)));
        assert!(!tap.press(now + Duration::from_millis(1501)));
        tap.reset();
        assert!(!tap.press(now + Duration::from_millis(1502)));
    }

    #[test]
    fn tree_double_escape_cancelled_scan_ignores_late_result_and_keeps_draft() {
        let mut app = app();
        app.state = State::Ready;
        let (commands, requests) = mpsc::channel();
        let (events, replies) = mpsc::channel();
        app.commands = Some(commands);
        app.events = Some(replies);
        let mut dirty = false;
        app.handle(key(KeyCode::Esc), &mut dirty);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadTree)));
        assert!(app.tree_panel.is_some());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.tree_panel.is_none());
        assert!(matches!(app.state, State::ReadingTree));
        events
            .send(WorkerEvent::TreeRead(Ok(
                crate::agent::persistence::tree::View {
                    tree: crate::agent::persistence::tree::Tree::fixture(
                        &[(None, crate::agent::persistence::tree::Kind::User, "seed")],
                        Some(0),
                    ),
                    records: vec![vec![crate::agent::persistence::ReplayRecord::User(
                        "seed".into(),
                    )]],
                },
            )))
            .unwrap();
        app.receive();
        assert!(app.tree_panel.is_none());
        app.input.insert_str("保留草稿");
        app.handle(key(KeyCode::Esc), &mut dirty);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(requests.try_recv().is_err());
        assert_eq!(app.input.lines(), &["保留草稿"]);
        app.clear_input();
        app.handle(key(KeyCode::Esc), &mut dirty);
        app.handle(Event::Paste("".into()), &mut dirty);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(requests.try_recv().is_err());
        let token = CancellationToken::new();
        app.active_cancellation = Some(token.clone());
        app.state = State::Running;
        app.handle(key(KeyCode::Esc), &mut dirty);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(token.is_cancelled());
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn tree_inline_preview_keeps_history_viewport_draft_and_paste_isolated() {
        use crate::agent::persistence::{
            ReplayRecord,
            tree::{Kind, Tree, View},
        };
        let mut app = app();
        app.state = State::Ready;
        for i in 0..40 {
            app.append("You", &format!("original message {i}"));
        }
        app.input.insert_str("existing draft");
        draw(&mut app, 100, 30);
        app.conversation.page_up();
        let before = draw(&mut app, 100, 30);
        let history = app.input_history.clone();
        let (commands, requests) = mpsc::channel();
        let (events, replies) = mpsc::channel();
        app.commands = Some(commands);
        app.events = Some(replies);
        app.open_tree();
        assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadTree)));
        let loading = draw(&mut app, 100, 30);
        assert!(loading.contains("Loading session tree…"));
        assert!(loading.contains("user turns"));
        assert_eq!(
            before.lines().take(26).collect::<Vec<_>>(),
            loading.lines().take(26).collect::<Vec<_>>()
        );
        events
            .send(WorkerEvent::TreeRead(Ok(View {
                tree: Tree::fixture(
                    &[
                        (None, Kind::User, "preview only"),
                        (Some(0), Kind::Assistant, "branch answer"),
                    ],
                    Some(1),
                ),
                records: vec![
                    vec![ReplayRecord::User("preview only".into())],
                    vec![
                        ReplayRecord::Event(AgentEvent::AssistantStarted),
                        ReplayRecord::Event(AgentEvent::AssistantFinished {
                            text: "branch answer".into(),
                        }),
                    ],
                ],
            })))
            .unwrap();
        app.receive();
        let preview = draw(&mut app, 100, 30);
        assert!(preview.contains("preview only") && preview.contains("branch answer"));
        assert!(!preview.contains("existing draft"));
        let mut dirty = false;
        for event in [
            key(KeyCode::Up),
            key(KeyCode::Char('f')),
            Event::Paste("must not search or edit".into()),
            key(KeyCode::PageDown),
        ] {
            app.handle(event, &mut dirty);
        }
        assert_eq!(app.input.lines(), &["existing draft"]);
        assert_eq!(app.input_history, history);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.tree_panel.is_none());
        assert_eq!(draw(&mut app, 100, 30), before);
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn tree_navigation_error_preserves_transcript_draft_and_panel() {
        let mut app = app();
        app.state = State::NavigatingTree;
        app.append("You", "original branch");
        app.input.insert_str("existing draft");
        app.tree_panel = Some(tree::Panel::new());
        app.tree_panel.as_mut().unwrap().pending = true;
        let (events, replies) = mpsc::channel();
        app.events = Some(replies);
        events
            .send(WorkerEvent::TreeNavigated(Err(anyhow::anyhow!(
                "cannot switch model: unsupported history item"
            ))))
            .unwrap();
        app.receive();
        assert!(matches!(app.state, State::Ready));
        assert_eq!(app.input.lines(), &["existing draft"]);
        assert!(!app.tree_panel.as_ref().unwrap().pending);
        app.tree_panel = None;
        assert!(draw(&mut app, 100, 30).contains("original branch"));
    }

    #[test]
    fn resume_cancelled_scan_never_reopens_panel_and_keeps_worker_busy_until_reply() {
        let mut app = app();
        app.state = State::Ready;
        app.append("You", "original conversation");
        let original = draw(&mut app, 100, 30);
        let (commands, requests) = mpsc::channel();
        let (events, replies) = mpsc::channel();
        app.commands = Some(commands);
        app.events = Some(replies);
        let mut dirty = false;
        app.handle(Event::Paste("/resume".into()), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::ListSessions)
        ));
        assert!(draw(&mut app, 100, 30).contains("Loading saved sessions…"));
        app.handle(Event::Paste("must not enter editor".into()), &mut dirty);
        app.handle(function_key('p'), &mut dirty);
        app.handle(key(KeyCode::F(1)), &mut dirty);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(draw(&mut app, 100, 30).contains("original conversation"));
        app.handle(Event::Paste("next prompt".into()), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(app.input.lines(), &["next prompt"]);
        events
            .send(WorkerEvent::SessionsListed(Ok(
                crate::agent::persistence::SessionListing {
                    sessions: vec![crate::agent::persistence::SessionSummary {
                        id: "late".into(),
                        cwd: "/tmp".into(),
                        updated_at: std::time::SystemTime::now(),
                        summary: "late scan result".into(),
                        preview: String::new(),
                        bytes: 0,
                        done: false,
                    }],
                    diagnostics: Vec::new(),
                },
            )))
            .unwrap();
        app.receive();
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("original conversation"));
        assert!(!screen.contains("Resume session"));
        assert!(!screen.contains("late scan result"));
        assert_eq!(screen.lines().last(), original.lines().last());
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt, .. }) if prompt == "next prompt")
        );
    }

    #[test]
    fn notification_overlay_preserves_welcome_draft_and_cursor() {
        let mut app = app();
        app.show_welcome = true;
        app.input.insert_str("草稿🙂");
        let cursor = app.input.cursor();
        let before = draw(&mut app, 100, 30);
        let now = Instant::now();
        app.notifications
            .show(Level::Error, "temporary error".into(), now);
        app.notifications.tick(now + Duration::from_millis(200));
        let shown = draw(&mut app, 100, 30);
        assert!(shown.contains("temporary error"));
        assert!(app.show_welcome);
        assert_eq!(app.input.cursor(), cursor);
        assert_eq!(app.input.lines(), &["草稿🙂"]);
        assert_eq!(shown.lines().last(), before.lines().last());
        assert!(app.notifications.tick(now + Duration::from_millis(10000)));
        assert_eq!(draw(&mut app, 100, 30), before);
    }

    #[test]
    fn ask_roundtrip_disconnect_and_cancelled_answer_return_typed_cancelled() {
        for cancel_before_reply in [false, true] {
            let (events, requests) = mpsc::sync_channel(1);
            let token = CancellationToken::new();
            let worker_token = token.clone();
            let worker = thread::spawn(move || {
                let args = crate::interaction::AskArgs::parse(
                    r#"{"questions":[{"id":"q","question":"Question","options":[]}]}"#,
                )
                .unwrap();
                request_ask(
                    &events,
                    crate::interaction::AskRequest {
                        call_id: "race".into(),
                        args: std::sync::Arc::new(args),
                    },
                    &worker_token,
                )
            });
            let WorkerEvent::Ask { reply, .. } =
                requests.recv_timeout(Duration::from_secs(1)).unwrap()
            else {
                panic!("ask request expected")
            };
            if cancel_before_reply {
                token.cancel();
                let _ = reply.send(vec![crate::interaction::AskAnswer {
                    id: "q".into(),
                    selected_options: Vec::new(),
                    custom_input: Some("late".into()),
                }]);
            }
            drop(reply);
            let error = worker.join().unwrap().unwrap_err();
            assert!(error.is::<crate::cancellation::Cancelled>());
            assert!(token.is_cancelled());
        }
    }

    fn ask_fixture(
        app: &mut App,
    ) -> (
        mpsc::Receiver<Vec<crate::interaction::AskAnswer>>,
        CancellationToken,
    ) {
        let token = CancellationToken::new();
        app.state = State::Running;
        app.active_cancellation = Some(token.clone());
        app.conversation.begin_run();
        let args = crate::interaction::AskArgs::parse(r#"{"questions":[{"id":"choice","question":"Choose storage","options":[{"label":"SQLite"},{"label":"Postgres"}],"recommended":0}]}"#).unwrap();
        let (reply, answers) = mpsc::sync_channel(1);
        app.ask_panel = Some(ask::Panel::new(crate::interaction::AskRequest {
            call_id: "ask-1".into(),
            args: std::sync::Arc::new(args),
        }));
        app.ask_reply = Some(reply);
        (answers, token)
    }

    fn folding_fixture(app: &mut App) {
        app.conversation = Transcript::with_read_preview(true);
        for id in ["first", "second"] {
            app.conversation.progress(AgentEvent::ToolStarted {
                id: id.into(),
                name: "read".into(),
                arguments: format!(r#"{{"path":"{id}"}}"#),
            });
            app.conversation.progress(AgentEvent::ToolFinished {
                id: id.into(),
                result: serde_json::json!({"ok":true,"content":(0..20).map(|i| format!("{id}-{i:02}")).collect::<Vec<_>>().join("\n")}),
            });
        }
    }

    #[test]
    fn ctrl_o_is_global_press_only_and_keeps_ask_and_main_draft() {
        let mut app = app();
        folding_fixture(&mut app);
        app.input.insert_str("主草稿🙂");
        app.input.move_cursor(CursorMove::Back);
        let cursor = app.input.cursor();
        let (answers, token) = ask_fixture(&mut app);
        let mut dirty = false;
        app.handle(key(KeyCode::Down), &mut dirty);
        let collapsed = draw(&mut app, 100, 100);
        assert!(collapsed.contains("Ctrl + O to expand"));
        app.handle(control('o'), &mut dirty);
        let expanded = draw(&mut app, 100, 100);
        for expected in ["first-19", "second-19", "Postgres"] {
            assert!(expanded.contains(expected), "{expanded}");
        }
        let mut repeat = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
        repeat.kind = KeyEventKind::Repeat;
        app.handle(Event::Key(repeat), &mut dirty);
        repeat.kind = KeyEventKind::Release;
        app.handle(Event::Key(repeat), &mut dirty);
        assert!(!draw(&mut app, 100, 100).contains("to expand"));
        app.handle(control('o'), &mut dirty);
        assert!(draw(&mut app, 100, 100).contains("to expand"));
        assert_eq!(app.input.lines(), &["主草稿🙂"]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(app.ask_panel.is_some());
        assert!(!token.is_cancelled());
        assert!(matches!(answers.try_recv(), Err(TryRecvError::Empty)));
        app.handle(key(KeyCode::Enter), &mut dirty);
        let response = answers.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(response[0].selected_options, ["Postgres"]);
    }

    #[test]
    fn ctrl_o_keeps_normal_menu_and_alt_o_is_not_a_toggle() {
        let mut app = app();
        app.state = State::Ready;
        folding_fixture(&mut app);
        app.input.insert_str("/");
        app.refresh_candidates(true);
        let mut dirty = false;
        app.handle(key(KeyCode::Down), &mut dirty);
        let selected = app.menu.as_ref().unwrap().selected_index();
        let cursor = app.input.cursor();
        app.handle(control('o'), &mut dirty);
        assert_eq!(app.menu.as_ref().unwrap().selected_index(), selected);
        assert_eq!(app.input.cursor(), cursor);
        assert_eq!(app.input.lines(), &["/"]);
        assert!(!draw(&mut app, 100, 100).contains("to expand"));
        app.handle(
            Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT)),
            &mut dirty,
        );
        assert!(!draw(&mut app, 100, 100).contains("to expand"));
    }

    #[test]
    fn ask_focus_submission_keeps_main_draft_and_sends_once() {
        let mut app = app();
        app.input.insert_str("主草稿🙂");
        app.input.move_cursor(CursorMove::Back);
        let cursor = app.input.cursor();
        let (answers, token) = ask_fixture(&mut app);
        let mut dirty = false;
        app.handle(function_key('p'), &mut dirty);
        app.handle(key(KeyCode::F(1)), &mut dirty);
        app.handle(Event::Paste("must stay inside panel".into()), &mut dirty);
        app.handle(Event::Resize(24, 10), &mut dirty);
        draw(&mut app, 24, 10);
        assert_eq!(app.input.lines(), &["主草稿🙂"]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(matches!(answers.try_recv(), Err(TryRecvError::Empty)));
        app.handle(key(KeyCode::Down), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        let response = answers.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(response[0].selected_options, ["Postgres"]);
        assert!(!token.is_cancelled());
        assert!(app.ask_panel.is_none());
        assert!(matches!(
            answers.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
        assert_eq!(app.input.cursor(), cursor);
    }

    #[test]
    fn ask_cancel_exit_and_late_request_never_reopen_panel() {
        for exit in [false, true] {
            let mut app = app();
            let (answers, token) = ask_fixture(&mut app);
            if exit {
                app.close_ask_on_exit();
            } else {
                app.handle(key(KeyCode::Esc), &mut false);
            }
            assert!(token.is_cancelled());
            assert!(matches!(
                answers.try_recv(),
                Err(TryRecvError::Disconnected)
            ));
            let args = crate::interaction::AskArgs::parse(
                r#"{"questions":[{"id":"late","question":"Late question","options":[]}]}"#,
            )
            .unwrap();
            let (reply, late) = mpsc::sync_channel(1);
            let (events, receiver) = mpsc::channel();
            app.events = Some(receiver);
            events
                .send(WorkerEvent::Ask {
                    request: crate::interaction::AskRequest {
                        call_id: "late".into(),
                        args: std::sync::Arc::new(args),
                    },
                    reply,
                })
                .unwrap();
            app.receive();
            assert!(app.ask_panel.is_none());
            assert!(matches!(late.try_recv(), Err(TryRecvError::Disconnected)));
        }
    }

    fn assert_notification(app: &mut App, color: Color) {
        app.notifications
            .tick(Instant::now() + Duration::from_millis(200));
        let buffer = draw_buffer(app, 100, 30);
        assert!(
            buffer
                .content
                .iter()
                .any(|cell| cell.symbol() == "╭" && cell.fg == color)
        );
    }

    #[test]
    fn task_cancel_idle_menu_repeat_and_modified_escape_do_not_cancel() {
        let mut app = app();
        app.state = State::Ready;
        app.input.insert_str("保留草稿");
        let cursor = app.input.cursor();
        let mut dirty = false;
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert_eq!(app.input.lines(), &["保留草稿"]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(app.active_cancellation.is_none());
        app.input = editor();
        app.input.insert_str("/");
        app.refresh_candidates(true);
        assert!(app.menu.is_some());
        let token = CancellationToken::new();
        app.active_cancellation = Some(token.clone());
        app.state = State::Running;
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.menu.is_none());
        assert!(!token.is_cancelled());
        app.handle(
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Esc,
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )),
            &mut dirty,
        );
        app.handle(
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Esc,
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            &mut dirty,
        );
        app.handle(
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::ALT)),
            &mut dirty,
        );
        assert!(!token.is_cancelled());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(token.is_cancelled());
        assert!(matches!(app.state, State::Cancelling));
    }

    #[test]
    fn bang_shell_input_consumes_only_mode_prefix() {
        let mut app = app();
        app.state = State::Ready;
        let (commands, requests) = mpsc::channel();
        app.commands = Some(commands);
        let mut dirty = false;
        app.handle(key(KeyCode::Char('!')), &mut dirty);
        assert!(app.shell_mode);
        assert_eq!(app.input.lines(), &[""]);
        assert!(
            draw_buffer(&mut app, 100, 30)
                .content
                .iter()
                .any(|cell| { cell.symbol() == "─" && cell.fg == statusline::SHELL_MODE })
        );
        app.submit(PendingKind::Steering);
        assert!(requests.try_recv().is_err());

        app.handle(key(KeyCode::Char('!')), &mut dirty);
        app.handle(Event::Paste(" false".into()), &mut dirty);
        assert_eq!(app.input.lines(), &["! false"]);
        app.handle(key(KeyCode::Enter), &mut dirty);
        let WorkerCommand::Shell { command, .. } = requests.try_recv().unwrap() else {
            panic!("应提交本地 shell 命令");
        };
        assert_eq!(command, "! false");
        assert_eq!(app.input_history, ["!! false"]);
        assert!(!app.shell_mode);
        assert_eq!(app.input.lines(), &[""]);
    }

    #[test]
    fn bang_shell_paste_history_and_picker_preserve_mode() {
        let mut app = app();
        app.state = State::Ready;
        let mut dirty = false;
        app.handle(
            Event::Paste("  !printf '你好!'\n! false".into()),
            &mut dirty,
        );
        assert!(app.shell_mode);
        assert_eq!(app.input.lines(), &["  printf '你好!'", "! false"]);
        assert_eq!(app.input.cursor(), ratatui_textarea::DataCursor(1, 7));
        assert!(app.input.undo());
        assert_eq!(app.input.lines(), &[""]);
        assert!(app.input.redo());
        assert_eq!(app.input.lines(), &["  printf '你好!'", "! false"]);
        app.input_history.push("ordinary prompt".into());
        app.navigate_history(true);
        assert!(!app.shell_mode);
        app.navigate_history(false);
        assert!(app.shell_mode);
        assert_eq!(app.input.lines(), &["  printf '你好!'", "! false"]);
        app.open_model_picker();
        assert!(!app.shell_mode);
        app.close_model_panel();
        assert!(app.shell_mode);
        app.remember_input();
        app.clear_input();
        app.navigate_history(true);
        assert!(app.shell_mode);
        assert_eq!(app.input.lines(), &["  printf '你好!'", "! false"]);
    }

    #[test]
    fn bang_shell_input_exits_mode_and_suppresses_menus() {
        let mut app = app();
        app.state = State::Ready;
        let mut dirty = false;
        app.handle(key(KeyCode::Char('!')), &mut dirty);
        app.handle(key(KeyCode::Backspace), &mut dirty);
        assert!(!app.shell_mode);
        app.handle(Event::Paste("!/model @file".into()), &mut dirty);
        assert!(app.shell_mode);
        assert!(app.menu.is_none());
        assert!(app.model_panel.is_none());
        assert!(app.file_query.is_none());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(!app.shell_mode);
        assert_eq!(app.input.lines(), &["/model @file"]);
        app.clear_input();
        app.handle(Event::Paste("!echo ok".into()), &mut dirty);
        app.handle(control('a'), &mut dirty);
        app.handle(control('k'), &mut dirty);
        assert!(!app.shell_mode);
        assert_eq!(app.input.lines(), &[""]);
    }

    #[test]
    fn bang_shell_cleanup_failure_disables_future_execution() {
        let mut app = app();
        app.state = State::RunningShell;
        app.input.insert_str("draft remains");
        let (commands, requests) = mpsc::channel();
        let (events, receiver) = mpsc::channel();
        app.commands = Some(commands);
        app.events = Some(receiver);
        events
            .send(WorkerEvent::ShellCompleted(Err(
                crate::cancellation::CleanupError(anyhow::anyhow!("process group still alive"))
                    .into(),
            )))
            .unwrap();
        drop(events);
        app.receive();
        assert!(matches!(app.state, State::Unavailable));
        assert_eq!(app.input.lines(), &["draft remains"]);
        assert_eq!(
            app.unavailable.as_deref(),
            Some("Execution unavailable: shell cleanup failed; restart pp.")
        );
        assert!(draw(&mut app, 100, 30).contains("process group still alive"));
        app.submit(PendingKind::Steering);
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn completed_turn_uses_worker_timing_and_preserves_draft() {
        use chrono::TimeZone;

        for (seconds, duration) in [(0, "0s"), (5, "5s"), (65, "65s"), (3725, "3725s")] {
            let mut app = app();
            app.state = State::Running;
            app.show_welcome = false;
            app.notifications.clear();
            app.input.insert_str("下一轮草稿");
            let cursor = app.input.cursor();
            let (events, receiver) = mpsc::channel();
            app.events = Some(receiver);
            events
                .send(WorkerEvent::Progress(AgentEvent::AssistantFinished {
                    text: "finished answer".into(),
                }))
                .unwrap();
            events
                .send(WorkerEvent::Completed {
                    result: Ok(RunOutcome::Completed),
                    elapsed: Duration::from_secs(seconds),
                    finished_at: chrono::Local
                        .with_ymd_and_hms(2026, 10, 19, 19, 6, 0)
                        .unwrap(),
                })
                .unwrap();
            app.receive();
            let expected = format!("✻ Completed · {duration} · done 19:06");
            let screen = draw(&mut app, 100, 30);
            assert!(screen.contains(&expected), "{screen}");
            assert_eq!(screen.matches("Completed ·").count(), 1);
            assert!(screen.find("finished answer").unwrap() < screen.find(&expected).unwrap());
            assert!(matches!(app.state, State::Ready));
            assert_eq!(app.input.lines(), &["下一轮草稿"]);
            assert_eq!(app.input.cursor(), cursor);
            app.receive();
            assert_eq!(draw(&mut app, 100, 30).matches("Completed ·").count(), 1);
            assert!(draw(&mut app, 35, 30).contains("Completed ·"));
        }
    }

    #[test]
    fn cancelled_and_failed_turns_have_no_completion_summary() {
        for result in [
            Ok(RunOutcome::Cancelled),
            Err(anyhow::anyhow!("request failed")),
        ] {
            let mut app = app();
            app.state = State::Running;
            app.show_welcome = false;
            let (events, receiver) = mpsc::channel();
            app.events = Some(receiver);
            events
                .send(WorkerEvent::completed(Instant::now(), result))
                .unwrap();
            app.receive();
            assert!(!draw(&mut app, 100, 30).contains("Completed ·"));
        }
    }

    #[test]
    fn task_cancel_waits_for_completed_and_preserves_editable_draft() {
        let mut app = app();
        app.state = State::Ready;
        app.show_welcome = false;
        let (commands, requests) = mpsc::channel();
        let (events, receiver) = mpsc::channel();
        app.commands = Some(commands);
        app.events = Some(receiver);
        let mut dirty = false;
        app.input.insert_str("first task");
        app.handle(key(KeyCode::Enter), &mut dirty);
        let WorkerCommand::Run { cancellation, .. } = requests.try_recv().unwrap() else {
            panic!()
        };
        app.input.insert_str("保留草稿");
        app.input.move_cursor(CursorMove::Back);
        let cursor = app.input.cursor();
        let history = app.input_history.clone();
        app.secondary_status = Some("secondary intact".into());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(cancellation.is_cancelled());
        app.handle(key(KeyCode::Esc), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        app.handle(
            Event::Key(KeyEvent::new(KeyCode::Char('p'), FUNCTION_MODIFIER)),
            &mut dirty,
        );
        app.handle(key(KeyCode::BackTab), &mut dirty);
        assert!(requests.try_recv().is_err());
        assert_eq!(app.input.lines(), &["保留草稿"]);
        assert_eq!(app.input.cursor(), cursor);
        assert_eq!(app.input_history, history);
        assert!(matches!(app.state, State::Cancelling));
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("✻ Cancelling ·"));
        assert_eq!(screen.matches("Cancelling").count(), 1);
        assert!(screen.contains("secondary intact"));
        app.handle(Event::Paste("新".into()), &mut dirty);
        events
            .send(WorkerEvent::completed(
                Instant::now(),
                Ok(RunOutcome::Cancelled),
            ))
            .unwrap();
        app.receive();
        assert!(matches!(app.state, State::Ready));
        assert!(app.active_cancellation.is_none());
        assert_eq!(app.input.lines(), &["保留草新稿"]);
        assert_eq!(
            draw(&mut app, 100, 30)
                .matches(crate::cancellation::CANCELLED_NOTE)
                .count(),
            1
        );
    }

    #[test]
    fn task_cancel_committed_completion_wins_and_next_token_is_independent() {
        let mut app = app();
        app.state = State::Ready;
        app.show_welcome = false;
        let (commands, requests) = mpsc::channel();
        let (events, receiver) = mpsc::channel();
        app.commands = Some(commands);
        app.events = Some(receiver);
        let mut dirty = false;
        app.input.insert_str("first task");
        app.handle(key(KeyCode::Enter), &mut dirty);
        let WorkerCommand::Run {
            cancellation: old, ..
        } = requests.try_recv().unwrap()
        else {
            panic!()
        };
        // 完成已在 worker 提交，UI 尚未接到 Completed；迟到 Esc 只取消旧 token。
        events
            .send(WorkerEvent::completed(
                Instant::now(),
                Ok(RunOutcome::Completed),
            ))
            .unwrap();
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(old.is_cancelled());
        app.receive();
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(matches!(app.state, State::Ready));
        assert!(!draw(&mut app, 100, 30).contains(crate::cancellation::CANCELLED_NOTE));
        app.input.insert_str("next task");
        app.handle(key(KeyCode::Enter), &mut dirty);
        let WorkerCommand::Run {
            cancellation: next, ..
        } = requests.try_recv().unwrap()
        else {
            panic!()
        };
        old.cancel();
        assert!(!next.is_cancelled());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(next.is_cancelled());
        assert!(matches!(app.state, State::Cancelling));
        assert!(app.handle(
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &mut dirty
        ));
    }

    #[test]
    fn session_and_task_failure_notifications_never_enter_transcript() {
        let mut app = app();
        app.state = State::Running;
        app.show_welcome = false;
        app.input.insert_str("保留草稿🙂");
        let cursor = app.input.cursor();
        app.conversation.progress(AgentEvent::AssistantStarted);
        app.conversation.progress(AgentEvent::AssistantDelta {
            text: "partial answer".into(),
            refusal: false,
        });
        let (sender, receiver) = mpsc::channel();
        app.events = Some(receiver);
        sender
            .send(WorkerEvent::SessionStarted("fixture-session".into()))
            .unwrap();
        app.receive();
        assert_notification(&mut app, statusline::ACCENT);
        assert!(draw(&mut app, 100, 30).contains("Session: fixture-session"));
        sender
            .send(WorkerEvent::completed(
                Instant::now(),
                Err(anyhow::anyhow!("request failed")),
            ))
            .unwrap();
        app.receive();
        assert!(matches!(app.state, State::Ready));
        assert_notification(&mut app, Color::Rgb(224, 108, 117));
        assert!(draw(&mut app, 100, 30).contains("request failed"));
        let snapshot = draw_buffer(&mut app, 100, 30);
        let mut scrollback = Vec::new();
        app.conversation
            .write_full_for_test(&mut scrollback, &snapshot)
            .unwrap();
        let history = String::from_utf8_lossy(&scrollback);
        assert!(history.contains("partial answer"));
        assert!(!history.contains("fixture-session"));
        assert!(!history.contains("request failed"));
        app.notifications
            .tick(Instant::now() + Duration::from_millis(10000));
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("partial answer"));
        assert!(!screen.contains("request failed"));
        assert!(!screen.contains("Session:"));
        assert_eq!(app.input.lines(), &["保留草稿🙂"]);
        assert_eq!(app.input.cursor(), cursor);
    }

    #[test]
    fn startup_and_worker_errors_are_popups_without_dismissing_welcome() {
        let mut app = App::new(Err(anyhow::anyhow!("fixture configuration unavailable"))).unwrap();
        assert_notification(&mut app, Color::Rgb(224, 108, 117));
        assert!(draw(&mut app, 100, 30).contains("fixture configuration unavailable"));
        assert!(app.show_welcome);
        app.input.insert_str("草稿");
        app.fail_worker();
        assert_notification(&mut app, Color::Rgb(224, 108, 117));
        assert!(draw(&mut app, 100, 30).contains("agent worker channel closed"));
        assert!(matches!(app.state, State::Unavailable));
        app.notifications
            .tick(Instant::now() + Duration::from_millis(10000));
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("▄▄▄██▀▀██"));
        assert!(!screen.contains("configuration unavailable"));
        assert!(!screen.contains("channel closed"));
        assert_eq!(app.input.lines(), &["草稿"]);
    }

    fn active_menu(app: &App) -> &SelectionMenu<MenuAction> {
        app.model_panel
            .as_ref()
            .map(|panel| &panel.menu)
            .or(app.menu.as_ref())
            .unwrap()
    }

    fn model_query(app: &App) -> &str {
        &app.model_panel.as_ref().unwrap().search.lines()[0]
    }

    fn menu_values(app: &App) -> Vec<MenuAction> {
        app.model_panel
            .as_ref()
            .map(|panel| &panel.menu)
            .or(app.menu.as_ref())
            .map_or_else(Vec::new, |menu| {
                menu.items().iter().map(|item| item.value).collect()
            })
    }

    #[test]
    fn file_picker_activation_filters_without_rescanning() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new_with_skills(
            Err(anyhow::anyhow!("fixture")),
            crate::skills::Skills::default().shared(),
            commands::FileCommands::default(),
            dir.path(),
            NotificationOptions::default(),
        )
        .unwrap();
        let (requests, scans) = mpsc::channel();
        let (replies, receiver) = mpsc::channel();
        app.file_worker = files::Worker {
            requests,
            replies: receiver,
        };
        let mut dirty = false;
        app.handle(Event::Paste("分析\n查看 @".into()), &mut dirty);
        scans.try_recv().unwrap();
        assert!(app.files_pending);
        app.handle(Event::Paste("main".into()), &mut dirty);
        assert!(scans.try_recv().is_err());
        replies
            .send(Ok(vec!["src/main.rs".into(), "src/menu.rs".into()]))
            .unwrap();
        app.receive();
        assert_eq!(menu_values(&app), [MenuAction::FileDirectory(0)]);
        assert_eq!(app.file_query.as_ref().unwrap().prefix, "main");
        app.state = State::Running;
        app.refresh_candidates(false);
        assert!(app.menu.is_none());
        assert!(app.file_query.is_none());
    }

    type FilePickerFixture = (
        App,
        Receiver<()>,
        Sender<Result<Vec<String>>>,
        Receiver<WorkerCommand>,
    );

    fn file_picker_fixture() -> FilePickerFixture {
        let dir = tempfile::tempdir().unwrap();
        let mut app = App::new_with_skills(
            Err(anyhow::anyhow!("fixture")),
            crate::skills::Skills::default().shared(),
            commands::FileCommands::default(),
            dir.path(),
            NotificationOptions::default(),
        )
        .unwrap();
        let (requests, scans) = mpsc::channel();
        let (replies, receiver) = mpsc::channel();
        app.file_worker = files::Worker {
            requests,
            replies: receiver,
        };
        let (sender, commands) = mpsc::channel();
        app.commands = Some(sender);
        app.state = State::Ready;
        app.notifications.clear();
        (app, scans, replies, commands)
    }

    #[test]
    fn file_picker_enter_and_tab_replace_only_current_reference() {
        for confirm in [KeyCode::Enter, KeyCode::Tab] {
            let (mut app, scans, replies, commands) = file_picker_fixture();
            let mut dirty = false;
            app.handle(
                Event::Paste("首行\n分析🙂 @src/old.rs 后续".into()),
                &mut dirty,
            );
            for _ in 0..9 {
                app.handle(key(KeyCode::Left), &mut dirty);
            }
            scans.try_recv().unwrap();
            replies.send(Ok(vec!["src/main.rs".into()])).unwrap();
            app.receive();
            app.handle(key(confirm), &mut dirty);
            assert_eq!(app.input.lines(), &["首行", "分析🙂 @src/ 后续"]);
            assert_eq!(app.input.cursor(), (1, 9));
            assert_eq!(app.file_directory, "src/");
            assert!(commands.try_recv().is_err());
            app.handle(key(confirm), &mut dirty);
            assert_eq!(app.input.lines(), &["首行", "分析🙂 @src/main.rs 后续"]);
            assert_eq!(app.input.cursor(), (1, 17));
            assert!(app.menu.is_none() && app.file_query.is_none());
            assert!(app.input_history.is_empty());
            assert!(commands.try_recv().is_err());
            assert!(matches!(app.state, State::Ready));
        }
    }

    #[test]
    fn file_picker_directory_navigation_completes_draft_without_submitting() {
        let (mut app, scans, replies, commands) = file_picker_fixture();
        let mut dirty = false;
        app.handle(Event::Paste("查看 @".into()), &mut dirty);
        scans.try_recv().unwrap();
        replies
            .send(Ok(vec![
                "README.md".into(),
                "docs/my folder/说明.md".into(),
                "src/main.rs".into(),
                "src/tui/menu.rs".into(),
            ]))
            .unwrap();
        app.receive();
        let labels = |app: &App| {
            app.menu
                .as_ref()
                .unwrap()
                .items()
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>()
                .join(",")
        };
        assert_eq!(labels(&app), "docs/,src/,README.md");
        let cursor = app.input.cursor();
        app.handle(key(KeyCode::Down), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert_eq!(app.input.lines(), &["查看 @src/"]);
        assert_eq!(app.input.cursor(), (0, 8));
        assert_eq!(app.file_directory, "src/");
        assert_eq!(labels(&app), "../,tui/,main.rs");
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.file_directory, "src/tui/");
        assert_eq!(app.input.lines(), &["查看 @src/tui/"]);
        assert_eq!(app.input.cursor(), (0, 12));
        assert_eq!(labels(&app), "../,menu.rs");
        app.handle(key(KeyCode::Home), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert_eq!(app.file_directory, "src/");
        assert_eq!(app.input.lines(), &["查看 @src/"]);
        app.handle(key(KeyCode::Home), &mut dirty);
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert!(app.file_directory.is_empty());
        assert_eq!(labels(&app), "docs/,src/,README.md");
        assert_eq!(app.input.lines(), &["查看 @"]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(app.input_history.is_empty());
        assert!(commands.try_recv().is_err());
        assert!(scans.try_recv().is_err());
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.file_directory, "docs/");
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert_eq!(app.file_directory, "docs/my folder/");
        assert_eq!(app.input.lines(), &["查看 @\"docs/my folder/\""]);
        assert_eq!(app.input.cursor(), (0, 20));
        app.handle(Event::Paste("说明".into()), &mut dirty);
        assert_eq!(app.input.lines(), &["查看 @\"docs/my folder/说明\""]);
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &["查看 @\"docs/my folder/说明.md\" "]);
        assert!(app.menu.is_none());
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn file_picker_nested_filtering_and_reactivation_reset_directory() {
        let (mut app, scans, replies, commands) = file_picker_fixture();
        let mut dirty = false;
        app.handle(Event::Paste("@".into()), &mut dirty);
        scans.try_recv().unwrap();
        replies
            .send(Ok(vec!["src/main.rs".into(), "src/tui/menu.rs".into()]))
            .unwrap();
        app.receive();
        app.handle(key(KeyCode::Tab), &mut dirty);
        app.handle(Event::Paste("menu".into()), &mut dirty);
        assert_eq!(
            menu_values(&app),
            [MenuAction::FileParent, MenuAction::FileDirectory(1)]
        );
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.file_directory, "src/tui/");
        assert_eq!(
            menu_values(&app),
            [MenuAction::FileParent, MenuAction::File(1)]
        );
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert_eq!(app.input.lines(), &["@src/tui/"]);
        app.handle(key(KeyCode::Backspace), &mut dirty);
        scans.try_recv().unwrap();
        assert!(app.file_directory.is_empty());
        replies.send(Ok(vec!["src/tui/menu.rs".into()])).unwrap();
        app.receive();
        assert_eq!(menu_values(&app), [MenuAction::FileDirectory(0)]);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn file_picker_pending_blocks_confirmation_and_escape_ignores_late_reply() {
        let (mut app, scans, replies, commands) = file_picker_fixture();
        let mut dirty = false;
        app.handle(Event::Paste("查看 @".into()), &mut dirty);
        scans.try_recv().unwrap();
        app.handle(key(KeyCode::Enter), &mut dirty);
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &["查看 @"]);
        assert!(commands.try_recv().is_err());
        app.handle(key(KeyCode::Esc), &mut dirty);
        replies.send(Ok(vec!["new.rs".into()])).unwrap();
        app.receive();
        assert!(app.menu.is_none() && app.file_query.is_none());
        assert_eq!(app.input.lines(), &["查看 @"]);
        app.handle(key(KeyCode::Char('n')), &mut dirty);
        scans.try_recv().unwrap();
        replies.send(Ok(vec!["new.rs".into()])).unwrap();
        app.receive();
        assert_eq!(menu_values(&app), [MenuAction::File(0)]);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.handle(control('c'), &mut dirty));
    }

    #[test]
    fn file_picker_cursor_history_and_resize_preserve_draft_and_selection() {
        let (mut app, scans, replies, _) = file_picker_fixture();
        let mut dirty = false;
        app.handle(Event::Paste("查看 @".into()), &mut dirty);
        scans.try_recv().unwrap();
        replies
            .send(Ok(vec!["a.rs".into(), "b.rs".into()]))
            .unwrap();
        app.receive();
        app.handle(key(KeyCode::Down), &mut dirty);
        app.handle(Event::Resize(20, 4), &mut dirty);
        draw(&mut app, 20, 4);
        app.handle(Event::Resize(100, 30), &mut dirty);
        app.refresh_candidates(false);
        assert_eq!(app.menu.as_ref().unwrap().selected_index(), Some(1));
        app.handle(control('a'), &mut dirty);
        assert!(app.file_query.is_none());
        app.handle(control('e'), &mut dirty);
        scans.try_recv().unwrap();
        app.handle(key(KeyCode::Left), &mut dirty);
        assert!(app.file_query.is_none());
        app.handle(key(KeyCode::Right), &mut dirty);
        assert!(app.file_query.is_some());
        assert!(scans.try_recv().is_err());
        app.handle(control('a'), &mut dirty);
        app.input_history.push("历史 @old".into());
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["历史 @old"]);
        assert!(app.file_query.is_none() && app.menu.is_none());
        app.handle(key(KeyCode::Down), &mut dirty);
        assert_eq!(app.input.lines(), &["查看 @"]);
        assert_eq!(app.input.cursor(), (0, 0));
        replies.send(Ok(vec!["a.rs".into()])).unwrap();
        app.receive();
        assert!(app.menu.is_none());
    }

    #[test]
    fn file_picker_empty_results_submit_raw_text_and_errors_preserve_worker() {
        for result in [
            Ok(Vec::new()),
            Ok(vec!["src/other.rs".into()]),
            Err(anyhow::anyhow!("fixture scan failed")),
        ] {
            let (mut app, scans, replies, commands) = file_picker_fixture();
            let mut dirty = false;
            app.handle(Event::Paste("查看 @missing".into()), &mut dirty);
            scans.try_recv().unwrap();
            replies.send(result).unwrap();
            app.receive();
            assert!(app.menu.is_none());
            assert!(scans.try_recv().is_err());
            app.handle(key(KeyCode::Enter), &mut dirty);
            assert!(
                matches!(commands.try_recv(), Ok(WorkerCommand::Run { prompt: text, .. }) if text == "查看 @missing")
            );
        }
        let (mut app, scans, replies, commands) = file_picker_fixture();
        drop(replies);
        let mut dirty = false;
        app.handle(Event::Paste("@".into()), &mut dirty);
        scans.try_recv().unwrap();
        app.receive();
        assert!(matches!(app.state, State::Ready));
        assert_eq!(app.input.lines(), &["@"]);
        assert!(commands.try_recv().is_err());
        assert_notification(&mut app, Color::Rgb(224, 108, 117));
        assert!(draw(&mut app, 100, 30).contains("File picker unavailable; restart pp."));
    }

    #[test]
    fn file_picker_request_disconnect_preserves_draft_and_agent() {
        let (mut app, scans, _replies, commands) = file_picker_fixture();
        drop(scans);
        let mut dirty = false;
        app.handle(Event::Paste("查看 @".into()), &mut dirty);
        assert!(matches!(app.state, State::Ready));
        assert!(!app.files_pending);
        assert_eq!(app.input.lines(), &["查看 @"]);
        assert_notification(&mut app, Color::Rgb(224, 108, 117));
        assert!(draw(&mut app, 100, 30).contains("File picker unavailable; restart pp."));
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(
            matches!(commands.try_recv(), Ok(WorkerCommand::Run { prompt: text, .. }) if text == "查看 @")
        );
    }

    #[test]
    fn file_picker_busy_slash_and_vertical_movement_do_not_reopen_files() {
        let (mut app, scans, replies, commands) = file_picker_fixture();
        let mut dirty = false;
        app.handle(Event::Paste("first\n@".into()), &mut dirty);
        scans.try_recv().unwrap();
        app.handle(key(KeyCode::Up), &mut dirty);
        assert!(app.file_query.is_none());
        replies.send(Ok(vec!["src/main.rs".into()])).unwrap();
        app.receive();
        assert!(app.menu.is_none());
        app.handle(key(KeyCode::Down), &mut dirty);
        scans.try_recv().unwrap();
        app.state = State::Running;
        app.refresh_candidates(false);
        replies.send(Ok(vec!["src/main.rs".into()])).unwrap();
        app.receive();
        assert!(app.menu.is_none() && app.file_query.is_none());
        assert_eq!(app.input.lines(), &["first", "@"]);
        assert!(commands.try_recv().is_err());
        app.state = State::Ready;
        app.clear_input();
        app.handle(Event::Paste("/model @".into()), &mut dirty);
        assert!(app.file_query.is_none());
        assert!(scans.try_recv().is_err());
        app.handle(key(KeyCode::Esc), &mut dirty);
        app.clear_input();
        app.handle(Event::Paste("@".into()), &mut dirty);
        scans.try_recv().unwrap();
        app.handle(function_key('p'), &mut dirty);
        assert!(app.file_query.is_none());
        replies.send(Ok(vec!["src/main.rs".into()])).unwrap();
        app.receive();
        assert_eq!(app.input.lines(), &[""]);
        assert!(app.model_panel.is_some());
        assert!(app.file_query.is_none());
    }

    #[test]
    fn file_picker_stale_action_cannot_replace_another_token() {
        let (mut app, scans, replies, commands) = file_picker_fixture();
        let mut dirty = false;
        app.handle(Event::Paste("@a @b".into()), &mut dirty);
        scans.try_recv().unwrap();
        replies.send(Ok(vec!["b.rs".into()])).unwrap();
        app.receive();
        app.input.move_cursor(CursorMove::Head);
        app.input.move_cursor(CursorMove::Forward);
        app.apply_menu_action(MenuAction::File(0), true);
        assert_eq!(app.input.lines(), &["@a @b"]);
        assert_eq!(app.file_query.as_ref().unwrap().start, 0);
        assert!(commands.try_recv().is_err());
        app.apply_menu_action(MenuAction::File(usize::MAX), true);
        assert_eq!(app.input.lines(), &["@a @b"]);
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn control(character: char) -> Event {
        Event::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::CONTROL,
        ))
    }

    fn function_key(character: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(character), FUNCTION_MODIFIER))
    }

    fn opposite_function_key(character: char) -> Event {
        let modifiers = if cfg!(target_os = "macos") {
            KeyModifiers::CONTROL
        } else {
            KeyModifiers::ALT
        };
        Event::Key(KeyEvent::new(KeyCode::Char(character), modifiers))
    }

    #[test]
    fn input_history_copies_submissions_and_restores_multiline_draft() {
        let mut app = app();
        let (sender, requests) = mpsc::channel();
        app.commands = Some(sender);
        let mut dirty = false;
        for prompt in ["first-中", "second\n🙂末行"] {
            app.state = State::Ready;
            app.handle(Event::Paste(prompt.into()), &mut dirty);
            app.handle(key(KeyCode::Enter), &mut dirty);
            assert!(
                matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt: text, .. }) if text == prompt)
            );
        }
        app.handle(Event::Paste("草稿\n未发送🙂".into()), &mut dirty);
        app.handle(key(KeyCode::Left), &mut dirty);
        let cursor = app.input.cursor();
        app.handle(key(KeyCode::Down), &mut dirty);
        assert_eq!(app.input.cursor(), cursor);
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["草稿", "未发送🙂"]);
        assert_eq!(app.input.cursor(), (0, 2));
        assert!(app.history_index.is_none());
        let cursor = app.input.cursor();
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["second", "🙂末行"]);
        app.handle(key(KeyCode::Char('!')), &mut dirty);
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["second", "🙂末行!"]);
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["first-中"]);
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["first-中"]);
        app.handle(key(KeyCode::Down), &mut dirty);
        assert_eq!(app.input.lines(), &["second", "🙂末行"]);
        app.handle(key(KeyCode::Down), &mut dirty);
        assert_eq!(app.input.lines(), &["草稿", "未发送🙂"]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));

        app.state = State::Ready;
        app.handle(key(KeyCode::Up), &mut dirty);
        app.handle(key(KeyCode::Char('!')), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt: text, .. }) if text == "second\n🙂末行!")
        );
        assert_eq!(
            app.input_history,
            ["first-中", "second\n🙂末行", "second\n🙂末行!"]
        );
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["second", "🙂末行!"]);
        app.handle(key(KeyCode::Down), &mut dirty);
        assert_eq!(app.input.lines(), &[""]);
    }

    #[test]
    fn input_history_ignores_rejected_submissions_and_survives_reset() {
        let mut app = app();
        let (sender, _requests) = mpsc::channel();
        app.commands = Some(sender);
        let mut dirty = false;
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &[""]);
        for (state, text) in [
            (State::Ready, "  "),
            (State::Ready, "/unknown"),
            (State::Running, "/new"),
            (State::Cancelling, "cancelling draft"),
            (State::Unavailable, "unavailable draft"),
        ] {
            app.state = state;
            app.clear_input();
            app.handle(Event::Paste(text.into()), &mut dirty);
            app.handle(key(KeyCode::Enter), &mut dirty);
            assert!(app.input_history.is_empty());
            assert_eq!(app.input.lines(), &[text]);
        }
        app.state = State::Ready;
        app.clear_input();
        app.handle(Event::Paste("/help".into()), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        app.handle(Event::Paste("/new".into()), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        let (events, receiver) = mpsc::channel();
        app.events = Some(receiver);
        app.notify(Level::Error, "stale session feedback".into());
        events.send(WorkerEvent::Reset).unwrap();
        app.receive();
        assert!(app.show_welcome);
        assert!(matches!(app.state, State::Ready));
        assert_eq!(app.input.lines(), &[""]);
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("▄▄▄██▀▀██"));
        assert!(!screen.contains("Session reset."));
        assert!(!screen.contains("stale session feedback"));
        assert!(!screen.contains("╭"));
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["/new"]);
        assert!(app.menu.is_none());
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["/help"]);
        app.handle(key(KeyCode::Down), &mut dirty);
        app.handle(key(KeyCode::Down), &mut dirty);
        assert_eq!(app.input.lines(), &[""]);
    }

    #[test]
    fn runtime_input_pending_recall_protects_draft_and_never_reuses_claimed_text() {
        let (mut app, _requests) = file_commands_app(commands::FileCommands::default());
        app.state = State::Running;
        app.input_queue.begin_run().unwrap();
        let id = app
            .input_queue
            .enqueue(
                PendingKind::FollowUp,
                "/review 原文\n".into(),
                Some("expanded once".into()),
                false,
            )
            .unwrap();
        app.input.insert_str(" ");
        let cursor = app.input.cursor();
        let mut dirty = false;
        app.handle(function_key('q'), &mut dirty);
        assert!(app.queue_menu);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert_eq!(app.input.lines(), &[" "]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(app.has_pending());
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert!(app.has_pending());
        app.input = editor();
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert_eq!(app.input.lines(), &["/review 原文", ""]);
        assert!(!app.queue_menu);
        assert!(!app.has_pending());
        assert!(app.input_history.is_empty());
        assert!(app.input_queue.recall(id).unwrap().is_none());
        app.input = editor();
        let id = app
            .input_queue
            .enqueue(PendingKind::Steering, "claimed".into(), None, false)
            .unwrap();
        app.toggle_pending_menu();
        let claimed = app.input_queue.claim_steering(false).unwrap();
        app.recall_pending(id);
        assert!(app.input.is_empty());
        assert!(!app.queue_menu);
        assert_eq!(claimed.front().unwrap().source, "claimed");
    }

    #[test]
    fn runtime_input_pending_menu_preserves_selection_and_escape_does_not_cancel() {
        let (mut app, _requests) = file_commands_app(commands::FileCommands::default());
        app.state = State::Running;
        let token = CancellationToken::new();
        app.active_cancellation = Some(token.clone());
        for text in ["first", "界\u{1b}[31m\tpreview", "last"] {
            app.input_queue
                .enqueue(PendingKind::FollowUp, text.into(), None, false)
                .unwrap();
        }
        let mut dirty = false;
        app.handle(opposite_function_key('q'), &mut dirty);
        assert!(!app.queue_menu);
        app.handle(function_key('q'), &mut dirty);
        app.handle(key(KeyCode::Down), &mut dirty);
        let selected = app.menu.as_ref().unwrap().items()[1].value;
        app.input_queue
            .enqueue(PendingKind::Steering, "priority".into(), None, false)
            .unwrap();
        app.receive();
        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.items()[menu.selected_index().unwrap()].value, selected);
        for (width, height) in [(100, 30), (40, 10)] {
            let screen = draw(&mut app, width, height);
            assert!(screen.contains("Steer #"));
            assert!(!screen.contains('\u{1b}'));
        }
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(!app.queue_menu);
        assert!(!token.is_cancelled());
        assert!(!app.queue_paused);
        app.handle(function_key('q'), &mut dirty);
        app.handle(Event::Paste("新草稿".into()), &mut dirty);
        assert!(!app.queue_menu);
        assert_eq!(app.input.lines(), &["新草稿"]);
        assert!(!token.is_cancelled());
    }

    #[test]
    fn runtime_input_explicit_resume_preserves_draft_and_only_ready_dispatches() {
        let (mut app, requests) = file_commands_app(commands::FileCommands::default());
        app.input_queue
            .enqueue(PendingKind::FollowUp, "next task".into(), None, false)
            .unwrap();
        app.queue_paused = true;
        app.state = State::Ready;
        let mut dirty = false;
        app.handle(opposite_function_key('r'), &mut dirty);
        assert!(app.queue_paused);
        assert!(requests.try_recv().is_err());
        app.input.insert_str("草稿");
        let cursor = app.input.cursor();
        for state in [State::Running, State::Cancelling, State::Unavailable] {
            app.state = state;
            app.handle(function_key('r'), &mut dirty);
            assert!(app.queue_paused);
            assert!(requests.try_recv().is_err());
        }
        app.state = State::Ready;
        app.handle(function_key('q'), &mut dirty);
        app.handle(function_key('r'), &mut dirty);
        assert!(!app.queue_paused);
        assert!(!app.queue_menu);
        assert!(
            matches!(requests.try_recv().unwrap(), WorkerCommand::Run { prompt, .. } if prompt == "next task")
        );
        assert_eq!(app.input.lines(), &["草稿"]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(app.input_history.is_empty());
    }

    #[test]
    fn runtime_input_scheduler_prioritizes_steering_and_pauses_on_cancel_race() {
        let (mut app, requests) = file_commands_app(commands::FileCommands::default());
        let (events, receiver) = mpsc::channel();
        app.events = Some(receiver);
        let mut dirty = false;
        app.input.insert_str("initial");
        app.handle(key(KeyCode::Enter), &mut dirty);
        let WorkerCommand::Run {
            cancellation: initial,
            ..
        } = requests.try_recv().unwrap()
        else {
            panic!()
        };
        app.input.insert_str("queued");
        app.handle(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            &mut dirty,
        );
        app.input.insert_str("steer");
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(app.input.is_empty());
        assert_eq!(app.input_history, ["initial", "queued", "steer"]);
        assert!(requests.try_recv().is_err());
        app.input.insert_str("草稿\nline");
        let cursor = app.input.cursor();
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(initial.is_cancelled());
        // A committed final reply can win cancellation, but must not resume the queue.
        events
            .send(WorkerEvent::completed(
                Instant::now(),
                Ok(RunOutcome::Completed),
            ))
            .unwrap();
        app.receive();
        assert!(matches!(app.state, State::Ready));
        assert!(app.queue_paused);
        assert!(requests.try_recv().is_err());
        assert_eq!(app.input.cursor(), cursor);
        assert_eq!(app.input.lines(), &["草稿", "line"]);
        app.queue_paused = false;
        app.dispatch_next(true);
        let WorkerCommand::Run {
            prompt,
            cancellation: next,
            ..
        } = requests.try_recv().unwrap()
        else {
            panic!()
        };
        assert_eq!(prompt, "steer");
        assert!(!next.is_cancelled());
        assert_eq!(app.input.cursor(), cursor);
        events
            .send(WorkerEvent::completed(
                Instant::now(),
                Ok(RunOutcome::Completed),
            ))
            .unwrap();
        app.receive();
        assert!(
            matches!(requests.try_recv().unwrap(), WorkerCommand::Run { prompt, .. } if prompt == "queued")
        );
        assert_eq!(app.input.lines(), &["草稿", "line"]);
        assert_eq!(app.input_history, ["initial", "queued", "steer"]);
    }

    #[test]
    fn runtime_input_modified_and_repeated_enter_preserve_draft_and_history_guard() {
        let (mut app, requests) = file_commands_app(commands::FileCommands::default());
        app.state = State::Running;
        let mut dirty = false;
        app.input.insert_str("one");
        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::SHIFT,
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        ] {
            app.handle(
                Event::Key(KeyEvent::new(KeyCode::Enter, modifiers)),
                &mut dirty,
            );
        }
        app.handle(
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )),
            &mut dirty,
        );
        assert_eq!(app.input.lines(), &["one"]);
        assert!(app.input_history.is_empty());
        app.handle(key(KeyCode::Enter), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert_eq!(app.input_history, ["one"]);
        assert_eq!(
            app.input_queue
                .snapshot_since(None)
                .unwrap()
                .unwrap()
                .steering(),
            1
        );
        app.state = State::Ready;
        app.queue_paused = true;
        for command in ["/new", "/resume", "/tree"] {
            app.input = editor();
            app.input.insert_str(command);
            let cursor = app.input.cursor();
            app.submit(PendingKind::Steering);
            assert_eq!(app.input.lines(), &[command]);
            assert_eq!(app.input.cursor(), cursor);
            assert!(matches!(app.state, State::Ready));
            assert!(requests.try_recv().is_err());
        }
        app.input = editor();
        app.handle(key(KeyCode::Esc), &mut dirty);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.tree_panel.is_none());
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn input_arrows_move_between_wrapped_lines_before_history() {
        let mut app = app();
        app.input_history.push("history".into());
        let text = "abcdefghijklmnopqrstuvwxyz0123456789!";
        let mut dirty = false;
        app.handle(Event::Paste(text.into()), &mut dirty);
        draw(&mut app, 14, 12);
        let cursor = app.input.cursor();
        let row = app.input.screen_cursor().row;
        assert!(row > 0);

        app.handle(key(KeyCode::Up), &mut dirty);
        draw(&mut app, 14, 12);
        assert_eq!(app.input.screen_cursor().row, row - 1);
        assert!(app.history_index.is_none());
        app.handle(key(KeyCode::Down), &mut dirty);
        draw(&mut app, 14, 12);
        assert_eq!(app.input.cursor(), cursor);
        assert_eq!(app.input.lines(), &[text]);
        assert!(app.history_index.is_none());
    }

    #[test]
    fn input_native_cursor_follows_unicode_wrapping_scrolling_and_resize() {
        for (text, width, height, expected) in [
            ("", 100, 30, (2, 27)),
            ("中🙂h", 100, 30, (7, 27)),
            ("first\n中🙂h", 100, 30, (7, 27)),
            ("0\n1\n2\n3\n4\n5\n中🙂h", 100, 30, (7, 27)),
        ] {
            let mut app = app();
            app.input.insert_str(text);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, &StatusContext::default()))
                .unwrap();
            assert!(terminal.backend().cursor_visible());
            assert_eq!(terminal.backend().cursor_position(), expected.into());
            assert!(
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .all(|cell| !cell.modifier.contains(Modifier::REVERSED))
            );
        }

        let mut app = app();
        app.input.insert_str("abcdefghijklmnopqrstuvwxyz");
        for (width, column, top) in [(14, 4, 2), (40, 28, 0), (9, 7, 3)] {
            let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
            terminal
                .draw(|frame| app.draw(frame, &StatusContext::default()))
                .unwrap();
            assert_eq!(terminal.backend().cursor_position(), (column, 9).into());
            assert_eq!(app.input.scroll_offset(), (top, 0));
        }
    }

    #[test]
    fn input_control_keys_respect_unicode_and_line_boundaries() {
        let mut app = app();
        let mut dirty = false;
        app.handle(Event::Paste("first\n中🙂abc\nlast".into()), &mut dirty);
        app.input.move_cursor(CursorMove::Jump(1, 2));
        app.handle(control('a'), &mut dirty);
        app.handle(control('a'), &mut dirty);
        assert_eq!(app.input.cursor(), (1, 0));
        app.handle(control('e'), &mut dirty);
        app.handle(control('e'), &mut dirty);
        assert_eq!(app.input.cursor(), (1, 5));
        app.handle(control('k'), &mut dirty);
        assert_eq!(app.input.lines(), &["first", "中🙂abc", "last"]);
        app.input.move_cursor(CursorMove::Jump(1, 2));
        app.handle(control('k'), &mut dirty);
        assert_eq!(app.input.lines(), &["first", "中🙂", "last"]);
        assert_eq!(app.input.cursor(), (1, 2));
        app.handle(control('a'), &mut dirty);
        app.handle(control('k'), &mut dirty);
        assert_eq!(app.input.lines(), &["first", "", "last"]);
        app.handle(control('k'), &mut dirty);
        assert_eq!(app.input.lines(), &["first", "", "last"]);
    }

    #[test]
    fn input_history_yields_to_menus_and_control_edits_refresh_candidates() {
        let mut app = app();
        app.input_history.push("older task".into());
        let mut dirty = false;
        app.handle(Event::Paste("/".into()), &mut dirty);
        app.handle(key(KeyCode::Down), &mut dirty);
        assert_eq!(app.menu.as_ref().unwrap().selected_index(), Some(1));
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["/"]);
        assert!(app.history_index.is_none());
        app.handle(control('a'), &mut dirty);
        assert_eq!(app.input.cursor(), (0, 0));
        app.handle(control('e'), &mut dirty);
        assert_eq!(app.input.cursor(), (0, 1));
        app.handle(control('a'), &mut dirty);
        app.handle(control('k'), &mut dirty);
        assert_eq!(app.input.lines(), &[""]);
        assert!(app.menu.is_none());
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["older task"]);
    }

    #[test]
    fn model_space_menu_renders_above_input_and_preserves_selection_on_resize() {
        let mut app = app();
        app.state = State::Ready;
        app.choices = (0..25)
            .map(|index| ModelChoice {
                reference: format!("fixture/model-{index}"),
                model: format!("vendor-{index}"),
                protocol: crate::models::Protocol::Response,
                thinking: crate::models::ModelThinking::default(),
                context_window: None,
            })
            .collect();
        app.current_model = Some(app.choices[2].clone());
        let (sender, requests) = mpsc::channel();
        app.commands = Some(sender);
        let (events, receiver) = mpsc::channel();
        app.events = Some(receiver);
        let mut dirty = false;
        app.input.insert_str("/model");
        app.refresh_candidates(true);
        let model_command = commands::COMMANDS
            .iter()
            .position(|spec| spec.name == "/model")
            .unwrap();
        assert_eq!(menu_values(&app), vec![MenuAction::Command(model_command)]);
        app.handle(key(KeyCode::Char(' ')), &mut dirty);
        let screen = draw(&mut app, 100, 30);
        assert!(screen.find(" ").unwrap() < screen.find("fixture/model-0").unwrap());
        assert!(!screen.contains("❯ /model"));
        assert!(screen.contains("❯ fixture/model-2"));
        assert!(
            screen.replace(' ', "").contains("fixture/model-2"),
            "{screen}"
        );
        assert!(screen.lines().last().unwrap().contains("fixture/model-2"));
        assert!(!screen.contains("fixture/vendor-2"), "{screen}");
        assert!(!screen.contains('┌'));
        for width in [100, 180, 60] {
            let buffer = draw_buffer(&mut app, width, 30);
            let y = (0..30)
                .find(|y| {
                    (0..width)
                        .map(|x| buffer[(x, *y)].symbol())
                        .collect::<String>()
                        .contains(" favorite ·  default ·  current")
                })
                .unwrap();
            for (icon, color) in [
                ("", Color::Rgb(254, 188, 56)),
                ("", Color::Rgb(187, 154, 247)),
                ("", Color::Rgb(158, 206, 106)),
            ] {
                let x = (0..width)
                    .find(|x| buffer[(*x, y)].symbol() == icon)
                    .unwrap();
                assert_eq!(buffer[(x, y)].fg, color);
            }
        }
        draw(&mut app, 100, 30);
        assert_eq!(app.model_panel.as_ref().unwrap().height(30), 15);
        app.handle(key(KeyCode::PageDown), &mut dirty);
        assert_eq!(active_menu(&app).selected_index(), Some(13));
        for (width, height) in [(20, 6), (1, 1), (100, 30)] {
            draw(&mut app, width, height);
            assert_eq!(app.input.lines(), &[""]);
            assert_eq!(active_menu(&app).selected_index(), Some(13));
        }
        app.handle(key(KeyCode::End), &mut dirty);
        assert!(draw(&mut app, 100, 30).contains("❯ fixture/model-24"));
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::SwitchModel(reference)) if reference == "fixture/model-24")
        );
        assert_eq!(
            app.current_model.as_ref().unwrap().reference,
            "fixture/model-2"
        );
        assert!(app.menu.is_none());
        app.input.insert_str("后续草稿");
        events
            .send(WorkerEvent::ModelSwitched(Ok((
                app.choices[24].clone(),
                app.think_level,
            ))))
            .unwrap();
        app.receive();
        assert_eq!(
            app.current_model.as_ref().unwrap().reference,
            "fixture/model-24"
        );
        assert_eq!(app.input.lines(), &["后续草稿"]);
    }

    fn favorite_model_choices() -> Vec<ModelChoice> {
        ["Alpha", "Beta", "Gamma", "Delta"]
            .into_iter()
            .map(|name| ModelChoice {
                reference: format!("gateway/team/{name}"),
                model: format!("vendor-{name}"),
                protocol: crate::models::Protocol::Completion,
                thinking: crate::models::ModelThinking::default(),
                context_window: None,
            })
            .collect()
    }

    #[test]
    fn model_panel_search_is_independent_single_line_and_never_submits_tasks() {
        let (mut app, requests, _events) = plan_app();
        app.choices = favorite_model_choices();
        let mut dirty = false;
        app.handle(Event::Paste("/model".into()), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert_eq!(app.input.lines(), &[""]);
        assert_eq!(model_query(&app), "");
        app.handle(
            Event::Paste("模型🙂\nsecond\r\nthird\rfourth".into()),
            &mut dirty,
        );
        assert_eq!(model_query(&app), "模型🙂 second third fourth");
        app.handle(control('a'), &mut dirty);
        app.handle(control('k'), &mut dirty);
        app.handle(control('j'), &mut dirty);
        app.handle(key(KeyCode::BackTab), &mut dirty);
        assert_eq!(model_query(&app), "");
        let query = "模型🙂".repeat(30);
        app.handle(Event::Paste(query.clone()), &mut dirty);
        let mut terminal = Terminal::new(TestBackend::new(20, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, &StatusContext::default()))
            .unwrap();
        let cursor = terminal.backend().cursor_position();
        assert!(cursor.x < 20 && cursor.y < 8);
        assert_eq!(model_query(&app), query);
        app.handle(control('a'), &mut dirty);
        app.handle(control('k'), &mut dirty);
        app.handle(Event::Paste("aMm".into()), &mut dirty);
        assert_eq!(menu_values(&app), [MenuAction::Model(2)]);
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(model_query(&app), "gateway/team/Gamma");
        assert_eq!(app.input.lines(), &[""]);
        assert!(requests.try_recv().is_err());
        assert!(app.input_history.is_empty());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.model_panel.is_none());
        assert_eq!(app.input.lines(), &[""]);
    }

    #[test]
    fn model_panel_caps_height_and_routes_wheel_without_scrolling_conversation() {
        let (mut app, requests, _events) = plan_app();
        app.show_welcome = false;
        app.choices = (0..25)
            .map(|index| ModelChoice {
                reference: format!("fixture/model-{index:02}"),
                model: format!("vendor-{index:02}"),
                protocol: crate::models::Protocol::Response,
                thinking: crate::models::ModelThinking::default(),
                context_window: None,
            })
            .collect();
        for index in 0..40 {
            app.append("Assistant", &format!("history-{index:02}"));
        }
        let mut dirty = false;
        app.handle(Event::Paste("/model ".into()), &mut dirty);
        let before = draw_buffer(&mut app, 100, 30);
        assert_eq!(app.model_panel.as_ref().unwrap().height(30), 15);
        let model_rows = |buffer: &ratatui::buffer::Buffer| -> Vec<u16> {
            (0..30)
                .filter(|y| {
                    (0..99)
                        .map(|x| buffer[(x, *y)].symbol())
                        .collect::<String>()
                        .contains("fixture/model-")
                })
                .collect()
        };
        assert_eq!(model_rows(&before), (14..25).collect::<Vec<_>>());
        assert_eq!(before[(99, 14)].symbol(), "█");
        let wheel = |kind, row| {
            Event::Mouse(MouseEvent {
                kind,
                column: 50,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.handle(wheel(MouseEventKind::ScrollDown, 15), &mut dirty);
        assert_eq!(active_menu(&app).selected_index(), Some(3));
        app.handle(key(KeyCode::End), &mut dirty);
        let end = draw_buffer(&mut app, 100, 30);
        assert_eq!(model_rows(&end).len(), 11);
        assert_eq!(end[(99, 24)].symbol(), "█");
        for y in (0..11).chain(26..30) {
            for x in 0..100 {
                assert_eq!(before[(x, y)], end[(x, y)]);
            }
        }
        app.handle(wheel(MouseEventKind::ScrollDown, 15), &mut dirty);
        assert_eq!(active_menu(&app).selected_index(), Some(24));
        app.handle(wheel(MouseEventKind::ScrollUp, 15), &mut dirty);
        assert_eq!(active_menu(&app).selected_index(), Some(21));
        app.handle(wheel(MouseEventKind::ScrollUp, 0), &mut dirty);
        let scrolled = draw_buffer(&mut app, 100, 30);
        assert_eq!(active_menu(&app).selected_index(), Some(21));
        assert!((0..11).any(|y| (0..100).any(|x| before[(x, y)] != scrolled[(x, y)])));
        app.handle(key(KeyCode::Home), &mut dirty);
        app.handle(wheel(MouseEventKind::ScrollUp, 15), &mut dirty);
        assert_eq!(active_menu(&app).selected_index(), Some(0));
        app.handle(Event::Paste("model-24".into()), &mut dirty);
        assert_eq!(app.model_panel.as_ref().unwrap().height(30), 15);
        let filtered = draw_buffer(&mut app, 100, 30);
        assert_eq!(filtered[(0, 12)].symbol(), before[(0, 12)].symbol());
        assert_eq!(model_rows(&filtered), [14]);
        app.handle(control('a'), &mut dirty);
        app.handle(control('k'), &mut dirty);
        app.handle(Event::Paste("missing".into()), &mut dirty);
        assert_eq!(app.model_panel.as_ref().unwrap().height(30), 15);
        assert!(
            draw(&mut app, 100, 30)
                .lines()
                .nth(14)
                .unwrap()
                .contains("No matching models")
        );
        app.handle(control('a'), &mut dirty);
        app.handle(control('k'), &mut dirty);
        assert_eq!(model_rows(&draw_buffer(&mut app, 100, 30)).len(), 11);
        app.handle(Event::Paste("model-24".into()), &mut dirty);
        for (width, height) in [(20, 6), (1, 1), (1, 8), (100, 30)] {
            draw(&mut app, width, height);
            assert_eq!(model_query(&app), "model-24");
            assert_eq!(active_menu(&app).items()[0].value, MenuAction::Model(24));
        }
        assert!(draw(&mut app, 100, 30).contains(" model-24"));
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn model_favorites_commit_before_reorder_preserve_focus_and_reject_repeats() {
        let (mut app, requests, events) = plan_app();
        app.choices = favorite_model_choices();
        app.current_model = Some(app.choices[0].clone());
        app.default_model_reference = Some(app.choices[0].reference.clone());
        app.favorite_models = vec![
            app.choices[3].reference.clone(),
            app.choices[1].reference.clone(),
        ];
        let mut dirty = false;
        app.input.insert_str("/model ");
        app.refresh_candidates(true);
        assert_eq!(
            menu_values(&app),
            vec![
                MenuAction::Model(1),
                MenuAction::Model(3),
                MenuAction::Model(0),
                MenuAction::Model(2)
            ]
        );
        app.handle(key(KeyCode::End), &mut dirty);
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            app.handle(
                Event::Key(KeyEvent {
                    kind,
                    ..KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
                }),
                &mut dirty,
            );
        }
        assert!(requests.try_recv().is_err());
        app.handle(control('f'), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::ToggleFavoriteModel(reference)) if reference == app.choices[2].reference)
        );
        assert_eq!(app.favorite_models.len(), 2);
        assert_eq!(menu_values(&app).last(), Some(&MenuAction::Model(2)));
        for event in [control('f'), control('s'), key(KeyCode::Enter)] {
            app.handle(event, &mut dirty);
        }
        assert!(requests.try_recv().is_err());
        events
            .send(WorkerEvent::FavoriteModelsSaved(Ok(vec![
                app.choices[1].reference.clone(),
                app.choices[2].reference.clone(),
                app.choices[3].reference.clone(),
            ])))
            .unwrap();
        app.receive();
        assert_eq!(
            menu_values(&app),
            vec![
                MenuAction::Model(1),
                MenuAction::Model(2),
                MenuAction::Model(3),
                MenuAction::Model(0)
            ]
        );
        assert_eq!(active_menu(&app).selected_index(), Some(1));
        assert!(draw(&mut app, 130, 30).contains("❯ gateway/team/Gamma "));
        app.handle(control('f'), &mut dirty);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::ToggleFavoriteModel(_))
        ));
        events
            .send(WorkerEvent::FavoriteModelsSaved(Err(anyhow::anyhow!(
                "fixture save failure"
            ))))
            .unwrap();
        app.receive();
        assert_eq!(app.favorite_models.len(), 3);
        assert_eq!(active_menu(&app).selected_index(), Some(1));
        assert!(draw(&mut app, 130, 30).contains("fixture save failure"));
        app.handle(control('f'), &mut dirty);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::ToggleFavoriteModel(_))
        ));
        events
            .send(WorkerEvent::FavoriteModelsSaved(Ok(vec![
                app.choices[1].reference.clone(),
                app.choices[3].reference.clone(),
            ])))
            .unwrap();
        app.receive();
        assert_eq!(active_menu(&app).selected_index(), Some(3));
        assert_eq!(
            app.current_model.as_ref().unwrap().reference,
            app.choices[0].reference
        );
        assert_eq!(
            app.default_model_reference.as_deref(),
            Some(app.choices[0].reference.as_str())
        );
    }

    #[test]
    fn model_search_matches_substrings_case_insensitively_and_handles_no_results() {
        let (mut app, requests, _events) = plan_app();
        app.choices = favorite_model_choices();
        app.favorite_models = vec![app.choices[2].reference.clone()];
        let mut dirty = false;
        for (query, expected) in [
            ("  WaY/TeAm  ", vec![2, 0, 1, 3]),
            ("ENDOR-gA", vec![2]),
            ("eTa", vec![1]),
            ("", vec![2, 0, 1, 3]),
            ("missing", vec![]),
        ] {
            app.close_model_panel();
            app.input = editor();
            app.handle(Event::Paste(format!("/model {query}")), &mut dirty);
            assert_eq!(app.model_panel.as_ref().unwrap().height(30), 8);
            assert_eq!(
                menu_values(&app),
                expected
                    .into_iter()
                    .map(MenuAction::Model)
                    .collect::<Vec<_>>()
            );
        }
        assert!(draw(&mut app, 100, 30).contains("No matching models"));
        for event in [
            key(KeyCode::Enter),
            key(KeyCode::Tab),
            control('f'),
            control('s'),
        ] {
            app.handle(event, &mut dirty);
        }
        assert!(requests.try_recv().is_err());
        assert_eq!(app.input.lines(), &[""]);
        assert_eq!(model_query(&app), "missing");
        for _ in 0..7 {
            app.handle(key(KeyCode::Backspace), &mut dirty);
        }
        assert_eq!(menu_values(&app).len(), 4);
        app.handle(Event::Paste("aMm".into()), &mut dirty);
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &[""]);
        assert_eq!(model_query(&app), "gateway/team/Gamma");
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::SwitchModel(reference)) if reference == "gateway/team/Gamma")
        );
    }

    #[test]
    fn model_menu_filters_completes_and_cancels_without_requests() {
        let mut app = app();
        app.state = State::Ready;
        app.choices = vec![ModelChoice {
            reference: "gateway/team/model with spaces".into(),
            model: "vendor-one".into(),
            protocol: crate::models::Protocol::Completion,
            thinking: crate::models::ModelThinking::default(),
            context_window: None,
        }];
        let mut dirty = false;
        app.handle(Event::Paste("/model vendor".into()), &mut dirty);
        assert_eq!(menu_values(&app), vec![MenuAction::Model(0)]);
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &[""]);
        assert_eq!(model_query(&app), "gateway/team/model with spaces");
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.model_panel.is_none());
        app.refresh_candidates(false);
        assert!(app.menu.is_none());
        app.handle(key(KeyCode::Backspace), &mut dirty);
        assert!(app.model_panel.is_none());
        app.input = editor();
        app.input.insert_str("/model missing");
        app.refresh_candidates(true);
        assert!(active_menu(&app).items().is_empty());
        assert!(draw(&mut app, 100, 30).contains("No matching models"));
        app.close_model_panel();
        app.input = editor();
        app.input.insert_str("/model ");
        for state in [State::Running, State::SwitchingModel, State::Unavailable] {
            app.state = state;
            app.refresh_candidates(true);
            assert!(app.menu.is_none());
            assert!(app.model_panel.is_none());
        }
    }

    #[test]
    fn model_menu_prefers_exact_reference_over_current_prefix_match() {
        let mut app = app();
        app.state = State::Ready;
        app.choices = ["m", "m2"]
            .into_iter()
            .map(|id| ModelChoice {
                reference: format!("a/{id}"),
                model: id.into(),
                protocol: crate::models::Protocol::Completion,
                thinking: crate::models::ModelThinking::default(),
                context_window: None,
            })
            .collect();
        app.current_model = Some(app.choices[1].clone());
        app.default_model_reference = Some(app.choices[0].reference.clone());
        let mut dirty = false;
        app.handle(Event::Paste("/model a/".into()), &mut dirty);
        assert_eq!(active_menu(&app).selected_index(), Some(1));
        app.handle(key(KeyCode::Char('m')), &mut dirty);
        assert_eq!(
            menu_values(&app),
            vec![MenuAction::Model(0), MenuAction::Model(1)]
        );
        assert_eq!(active_menu(&app).selected_index(), Some(0));
    }

    #[test]
    fn menu_overlay_preserves_editor_and_history_when_candidates_change() {
        for (width, height) in [(100, 30), (40, 12), (20, 6)] {
            for draft in ["/", "first line\n@"] {
                for scrolled in [false, true] {
                    let mut app = app();
                    app.show_welcome = false;
                    for index in 0..40 {
                        app.append("Assistant", &format!("history-{index:02}"));
                    }
                    app.input.insert_str(draft);
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    let mut render = |app: &mut App| {
                        terminal
                            .draw(|frame| app.draw(frame, &StatusContext::default()))
                            .unwrap();
                        (
                            terminal.backend().buffer().clone(),
                            terminal.backend().cursor_position(),
                        )
                    };
                    render(&mut app);
                    if scrolled {
                        app.conversation.page_up();
                    }
                    let (baseline, cursor) = render(&mut app);
                    let input_top = height - 1 - (app.input.lines().len() as u16 + 2);
                    for count in [12, 1, 3, 12] {
                        app.menu = Some(
                            SelectionMenu::new(
                                (0..count)
                                    .map(|index| MenuItem {
                                        value: MenuAction::Command(index),
                                        label: format!("/candidate-{index}"),
                                        description: "Candidate description".into(),
                                        icon: None,
                                        current: false,
                                    })
                                    .collect(),
                                count - 1,
                            )
                            .with_hint("Enter select · Esc close"),
                        );
                        let menu_top = input_top - app.menu.as_ref().unwrap().height(input_top);
                        let (buffer, position) = render(&mut app);
                        assert_eq!(position, cursor);
                        // 覆盖区之外的对话、输入正文和状态栏必须逐格保持原位。
                        for y in (0..menu_top).chain(input_top + 1..height) {
                            for x in 0..width {
                                if y == height - 2 {
                                    assert_eq!(buffer[(x, y)].symbol(), baseline[(x, y)].symbol());
                                } else {
                                    assert_eq!(buffer[(x, y)], baseline[(x, y)]);
                                }
                            }
                        }
                    }
                    app.menu = None;
                    assert_eq!(render(&mut app), (baseline, cursor));
                }
            }
        }
    }

    #[test]
    fn command_menu_renders_above_input_with_columns_and_blue_selection() {
        let mut app = app();
        app.input.insert_str("/");
        app.refresh_candidates(true);
        let mut dirty = false;
        app.handle(key(KeyCode::Down), &mut dirty);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, &StatusContext::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let input_row = 27;
        assert_eq!(buffer[(0, input_row)].symbol(), "❯");
        assert_eq!(buffer[(2, input_row)].symbol(), "/");
        assert!(!buffer[(3, input_row)].modifier.contains(Modifier::REVERSED));
        assert_eq!(terminal.backend().cursor_position(), (3, input_row).into());
        let separator_row = input_row - 1;
        for x in 0..100 {
            assert_eq!(buffer[(x, separator_row)].symbol(), "─");
            assert_eq!(buffer[(x, separator_row)].fg, menu::SEPARATOR);
        }
        let menu_top = separator_row - 8;
        for (index, spec) in COMMANDS.iter().take(8).enumerate() {
            let y = menu_top + index as u16;
            let active = index == 1;
            assert_eq!(buffer[(0, y)].symbol(), if active { "❯" } else { " " });
            assert_eq!(buffer[(2, y)].symbol(), "/");
            assert_eq!(buffer[(3, y)].symbol(), &spec.name[1..2]);
            assert_eq!(
                buffer[(3, y)].fg,
                if active {
                    menu::SELECTED
                } else {
                    Color::DarkGray
                }
            );
            assert!(!buffer[(4, y)].modifier.contains(Modifier::REVERSED));
        }
        assert_eq!(app.input.lines(), &["/"]);
        app.handle(key(KeyCode::End), &mut dirty);
        app.handle(key(KeyCode::Down), &mut dirty);
        app.handle(key(KeyCode::Down), &mut dirty);
        app.handle(key(KeyCode::Down), &mut dirty);
        for (width, height) in [(20, 6), (1, 1), (2, 2), (100, 30)] {
            let screen = draw(&mut app, width, height);
            if height == 6 || height == 30 {
                assert!(screen.contains("quit"));
            }
            assert_eq!(app.input.lines(), &["/"]);
        }
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &["/quit"]);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.menu.is_none());
        app.refresh_candidates(false);
        assert!(app.menu.is_none());
    }

    #[test]
    fn shortcuts_cycle_all_levels_after_ack_without_recording_or_moving_draft() {
        let mut app = app();
        app.state = State::Ready;
        let (sender, requests) = mpsc::channel();
        app.commands = Some(sender);
        let (events, receiver) = mpsc::channel();
        app.events = Some(receiver);
        app.input.insert_str("草稿\n未发送🙂");
        app.input_history.push("已提交".into());
        app.navigate_history(true);
        let cursor = app.input.cursor();
        let mut dirty = false;
        let encodings = [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        ];
        for (index, (level, label)) in [
            (ThinkLevel::Low, "low"),
            (ThinkLevel::Medium, "med"),
            (ThinkLevel::High, "hig"),
            (ThinkLevel::Xhigh, "xhi"),
            (ThinkLevel::Max, "max"),
            (ThinkLevel::None, "off"),
        ]
        .into_iter()
        .enumerate()
        {
            let key = encodings[index % encodings.len()];
            let previous = app.think_level;
            app.handle(Event::Key(key), &mut dirty);
            assert!(
                matches!(requests.try_recv(), Ok(WorkerCommand::SetThinkLevel(value)) if value == level)
            );
            assert_eq!(app.think_level, previous);
            let mut repeat = key;
            repeat.kind = KeyEventKind::Repeat;
            app.handle(Event::Key(repeat), &mut dirty);
            app.handle(Event::Key(key), &mut dirty);
            assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
            events.send(WorkerEvent::ThinkLevelSet(Ok(level))).unwrap();
            app.receive();
            assert_eq!(app.think_level, level);
            let screen = draw(&mut app, 100, 30);
            assert!(
                screen
                    .lines()
                    .last()
                    .unwrap()
                    .contains(&format!(" {label} ·"))
            );
            assert!(!screen.contains("Thinking level:"));
            assert!(!screen.contains("╭"));
            assert_eq!(app.input.lines(), &["已提交"]);
            assert_eq!(app.input.cursor(), cursor);
            assert_eq!(app.history_index, Some(0));
            assert_eq!(app.input_history, ["已提交"]);
        }
        app.navigate_history(false);
        assert_eq!(app.input.lines(), &["草稿", "未发送🙂"]);
    }

    #[test]
    fn shortcuts_model_picker_restores_history_draft_on_cancel_failure_and_success() {
        let mut app = app();
        app.state = State::Ready;
        app.choices = ["first", "second"]
            .map(|id| ModelChoice {
                reference: format!("fixture/{id}"),
                model: id.into(),
                protocol: crate::models::Protocol::Completion,
                thinking: crate::models::ModelThinking::default(),
                context_window: None,
            })
            .to_vec();
        app.current_model = Some(app.choices[0].clone());
        let (sender, requests) = mpsc::channel();
        app.commands = Some(sender);
        let (events, receiver) = mpsc::channel();
        app.events = Some(receiver);
        app.input.insert_str("草稿\n🙂结尾");
        app.input.move_cursor(CursorMove::Back);
        let cursor = app.input.cursor();
        app.input_history.push("旧任务".into());
        app.navigate_history(true);
        let mut dirty = false;
        app.handle(function_key('p'), &mut dirty);
        assert_eq!(app.input.lines(), &[""]);
        app.handle(function_key('p'), &mut dirty);
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &[""]);
        assert_eq!(model_query(&app), "fixture/first");
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert_eq!(app.input.lines(), &["旧任务"]);
        assert_eq!(app.history_index, Some(0));
        app.navigate_history(false);
        assert_eq!(app.input.lines(), &["草稿", "🙂结尾"]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));

        app.handle(function_key('p'), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert_eq!(app.input.lines(), &["草稿", "🙂结尾"]);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        for succeeds in [false, true] {
            app.handle(function_key('p'), &mut dirty);
            app.handle(key(KeyCode::Down), &mut dirty);
            app.handle(key(KeyCode::Enter), &mut dirty);
            assert!(
                matches!(requests.try_recv(), Ok(WorkerCommand::SwitchModel(reference)) if reference == "fixture/second")
            );
            assert_eq!(app.input.lines(), &["草稿", "🙂结尾"]);
            assert_eq!(app.input.cursor(), cursor);
            assert!(app.model_picker_draft.is_none());
            let result = if succeeds {
                Ok((app.choices[1].clone(), app.think_level))
            } else {
                Err(anyhow::anyhow!("fixture switch failure"))
            };
            events.send(WorkerEvent::ModelSwitched(result)).unwrap();
            app.receive();
            assert_eq!(
                app.current_model.as_ref().unwrap().model,
                if succeeds { "second" } else { "first" }
            );
            assert_eq!(app.input.lines(), &["草稿", "🙂结尾"]);
            assert_eq!(app.input_history, ["旧任务"]);
            assert_eq!(app.input.cursor(), cursor);
            if !succeeds {
                assert_notification(&mut app, Color::Rgb(224, 108, 117));
                assert!(draw(&mut app, 100, 30).contains("fixture switch failure"));
                app.notifications
                    .tick(Instant::now() + Duration::from_millis(10000));
            }
            let screen = draw(&mut app, 100, 30);
            assert!(!screen.contains("Current model:"));
            assert!(!screen.contains("fixture switch failure"));
            assert!(!screen.contains("╭"));
            assert!(screen.lines().last().unwrap().contains(if succeeds {
                "second"
            } else {
                "first"
            }));
        }
    }

    #[test]
    fn shortcuts_reject_busy_actions_and_ignore_repeats_and_shift_tab_in_menus() {
        let mut app = app();
        let (sender, requests) = mpsc::channel();
        app.commands = Some(sender);
        app.input.insert_str("草稿🙂");
        let mut dirty = false;
        for state in [
            State::Running,
            State::Resetting,
            State::SwitchingModel,
            State::SettingThinkLevel,
            State::Unavailable,
        ] {
            app.state = state;
            app.handle(key(KeyCode::BackTab), &mut dirty);
            app.handle(function_key('p'), &mut dirty);
            assert_eq!(app.input.lines(), &["草稿🙂"]);
            assert!(app.model_picker_draft.is_none());
        }
        app.state = State::Ready;
        app.skills_pending = true;
        app.handle(key(KeyCode::BackTab), &mut dirty);
        app.handle(function_key('p'), &mut dirty);
        app.skills_pending = false;
        for mut key in [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('p'), FUNCTION_MODIFIER),
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
        ] {
            key.kind = KeyEventKind::Repeat;
            app.handle(Event::Key(key), &mut dirty);
            key.kind = KeyEventKind::Release;
            app.handle(Event::Key(key), &mut dirty);
        }
        assert_eq!(app.input.lines(), &["草稿🙂"]);
        assert!(app.model_picker_draft.is_none());
        app.clear_input();
        app.handle(Event::Paste("/effort ".into()), &mut dirty);
        app.handle(
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)),
            &mut dirty,
        );
        assert_eq!(app.input.lines(), &["/effort "]);
        assert!(app.menu.is_some());
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        assert!(app.input_history.is_empty());
    }

    #[test]
    fn shortcuts_help_and_empty_model_search_keep_drafts_and_never_submit_tasks() {
        let mut app = app();
        app.state = State::Ready;
        let (sender, requests) = mpsc::channel();
        app.commands = Some(sender);
        app.input.insert_str("未发送🙂");
        let cursor = app.input.cursor();
        let mut dirty = false;
        app.state = State::Running;
        app.handle(key(KeyCode::F(1)), &mut dirty);
        let screen = draw(&mut app, 180, 40);
        assert!(screen.contains("Shift + Tab"));
        assert!(screen.contains(if cfg!(target_os = "macos") {
            "⌥ + P"
        } else {
            "Ctrl + P"
        }));
        assert_eq!(app.input.lines(), &["未发送🙂"]);
        assert_eq!(app.input.cursor(), cursor);
        app.state = State::Ready;
        app.handle(function_key('p'), &mut dirty);
        app.handle(Event::Paste("missing".into()), &mut dirty);
        assert!(active_menu(&app).items().is_empty());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert_eq!(app.input.lines(), &["未发送🙂"]);
        app.handle(function_key('p'), &mut dirty);
        app.input = editor();
        app.handle(Event::Paste("/effort high".into()), &mut dirty);
        assert!(app.menu.is_none());
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(app.model_picker_draft.is_some());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert_eq!(app.input.lines(), &["未发送🙂"]);
        assert!(app.input_history.is_empty());
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn model_default_uses_ctrl_s_and_ignores_alt_s_and_repeats() {
        let mut app = app();
        app.state = State::Ready;
        app.choices = vec![ModelChoice {
            reference: "fixture/primary".into(),
            model: "primary".into(),
            protocol: crate::models::Protocol::Completion,
            thinking: crate::models::ModelThinking::default(),
            context_window: None,
        }];
        let (sender, requests) = mpsc::channel();
        app.commands = Some(sender);
        let mut dirty = false;
        app.handle(Event::Paste("/model ".into()), &mut dirty);
        app.handle(
            Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)),
            &mut dirty,
        );
        let mut repeat = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        repeat.kind = KeyEventKind::Repeat;
        app.handle(Event::Key(repeat), &mut dirty);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        app.handle(control('s'), &mut dirty);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::SaveDefaultModel(reference)) if reference == "fixture/primary"
        ));
        assert_eq!(app.input.lines(), &[""]);
        assert!(app.model_panel.is_some());
    }

    #[test]
    fn model_default_ack_keeps_search_selection_and_current_model() {
        let (mut app, requests, events) = plan_app();
        app.choices = favorite_model_choices();
        app.current_model = Some(app.choices[0].clone());
        app.default_model_reference = Some(app.choices[0].reference.clone());
        let mut dirty = false;
        app.handle(Event::Paste("/model ENDOR-gA".into()), &mut dirty);
        app.handle(control('s'), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::SaveDefaultModel(reference)) if reference == app.choices[2].reference)
        );
        assert_eq!(
            app.default_model_reference.as_deref(),
            Some(app.choices[0].reference.as_str())
        );
        app.handle(key(KeyCode::Enter), &mut dirty);
        app.handle(control('s'), &mut dirty);
        assert!(requests.try_recv().is_err());
        events
            .send(WorkerEvent::DefaultModelSaved(Ok(app.choices[2]
                .reference
                .clone())))
            .unwrap();
        app.receive();
        assert_eq!(model_query(&app), "ENDOR-gA");
        assert_eq!(active_menu(&app).items()[0].value, MenuAction::Model(2));
        assert_eq!(active_menu(&app).label_suffixes()[0], Some(" "));
        assert_eq!(
            app.current_model.as_ref().unwrap().reference,
            app.choices[0].reference
        );
        app.handle(control('s'), &mut dirty);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::SaveDefaultModel(_))
        ));
        events
            .send(WorkerEvent::DefaultModelSaved(Err(anyhow::anyhow!(
                "fixture save failure"
            ))))
            .unwrap();
        app.receive();
        assert_eq!(model_query(&app), "ENDOR-gA");
        assert_eq!(
            app.default_model_reference.as_deref(),
            Some(app.choices[2].reference.as_str())
        );
        assert_eq!(app.input.lines(), &[""]);
        assert!(app.input_history.is_empty());
    }

    #[test]
    fn effort_ctrl_s_saves_highlighted_level_without_changing_current_level() {
        let mut app = app();
        app.state = State::Ready;
        app.think_level = ThinkLevel::High;
        let (sender, requests) = mpsc::channel();
        app.commands = Some(sender);
        let (events, receiver) = mpsc::channel();
        app.events = Some(receiver);
        let mut dirty = false;
        app.handle(Event::Paste("/effort ".into()), &mut dirty);
        app.handle(key(KeyCode::Down), &mut dirty);
        let mut repeat = KeyEvent::new(KeyCode::Char('s'), FUNCTION_MODIFIER);
        repeat.kind = KeyEventKind::Repeat;
        app.handle(Event::Key(repeat), &mut dirty);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        app.handle(opposite_function_key('s'), &mut dirty);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        app.handle(function_key('s'), &mut dirty);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::SaveDefaultThinkLevel(ThinkLevel::Xhigh))
        ));
        assert_eq!(app.think_level, ThinkLevel::High);
        app.handle(function_key('s'), &mut dirty);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        app.handle(Event::Paste("新草稿".into()), &mut dirty);
        events
            .send(WorkerEvent::DefaultThinkLevelSaved(Ok(ThinkLevel::Xhigh)))
            .unwrap();
        app.receive();
        assert_eq!(app.input.lines(), &["新草稿"]);
        assert_eq!(app.think_level, ThinkLevel::High);
        assert_notification(&mut app, statusline::ACCENT);
        let pending = std::mem::take(&mut app.notifications);
        let baseline = draw(&mut app, 100, 30);
        app.notifications = pending;
        let snapshot = draw_buffer(&mut app, 100, 30);
        let mut scrollback = Vec::new();
        app.conversation
            .write_full_for_test(&mut scrollback, &snapshot)
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&scrollback)
                .contains("Default thinking effort saved: xhigh (takes effect next launch)")
        );
        assert!(
            app.notifications
                .tick(Instant::now() + Duration::from_millis(10000))
        );
        assert_eq!(draw(&mut app, 100, 30), baseline);
        assert_eq!(app.input.lines(), &["新草稿"]);
        assert_eq!(app.think_level, ThinkLevel::High);
        app.handle(function_key('s'), &mut dirty);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn shortcuts_platform_function_modifier_preserves_editor_bindings() {
        let mut other = app();
        let mut app = app();
        let mut dirty = false;
        app.state = State::Ready;
        app.input.insert_str("draft-中");
        app.input.move_cursor(CursorMove::Back);
        let cursor = app.input.cursor();
        app.handle(function_key('p'), &mut dirty);
        assert_eq!(app.input.lines(), &[""]);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.model_picker_draft.is_none());
        assert_eq!(app.input.lines(), &["draft-中"]);
        assert_eq!(app.input.cursor(), cursor);

        other.state = State::Ready;
        other.input.insert_str("draft-中");
        other.handle(opposite_function_key('p'), &mut dirty);
        assert!(other.model_picker_draft.is_none());
        assert_eq!(other.input.lines(), &["draft-中"]);

        app.input.move_cursor(CursorMove::Forward);
        app.handle(control('j'), &mut dirty);
        assert_eq!(app.input.lines(), &["draft-中", ""]);
        app.handle(control('a'), &mut dirty);
        app.handle(control('e'), &mut dirty);
        app.handle(control('k'), &mut dirty);
        assert_eq!(app.input.lines(), &["draft-中", ""]);
        app.handle(
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT)),
            &mut dirty,
        );
        assert_eq!(app.input.lines(), &["draft-中", ""]);
        app.handle(key(KeyCode::Char('π')), &mut dirty);
        assert_eq!(app.input.lines(), &["draft-中", "π"]);
        assert!(app.model_picker_draft.is_none());
        assert!(app.handle(control('c'), &mut dirty));
    }

    #[test]
    fn effort_space_opens_inline_menu_and_confirms_only_after_enter() {
        let mut app = app();
        app.state = State::Ready;
        app.think_level = ThinkLevel::High;
        let (sender, requests) = mpsc::channel();
        app.commands = Some(sender);
        let (events, receiver) = mpsc::channel();
        app.events = Some(receiver);
        let mut dirty = false;
        app.input.insert_str("/effort");
        app.refresh_candidates(true);
        assert!(
            menu_values(&app)
                .iter()
                .all(|action| matches!(action, MenuAction::Command(_)))
        );
        app.handle(key(KeyCode::Char(' ')), &mut dirty);
        let screen = draw(&mut app, 100, 30);
        // 测试缓冲区把中文占用的第二列表示为空格。
        let compact = screen.replace(' ', "");
        assert!(
            menu_values(&app)
                .iter()
                .all(|action| matches!(action, MenuAction::Effort(_)))
        );
        assert!(compact.contains("Deepthinking(current)"));
        assert!(screen.contains("❯ high"));
        assert!(compact.find("Disablethinking").unwrap() < compact.rfind("❯/effort").unwrap());
        assert!(compact.contains("Highestlevelthemodelsupports"));
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        app.handle(key(KeyCode::Down), &mut dirty);
        app.handle(key(KeyCode::Down), &mut dirty);
        for (width, height) in [(20, 6), (1, 1), (100, 30)] {
            draw(&mut app, width, height);
            assert_eq!(app.input.lines(), &["/effort "]);
        }
        assert!(draw(&mut app, 100, 30).contains("❯ max"));
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::SetThinkLevel(ThinkLevel::Max))
        ));
        assert_eq!(app.think_level, ThinkLevel::High);
        assert!(app.menu.is_none());
        app.input.insert_str("新草稿");
        events
            .send(WorkerEvent::ThinkLevelSet(Ok(ThinkLevel::Max)))
            .unwrap();
        app.receive();
        assert_eq!(app.think_level, ThinkLevel::Max);
        assert_eq!(app.input.lines(), &["新草稿"]);
    }

    #[test]
    fn command_error_is_transient_and_preserves_editor_and_welcome() {
        let mut app = app();
        app.state = State::Ready;
        app.show_welcome = true;
        app.input.insert_str("/bogus");
        let cursor = app.input.cursor();
        app.submit(PendingKind::Steering);
        assert_notification(&mut app, Color::Rgb(224, 108, 117));
        assert_eq!(app.input.lines(), &["/bogus"]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(app.show_welcome);
        assert!(draw(&mut app, 100, 30).contains("Error"));
        app.notifications
            .tick(Instant::now() + Duration::from_millis(10000));
        let screen = draw(&mut app, 100, 30);
        assert!(!screen.contains("unknown command"));
        assert!(!screen.contains("╭"));
    }

    #[test]
    fn startup_skill_summary_keeps_complete_diagnostics_in_panel() {
        let mut registry = crate::skills::Skills::default();
        registry.diagnostics = vec![
            "skill 根路径必须是目录：first".into(),
            "second diagnostic".into(),
        ];
        let shared = registry.shared();
        let mut app = App::new_with_skills(
            Err(anyhow::anyhow!("fixture unavailable")),
            shared.clone(),
            commands::FileCommands::default(),
            Path::new("."),
            NotificationOptions::default(),
        )
        .unwrap();
        // 配置错误与 skill 诊断合并到同一个启动弹窗。
        app.show_welcome = true;
        assert_notification(&mut app, Color::Rgb(224, 108, 117));
        let screen = draw(&mut app, 100, 30).replace(' ', "");
        assert!(screen.contains("Skillscanissues:2.Use/skillsfordetails."));
        assert!(app.show_welcome);
        assert_eq!(shared.lock().unwrap().diagnostics.len(), 2);
        app.skill_panel = Some(skills::Panel::new(shared.lock().unwrap().clone()));
        app.notifications = Notifications::default();
        let screen = draw(&mut app, 100, 30).replace(' ', "");
        assert!(screen.contains("skill根路径必须是目录：first"));
        assert!(screen.contains("seconddiagnostic"));
        assert!(!screen.contains("Skillscanissues"));
        app.notifications
            .tick(Instant::now() + Duration::from_millis(10000));
        app.skill_panel = None;
        assert!(!draw(&mut app, 100, 30).contains("Skill scan issues"));
    }

    fn file_commands_app(files: commands::FileCommands) -> (App, mpsc::Receiver<WorkerCommand>) {
        let shared = crate::skills::Skills::default().shared();
        let mut app = App::new_with_skills(
            Err(anyhow::anyhow!("fixture unavailable")),
            shared,
            files,
            Path::new("."),
            NotificationOptions::default(),
        )
        .unwrap();
        app.notifications = Notifications::default();
        let (sender, requests) = mpsc::channel();
        app.commands = Some(sender);
        app.state = State::Ready;
        (app, requests)
    }

    fn review_files() -> commands::FileCommands {
        commands::FileCommands {
            entries: vec![commands::FileCommand {
                name: "/review".into(),
                description: "Review source".into(),
                content: "Review $1 now.".into(),
            }],
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn init_menu_submits_model_task_and_displays_command() {
        let (mut app, requests) = file_commands_app(commands::FileCommands::default());
        let mut dirty = false;
        app.handle(Event::Paste("/ini".into()), &mut dirty);
        assert!(draw(&mut app, 100, 30).contains("Create or update project AGENTS.md"));
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &["/init"]);
        assert!(requests.try_recv().is_err());
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt, .. })
            if prompt == crate::project_instructions::INIT_PROMPT)
        );
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("/init"));
        assert!(!screen.contains("First inspect"));
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["/init"]);
    }

    #[test]
    fn file_command_menu_completes_name_and_submits_expanded_prompt() {
        let (mut app, requests) = file_commands_app(review_files());
        let mut dirty = false;
        // 斜杠菜单在内置命令后链接用户命令，且显示加载的 description。
        app.handle(Event::Paste("/rev".into()), &mut dirty);
        assert_eq!(menu_values(&app), vec![MenuAction::FileCommand(0)]);
        assert!(draw(&mut app, 100, 30).contains("Review source"));
        // Tab 只补全名字，不展开也不发送。
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &["/review"]);
        assert!(requests.try_recv().is_err());
        // 参数不打开第二种菜单。
        app.handle(Event::Paste(" auth".into()), &mut dirty);
        assert!(app.menu.is_none());
        // Enter 发送展开后的正文并回显；历史保存原始 slash 输入。
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::Run { prompt: text, .. }) if text == "Review auth now."
        ));
        assert!(app.input.is_empty());
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("Review auth now."));
        assert!(!screen.contains("/review auth"));
        app.handle(key(KeyCode::Up), &mut dirty);
        assert_eq!(app.input.lines(), &["/review auth"]);
        app.input = editor();
        app.refresh_candidates(true);
        // 忙碌时展开一次入队，原始历史不丢失；未应用前不回显。
        app.handle(Event::Paste("/review later".into()), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(app.input.is_empty());
        assert_eq!(app.input_history.last().unwrap(), "/review later");
        let queued = app.input_queue.claim_next().unwrap().unwrap();
        assert_eq!(queued.source, "/review later");
        assert_eq!(queued.prompt(), "Review later now.");
        assert!(!draw(&mut app, 100, 30).contains("Review later now."));
        // 配置不可用和取消收尾仍保护草稿。
        for state in [State::Unavailable, State::Cancelling] {
            app.state = state;
            app.handle(Event::Paste("/review later".into()), &mut dirty);
            app.handle(key(KeyCode::Enter), &mut dirty);
            assert!(requests.try_recv().is_err());
            assert_eq!(app.input.lines(), &["/review later"]);
            app.input = editor();
            app.refresh_candidates(true);
        }
    }

    #[test]
    fn file_command_esc_keeps_draft_and_startup_notice_keeps_error_level() {
        let (mut app, _requests) = file_commands_app(review_files());
        let mut dirty = false;
        app.handle(Event::Paste("/rev".into()), &mut dirty);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.menu.is_none());
        assert_eq!(app.input.lines(), &["/rev"]);

        // 命令诊断与配置错误合并进同一个启动弹窗，并保留 Error 严重级别。
        let mut files = review_files();
        files.diagnostics = vec![
            "commands/broken.md: Invalid command metadata".into(),
            "commands/dup.md: duplicate command name: /dup".into(),
        ];
        let shared = crate::skills::Skills::default().shared();
        let mut app = App::new_with_skills(
            Err(anyhow::anyhow!("fixture unavailable")),
            shared,
            files,
            Path::new("."),
            NotificationOptions::default(),
        )
        .unwrap();
        app.show_welcome = true;
        assert_notification(&mut app, Color::Rgb(224, 108, 117));
        let screen = draw(&mut app, 100, 30).replace(' ', "");
        assert!(screen.contains("Commandscanissues:2.Use/helpfordetails."));
        // /help 附加用户命令说明与完整命令诊断。
        app.notifications = Notifications::default();
        app.state = State::Ready;
        app.show_help();
        let buffer = draw_buffer(&mut app, 100, 30);
        let mut scrollback = Vec::new();
        app.conversation
            .write_full_for_test(&mut scrollback, &buffer)
            .unwrap();
        let text = String::from_utf8_lossy(&scrollback);
        assert!(text.contains("Review source"));
        assert!(text.contains("Command load issues:"));
        assert!(text.contains("commands/broken.md: Invalid command metadata"));
        assert!(text.contains("commands/dup.md: duplicate command name: /dup"));
    }

    #[test]
    fn effort_menu_filters_completes_and_cancels_without_changing_level() {
        let mut app = app();
        app.state = State::Ready;
        let mut dirty = false;
        app.handle(Event::Paste("/effort ".into()), &mut dirty);
        assert!(app.menu.is_some());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.menu.is_none());
        app.refresh_candidates(false);
        assert!(app.menu.is_none());
        assert_eq!(app.input.lines(), &["/effort "]);
        app.handle(key(KeyCode::Char('x')), &mut dirty);
        assert_eq!(
            menu_values(&app),
            vec![MenuAction::Effort(ThinkLevel::Xhigh)]
        );
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &["/effort xhigh"]);
        assert_eq!(app.think_level, ThinkLevel::None);
        app.handle(key(KeyCode::Backspace), &mut dirty);
        app.handle(Event::Paste("\nother".into()), &mut dirty);
        assert!(app.menu.is_none());
        app.input = editor();
        app.input.insert_str("/effort ");
        for state in [State::Running, State::SettingThinkLevel, State::Unavailable] {
            app.state = state;
            app.refresh_candidates(true);
            assert!(app.menu.is_none());
        }
    }

    #[test]
    fn token_speed_completion_and_worker_fallback_preserve_success() {
        let mut app = app();
        app.usage = Some(crate::context::ContextUsage {
            estimated_tokens: 100,
            context_window: Some(40000),
            auto_compact: false,
            threshold_percent: 80,
            last_request: None,
        });
        let (events, receiver) = mpsc::channel();
        app.events = Some(receiver);
        let usage = crate::context::TokenUsage {
            input_tokens: 12000,
            output_tokens: 300,
            cache_write_tokens: None,
            cached_tokens: Some(40),
        };
        for event in [
            AgentEvent::ModelRequestStarted,
            AgentEvent::ModelOutputProgress { output_bytes: 600 },
            AgentEvent::ModelRequestFinished {
                elapsed_ms: 2000,
                usage: Some(usage),
                success: true,
            },
        ] {
            events.send(WorkerEvent::Progress(event)).unwrap();
        }
        app.receive();
        assert_eq!(app.usage.unwrap().last_request, Some(usage));
        assert_eq!(app.token_speed.value(), Some((150.0, false)));
        app.fail_worker();
        assert_eq!(app.token_speed.value(), Some((150.0, false)));
        app.events = Some(mpsc::channel().1);
        app.token_speed.start(Instant::now());
        app.token_speed.progress(600, Instant::now());
        app.fail_worker();
        assert!(!app.token_speed.is_active());
        assert_eq!(app.token_speed.value(), None);
    }

    #[test]
    fn token_speed_missing_usage_replaces_counts_but_failure_preserves_them() {
        let mut app = app();
        let usage = crate::context::TokenUsage {
            input_tokens: 12000,
            output_tokens: 300,
            cache_write_tokens: None,
            cached_tokens: Some(40),
        };
        app.usage = Some(crate::context::ContextUsage {
            estimated_tokens: 100,
            context_window: Some(40000),
            last_request: Some(usage),
            auto_compact: false,
            threshold_percent: 80,
        });
        let (events, receiver) = mpsc::channel();
        app.events = Some(receiver);
        for success in [false, true] {
            events
                .send(WorkerEvent::Progress(AgentEvent::ModelRequestStarted))
                .unwrap();
            events
                .send(WorkerEvent::Progress(AgentEvent::ModelOutputProgress {
                    output_bytes: 600,
                }))
                .unwrap();
            events
                .send(WorkerEvent::Progress(AgentEvent::ModelRequestFinished {
                    elapsed_ms: 2000,
                    usage: None,
                    success,
                }))
                .unwrap();
            events
                .send(WorkerEvent::Completed {
                    result: Ok(RunOutcome::Completed),
                    elapsed: Duration::from_secs(2),
                    finished_at: chrono::Local::now(),
                })
                .unwrap();
            app.receive();
            assert_eq!(
                app.usage.unwrap().last_request,
                if success { None } else { Some(usage) }
            );
            assert_eq!(
                app.token_speed.value(),
                if success { Some((100.0, true)) } else { None }
            );
        }
    }

    #[test]
    fn input_uses_horizontal_rules_and_reserves_secondary_status_on_the_right() {
        let mut app = app();
        app.state = State::Ready;
        app.input.insert_str("draft-中\nsecond line");
        let screen = draw(&mut app, 100, 30);
        let lines: Vec<_> = screen.lines().collect();
        assert!(lines[26].starts_with("❯ draft-中"));
        assert!(lines[27].starts_with("  second line"));
        assert_eq!(lines[28], "─".repeat(100));
        assert!(
            lines[29].starts_with(" \u{f1d8} · \u{ec19} — · \u{f0766} off · \u{f07c} fixture-cwd")
        );

        app.secondary_status = Some("extra info".into());
        let screen = draw(&mut app, 100, 30);
        let top = screen.lines().nth(25).unwrap();
        assert!(top.starts_with(&"─".repeat(50)));
        assert!(top.ends_with(" extra info "));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, &StatusContext::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for x in 0..88 {
            assert_eq!(buffer[(x, 25)].symbol(), "─");
            assert_eq!(buffer[(x, 25)].fg, statusline::ACCENT);
        }
        assert_eq!(app.input.lines(), &["draft-中", "second line"]);
        for (width, height) in [(1, 1), (2, 2), (20, 4), (100, 30)] {
            draw(&mut app, width, height);
            assert_eq!(app.input.lines(), &["draft-中", "second line"]);
        }
    }

    #[test]
    fn mouse_scroll_routes_without_editing_draft() {
        // Report over the input area: wheel navigation is deliberately global.
        let mouse = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 10,
                row: 26,
                modifiers: KeyModifiers::NONE,
            })
        };
        for state in [State::Ready, State::Running, State::Unavailable] {
            let mut app = app();
            app.state = state;
            let (sender, requests) = mpsc::channel();
            app.commands = Some(sender);
            for index in 0..60 {
                app.append("Assistant", &format!("history-{index:02}"));
            }
            app.input.insert_str("draft-中");
            let cursor = app.input.cursor();
            let bottom = draw(&mut app, 100, 30);
            assert!(bottom.contains("history-59"));
            let mut dirty = false;
            assert!(!app.handle(mouse(MouseEventKind::ScrollUp), &mut dirty));
            assert!(dirty);
            let older = draw(&mut app, 100, 30);
            assert!(older.contains("history-57"));
            assert!(!older.contains("history-59"));
            assert_eq!(app.input.lines(), &["draft-中"]);
            assert_eq!(app.input.cursor(), cursor);
            dirty = false;
            assert!(!app.handle(mouse(MouseEventKind::ScrollDown), &mut dirty));
            assert!(dirty);
            assert_eq!(draw(&mut app, 100, 30), bottom);

            for kind in [
                MouseEventKind::Moved,
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
                MouseEventKind::Drag(MouseButton::Left),
                MouseEventKind::ScrollLeft,
                MouseEventKind::ScrollRight,
            ] {
                dirty = false;
                assert!(!app.handle(mouse(kind), &mut dirty));
                assert!(!dirty);
                assert_eq!(draw(&mut app, 100, 30), bottom);
                assert_eq!(app.input.cursor(), cursor);
                assert_eq!(app.input.lines(), &["draft-中"]);
            }

            app.menu = Some(SelectionMenu::new(
                vec![MenuItem {
                    value: MenuAction::Model(0),
                    label: "fixture".into(),
                    description: "fixture model".into(),
                    icon: None,
                    current: true,
                }],
                0,
            ));
            for kind in [MouseEventKind::ScrollUp, MouseEventKind::ScrollDown] {
                dirty = false;
                assert!(!app.handle(mouse(kind), &mut dirty));
                assert!(dirty);
                assert!(app.menu.is_some());
            }
            app.menu = None;
            assert_eq!(draw(&mut app, 100, 30), bottom);
            assert_eq!(app.input.lines(), &["draft-中"]);
            assert_eq!(app.input.cursor(), cursor);
            assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));

            if !matches!(state, State::Running) {
                app.clear_input();
                app.input.insert_str("/");
                app.refresh_candidates(true);
                app.handle(key(KeyCode::Down), &mut dirty);
                let before = draw(&mut app, 100, 30);
                assert!(before.contains("Show available commands"));
                dirty = false;
                assert!(!app.handle(mouse(MouseEventKind::ScrollUp), &mut dirty));
                assert!(dirty);
                assert_ne!(draw(&mut app, 100, 30), before);
                assert_eq!(app.menu.as_ref().unwrap().selected_index(), Some(1));
                assert_eq!(app.input.lines(), &["/"]);
                assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
            }
        }
    }

    #[test]
    fn streaming_receive_is_bounded_and_preserves_busy_draft() {
        let mut app = app();
        let (sender, receiver) = mpsc::sync_channel(512);
        app.events = Some(receiver);
        app.state = State::Running;
        app.input.insert_str("draft-中");
        assert!(
            sender
                .send(WorkerEvent::Progress(AgentEvent::AssistantStarted))
                .is_ok()
        );
        for _ in 0..255 {
            assert!(
                sender
                    .send(WorkerEvent::Progress(AgentEvent::AssistantDelta {
                        text: "x".into(),
                        refusal: false,
                    }))
                    .is_ok()
            );
        }
        assert!(
            sender
                .send(WorkerEvent::completed(
                    Instant::now(),
                    Ok(RunOutcome::Completed)
                ))
                .is_ok()
        );
        assert!(app.receive());
        assert!(matches!(app.state, State::Running));
        assert_eq!(app.input.lines(), &["draft-中"]);
        assert!(app.receive());
        assert!(matches!(app.state, State::Ready));
        assert_eq!(app.input.lines(), &["draft-中"]);
        assert!(draw(&mut app, 100, 30).contains("draft-中"));
    }

    #[test]
    fn streaming_resize_paging_and_failure_keep_editor_and_ctrl_c_immediate() {
        let mut app = app();
        app.state = State::Running;
        for index in 0..60 {
            app.append("Assistant", &format!("history-{index}"));
        }
        app.input.insert_str("draft-中");
        app.conversation.progress(AgentEvent::AssistantStarted);
        app.conversation.progress(AgentEvent::AssistantDelta {
            text: "live-first".into(),
            refusal: false,
        });
        draw(&mut app, 100, 30);
        let mut dirty = false;
        assert!(!app.handle(key(KeyCode::PageUp), &mut dirty));
        let before = draw(&mut app, 100, 30);
        app.conversation.progress(AgentEvent::AssistantDelta {
            text: "-last".into(),
            refusal: false,
        });
        assert_eq!(before, draw(&mut app, 100, 30));
        assert_eq!(app.input.lines(), &["draft-中"]);
        assert!(!app.handle(Event::Resize(20, 4), &mut dirty));
        let small = draw(&mut app, 20, 4);
        assert!(small.contains("❯"));
        assert_eq!(app.input.lines(), &["draft-中"]);
        assert!(!app.handle(Event::Resize(100, 30), &mut dirty));
        assert!(draw(&mut app, 100, 30).contains("draft-中"));
        for _ in 0..10 {
            app.handle(key(KeyCode::PageDown), &mut dirty);
        }
        assert!(draw(&mut app, 100, 30).contains("live-first-last"));
        let (sender, receiver) = mpsc::sync_channel(256);
        app.events = Some(receiver);
        assert!(
            sender
                .send(WorkerEvent::completed(
                    Instant::now(),
                    Err(anyhow::anyhow!("stream failed"))
                ))
                .is_ok()
        );
        app.receive();
        assert!(matches!(app.state, State::Ready));
        assert_eq!(app.input.lines(), &["draft-中"]);
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("live-first-last"));
        assert!(screen.contains("stream failed"));
        assert!(app.handle(
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &mut dirty,
        ));
    }
    #[test]
    fn skills_panel_cancels_edits_preserves_draft_and_rejects_busy_management() {
        let mut app = app();
        let mut snapshot = crate::skills::Skills::default();
        snapshot.entries.push(crate::skills::Skill {
            name: "rust".into(),
            description: "Rust 指导".into(),
            path: "/skills/rust/SKILL.md".into(),
            enabled: true,
        });
        app.skill_panel = Some(skills::Panel::new(snapshot));
        app.input.insert_str("保留草稿");
        let mut dirty = false;
        app.handle(key(KeyCode::Tab), &mut dirty);
        app.handle(key(KeyCode::Char(' ')), &mut dirty);
        assert!(!app.skill_panel.as_ref().unwrap().entries[0].enabled);
        for (width, height) in [(100, 30), (20, 4), (1, 1), (100, 30)] {
            draw(&mut app, width, height);
        }
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.skill_panel.is_none());
        assert_eq!(app.input.lines(), &["保留草稿"]);
        app.input = editor();
        app.input.insert_str("/skills");
        app.state = State::Running;
        assert!(!app.submit(PendingKind::Steering));
        assert!(!app.skills_pending);
        assert_eq!(app.input.lines(), &["/skills"]);
    }

    fn plan_app() -> (
        App,
        mpsc::Receiver<WorkerCommand>,
        mpsc::Sender<WorkerEvent>,
    ) {
        let mut app = app();
        app.state = State::Ready;
        let (commands, requests) = mpsc::channel();
        let (events, replies) = mpsc::channel();
        app.commands = Some(commands);
        app.events = Some(replies);
        (app, requests, events)
    }

    fn ready_plan_workflow() -> WorkflowState {
        WorkflowState {
            mode: WorkMode::Plan,
            plan: Some(crate::plan::PlanDocument {
                id: "0123456789abcdef0123456789abcdef".into(),
                revision: 2,
                title: "Plan".into(),
                content: "# Plan\n\n- [ ] step one\n- [ ] step two\n".into(),
                status: PlanStatus::Ready,
                origin_prompt: "original task".into(),
                file_name: Some("create-canary.md".into()),
                location: crate::plan::PlanLocation::Plans,
                approved_tasks: None,
            }),
            ..Default::default()
        }
    }

    fn plan_review_fixture(status: PlanStatus) -> PlanReview {
        let document = crate::plan::PlanDocument {
            id: "0123456789abcdef0123456789abcdef".into(),
            revision: 2,
            title: "Plan".into(),
            content: "- [ ] step one\n- [ ] step two\n".into(),
            status,
            origin_prompt: "original task".into(),
            file_name: Some("create-canary.md".into()),
            location: crate::plan::PlanLocation::Plans,
            approved_tasks: None,
        };
        PlanReview {
            token: crate::plan::ReviewToken {
                session_id: "session".into(),
                plan_id: document.id.clone(),
                path: document.path().expect("fixture plan has a path"),
                revision: document.revision,
                content: document.content.clone(),
            },
            document,
            report: None,
        }
    }

    #[test]
    fn plan_prompt_waits_for_success_then_sends_once_without_opening_review() {
        for workflow in [WorkflowState::default(), ready_plan_workflow()] {
            let (mut app, requests, _events) = plan_app();
            app.workflow = workflow;
            app.input.insert_str("/plan 调研登录流程");
            app.submit(PendingKind::Steering);
            assert!(matches!(
                requests.try_recv(),
                Ok(WorkerCommand::EnterPlan { .. })
            ));
            assert!(requests.try_recv().is_err());
            assert!(matches!(app.state, State::ChangingPlan));
            assert!(app.input.is_empty());
            assert_eq!(app.input_history.last().unwrap(), "/plan 调研登录流程");
            // 切换期间新输入的草稿不受发送影响。
            app.input.insert_str("新草稿");
            app.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(ready_plan_workflow())));
            assert!(
                matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt, .. })
                if prompt == "调研登录流程")
            );
            assert!(requests.try_recv().is_err());
            assert!(matches!(app.state, State::Running));
            assert!(app.plan_panel.is_none());
            assert!(app.plan_after_enter.is_none());
            assert_eq!(app.input.lines(), &["新草稿"]);
        }
    }

    #[test]
    fn plan_prompt_failure_never_sends_or_leaks_into_later_transition() {
        for draft in ["", "新草稿"] {
            let (mut app, requests, _events) = plan_app();
            app.input.insert_str("/plan research");
            app.submit(PendingKind::Steering);
            assert!(matches!(
                requests.try_recv(),
                Ok(WorkerCommand::EnterPlan { .. })
            ));
            app.input.insert_str(draft);
            app.receive_event_for_test(WorkerEvent::WorkflowChanged(Err(anyhow::anyhow!(
                "清理失败"
            ))));
            assert!(requests.try_recv().is_err());
            assert!(app.plan_retry_enter);
            assert!(app.plan_after_enter.is_none());
            assert_eq!(
                app.input.lines(),
                &[if draft.is_empty() {
                    "/plan research"
                } else {
                    draft
                }]
            );
            app.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(ready_plan_workflow())));
            assert!(requests.try_recv().is_err());
        }
    }

    #[test]
    fn plan_prompt_with_pending_input_preserves_draft() {
        let (mut app, requests, _events) = plan_app();
        app.input_queue
            .enqueue(PendingKind::FollowUp, "pending".into(), None, false)
            .unwrap();
        app.input.insert_str("/plan research");
        app.submit(PendingKind::Steering);
        assert!(requests.try_recv().is_err());
        assert_eq!(app.input.lines(), &["/plan research"]);
        assert!(app.plan_after_enter.is_none());
    }

    #[test]
    fn plan_commands_are_busy_refused_while_running() {
        let (mut app, requests, _events) = plan_app();
        app.state = State::Running;
        // Plan 模式下切换与任务提交都必须被忙碌拒绝；旧子命令由解析层拒绝。
        app.workflow = WorkflowState {
            mode: WorkMode::Plan,
            plan: None,
            ..Default::default()
        };
        for command in ["/plan", "/plan research"] {
            app.input.insert_str(command);
            app.submit(PendingKind::Steering);
            assert_eq!(app.input.lines(), &[command]);
            app.input = editor();
        }
        assert!(requests.try_recv().is_err());
        assert_notification(&mut app, Color::Rgb(224, 108, 117));
    }

    #[test]
    fn plan_ready_refuses_auto_dispatch_but_ctrl_r_explicitly_resumes() {
        let (mut app, requests, _events) = plan_app();
        app.workflow = ready_plan_workflow();
        app.queue_paused = false;
        // 模拟 pending：直接通过输入队列入队。
        app.input_queue
            .enqueue(PendingKind::Steering, "follow up".to_string(), None, false)
            .unwrap();
        assert!(!app.dispatch_next(false));
        assert!(requests.try_recv().is_err());
        app.resume_queue();
        assert!(matches!(requests.try_recv(), Ok(WorkerCommand::Run { .. })));
    }

    #[test]
    fn read_plan_opens_panel_and_execute_keep_sends_token() {
        let (mut app, requests, _events) = plan_app();
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Ready,
        ))));
        assert!(app.plan_panel.is_some());
        assert_eq!(app.plan_progress, (0, 2));
        // 默认选中第一项；向下选中保留上下文执行并携带 token。
        app.handle(key(KeyCode::Down), &mut false);
        app.handle(key(KeyCode::Enter), &mut false);
        let command = requests.try_recv().unwrap();
        let WorkerCommand::ApprovePlan { token, context, .. } = command else {
            panic!("expected ApprovePlan command");
        };
        assert_eq!(token.revision, 2);
        assert_eq!(context, ExecutionContext::Keep);
        assert!(matches!(app.state, State::PreparingExecution));
        assert_eq!(
            app.secondary_status.as_deref(),
            Some("Preparing execution...")
        );
    }

    #[test]
    fn plan_focus_highlights_only_the_active_frame_and_preserves_selection() {
        let (mut app, requests, _events) = plan_app();
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Ready,
        ))));
        let muted = transcript_style::MUTED;
        let accent = transcript_style::ACCENT;
        for colors in [
            [muted, accent, muted],
            [muted, muted, statusline::ACCENT],
            [accent, muted, muted],
            [muted, accent, muted],
        ] {
            let buffer = draw_buffer(&mut app, 100, 30);
            let actions_y = (0..30).find(|&y| buffer[(0, y)].symbol() == "╭").unwrap();
            for (y, color) in [0, actions_y, 26].into_iter().zip(colors) {
                assert_eq!(buffer[(99, y)].fg, color, "边框所在行：{y}");
            }
            let rows: Vec<String> = (actions_y + 1..actions_y + 7)
                .map(|y| (1..99).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            assert!(rows[0].starts_with("❯ Execute in a new session"));
            assert_eq!(rows.iter().filter(|row| row.contains('❯')).count(), 1);
            for (index, row) in rows.iter().enumerate() {
                assert!(!row.contains(&format!("{}. ", index + 1)), "{row}");
            }
            let hint: String = (1..99)
                .map(|x| buffer[(x, actions_y + 7)].symbol())
                .collect();
            assert!(!hint.contains("1–6 select"), "{hint}");
            app.handle(key(KeyCode::Tab), &mut false);
        }
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn plan_ready_paste_keeps_output_and_discusses_from_the_input() {
        let (mut app, requests, _events) = plan_app();
        app.input.insert_str("existing ");
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Ready,
        ))));
        app.handle(Event::Paste("question".into()), &mut false);
        assert_eq!(app.input.lines(), &["existing question"]);
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("step one"), "{screen}");
        assert!(
            screen.contains("Save draft and leave Plan mode"),
            "{screen}"
        );
        assert!(screen.contains("existing question"), "{screen}");
        assert!(
            !screen.contains("Plan ready · /plan view to review"),
            "{screen}"
        );
        app.handle(key(KeyCode::Enter), &mut false);
        let WorkerCommand::Run { prompt, .. } = requests.try_recv().unwrap() else {
            panic!("expected discussion run");
        };
        assert_eq!(prompt, "existing question");
        assert!(app.plan_panel.is_none());
        assert!(app.input.is_empty());
    }

    #[test]
    fn execution_prepared_installs_cleanup_and_acks_before_running() {
        let (mut app, _requests, events) = plan_app();
        app.state = State::PreparingExecution;
        let (reply, ack) = mpsc::sync_channel(1);
        let registry = crate::mcp::Registry::connect(
            &Default::default(),
            Path::new("."),
            &CancellationToken::new(),
        )
        .unwrap();
        let cleanup = registry.cleanup().unwrap();
        let workflow = WorkflowState {
            mode: WorkMode::Execute,
            plan: Some(crate::plan::PlanDocument {
                status: PlanStatus::Executed,
                ..ready_plan_workflow().plan.expect("plan")
            }),
            ..Default::default()
        };
        events
            .send(WorkerEvent::ExecutionPrepared {
                workflow: workflow.clone(),
                session_id: Some("next".into()),
                replay: None,
                cleanup,
                todo_snapshot: None,
                reply,
            })
            .unwrap();
        app.receive();
        assert!(ack.try_recv().is_ok());
        assert!(app.mcp_cleanup.is_some());
        assert_eq!(app.workflow, workflow);
        assert_eq!(app.session_id.as_deref(), Some("next"));
        assert!(matches!(app.state, State::Running));
        assert!(!app.show_welcome);
    }

    #[test]
    fn awaiting_review_completion_requests_plan_read() {
        let (mut app, requests, events) = plan_app();
        events
            .send(WorkerEvent::completed(
                Instant::now(),
                Ok(RunOutcome::AwaitingPlanReview),
            ))
            .unwrap();
        app.receive();
        assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadPlan)));
        assert!(matches!(app.state, State::ReadingPlan));
    }

    #[test]
    fn progress_view_during_running_is_readonly_and_cached() {
        let (mut app, requests, _events) = plan_app();
        app.workflow = ready_plan_workflow();
        app.state = State::Running;
        app.input.insert_str("/plan view");
        app.submit(PendingKind::Steering);
        assert!(requests.try_recv().is_err());
        let panel = app.plan_panel.as_ref().expect("progress view");
        assert!(panel.token().is_none());
        for index in 0..4 {
            assert!(!panel.action_enabled(index));
        }
        // Esc 只关闭面板，不取消任务。
        app.handle(key(KeyCode::Esc), &mut false);
        assert!(app.plan_panel.is_none());
        assert!(matches!(app.state, State::Running));
    }

    #[test]
    fn plan_progress_event_updates_cache_and_panel_without_transcript() {
        let (mut app, _requests, _events) = plan_app();
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Executed,
        ))));
        app.receive_event_for_test(WorkerEvent::Progress(AgentEvent::PlanProgressUpdated {
            plan_id: "0123456789abcdef0123456789abcdef".into(),
            revision: 3,
            content: "- [x] step one\n- [ ] step two\n".into(),
            file_name: None,
            location: None,
            status: None,
        }));
        assert_eq!(app.plan_progress, (1, 2));
        assert_eq!(app.workflow.plan.as_ref().expect("plan").revision, 3);
    }

    #[test]
    fn plan_toggle_enters_standby_and_leaves_without_confirm() {
        let (mut app, requests, _events) = plan_app();
        // Execute：裸 /plan 进入 Plan 待命（Plan+None，不打开审阅）。
        app.input.insert_str("/plan");
        app.submit(PendingKind::Steering);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::EnterPlan { .. })
        ));
        assert!(matches!(
            app.plan_after_enter,
            Some(PlanEnterAction::Standby)
        ));
        app.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(WorkflowState {
            mode: WorkMode::Plan,
            plan: None,
            ..Default::default()
        })));
        assert!(app.plan_after_enter.is_none());
        assert!(!app.plan_retry_enter);
        assert!(app.plan_panel.is_none());

        // 空待命可直接退出：裸 /plan 直接发送 LeavePlan，无确认框。
        app.input.insert_str("/plan");
        app.submit(PendingKind::Steering);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::LeavePlan { .. })
        ));
        app.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(WorkflowState::default())));
        assert_eq!(app.workflow.mode, WorkMode::Execute);
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn plan_toggle_refuses_leaving_with_unsaved_revision_zero_draft() {
        let (mut app, requests, _events) = plan_app();
        let mut plan = crate::plan::PlanDocument::new().unwrap();
        plan.origin_prompt = "first task".into();
        app.workflow = WorkflowState {
            mode: WorkMode::Plan,
            plan: Some(plan),
            ..Default::default()
        };
        app.input.insert_str("/plan");
        app.submit(PendingKind::Steering);
        assert!(requests.try_recv().is_err());
        assert!(matches!(app.state, State::Ready));
        assert_eq!(app.workflow.mode, WorkMode::Plan);
        assert!(app.workflow.plan.is_some());
        assert_notification(&mut app, Color::Rgb(224, 108, 117));
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("Save the draft with plan_write before leaving Plan mode."));
    }

    #[test]
    fn plan_leave_failure_keeps_leave_direction_on_retry() {
        let (mut app, requests, _events) = plan_app();
        app.workflow = ready_plan_workflow();
        app.input.insert_str("/plan");
        app.submit(PendingKind::Steering);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::LeavePlan { .. })
        ));
        app.receive_event_for_test(WorkerEvent::WorkflowChanged(Err(anyhow::anyhow!(
            "退出失败"
        ))));
        // 退出失败不设置进入重试标记；下一次裸 /plan 仍然离开。
        assert!(!app.plan_retry_enter);
        app.input.insert_str("/plan");
        app.submit(PendingKind::Steering);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::LeavePlan { .. })
        ));
    }

    #[test]
    fn plan_old_subcommands_are_rejected_locally_in_tui() {
        let (mut app, requests, _events) = plan_app();
        for command in ["/plan new", "/plan off"] {
            app.input.insert_str(command);
            app.submit(PendingKind::Steering);
            // 只报本地错误：不切换模式、不发送模型请求、输入原样保留。
            assert!(requests.try_recv().is_err());
            assert_eq!(app.input.lines(), &[command]);
            assert_eq!(app.workflow.mode, WorkMode::Execute);
            assert!(app.workflow.plan.is_none());
            assert_notification(&mut app, Color::Rgb(224, 108, 117));
            let screen = draw(&mut app, 120, 30);
            assert!(screen.contains("Use /plan to toggle; manage plans with /plan-list."));
            app.input = editor();
            app.notifications.clear();
        }
    }

    /// 打开审阅面板并通过操作区第 4 项启动独立 reviewer；返回 ReviewPlan 命令。
    fn start_review(app: &mut App, requests: &mpsc::Receiver<WorkerCommand>) -> WorkerCommand {
        let mut review = plan_review_fixture(PlanStatus::Ready);
        review.report = app.workflow.review.clone();
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(review)));
        app.handle(key(KeyCode::Char('4')), &mut false);
        app.handle(key(KeyCode::Enter), &mut false);
        let command = requests.try_recv().unwrap();
        let WorkerCommand::ReviewPlan { token, .. } = &command else {
            panic!("expected ReviewPlan command");
        };
        assert_eq!(token.revision, 2);
        assert!(matches!(app.state, State::ReviewingPlan));
        assert!(app.plan_panel.is_none());
        assert!(app.review_transcript.is_some());
        command
    }

    #[test]
    fn review_action_refuses_pending_input() {
        let (mut app, requests, _events) = plan_app();
        app.workflow = ready_plan_workflow();
        app.input_queue
            .enqueue(PendingKind::Steering, "pending".into(), None, false)
            .unwrap();
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Ready,
        ))));
        app.handle(key(KeyCode::Char('4')), &mut false);
        app.handle(key(KeyCode::Enter), &mut false);
        assert!(requests.try_recv().is_err());
        assert!(app.plan_panel.is_some());
        assert!(matches!(app.state, State::Ready));
    }

    #[test]
    fn review_page_routes_progress_only_to_review_transcript_and_keeps_draft() {
        let (mut app, requests, _events) = plan_app();
        app.workflow = ready_plan_workflow();
        start_review(&mut app, &requests);
        app.usage = Some(crate::context::ContextUsage {
            estimated_tokens: 100,
            context_window: Some(40000),
            auto_compact: false,
            threshold_percent: 80,
            last_request: None,
        });
        app.receive_event_for_test(WorkerEvent::ReviewProgress(AgentEvent::AssistantDelta {
            text: "review finding".into(),
            refusal: false,
        }));
        app.receive_event_for_test(WorkerEvent::ReviewProgress(AgentEvent::ContextUpdated {
            usage: crate::context::ContextUsage {
                estimated_tokens: 999,
                context_window: Some(40000),
                auto_compact: false,
                threshold_percent: 80,
                last_request: None,
            },
        }));
        // 主用量与派发不受审查影响；屏幕只显示审查流。
        assert_eq!(app.usage.as_ref().unwrap().estimated_tokens, 100);
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("review finding"));

        // steering/followup/命令提交全部被拒，草稿不丢失。
        app.input.insert_str("draft during review");
        app.handle(key(KeyCode::Enter), &mut false);
        assert_eq!(app.input.lines(), &["draft during review"]);
        assert!(requests.try_recv().is_err());
        app.handle(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            &mut false,
        );
        assert_eq!(app.input.lines(), &["draft during review"]);
        assert!(requests.try_recv().is_err());
        app.input = editor();
        app.input.insert_str("/plan");
        app.handle(key(KeyCode::Enter), &mut false);
        assert_eq!(app.input.lines(), &["/plan"]);
        assert!(requests.try_recv().is_err());
        assert!(matches!(app.state, State::ReviewingPlan));
    }

    #[test]
    fn esc_cancels_reviewer_and_returns_to_menu_on_controlled_finish() {
        let (mut app, requests, _events) = plan_app();
        app.workflow = ready_plan_workflow();
        app.workflow.review = Some(PlanReviewReport {
            plan_id: "0123456789abcdef0123456789abcdef".into(),
            revision: 2,
            model_reference: "provider/model".into(),
            content: "old findings".into(),
        });
        let WorkerCommand::ReviewPlan { cancellation, .. } = start_review(&mut app, &requests)
        else {
            panic!("expected ReviewPlan command");
        };
        app.handle(key(KeyCode::Esc), &mut false);
        // 收尾前保持审查态；迟到的 pending 不派发。
        assert!(cancellation.is_cancelled());
        assert!(matches!(app.state, State::ReviewingPlan));
        app.input_queue
            .enqueue(PendingKind::Steering, "late".into(), None, false)
            .unwrap();
        app.resume_queue();
        assert!(requests.try_recv().is_err());

        app.receive_event_for_test(WorkerEvent::PlanReviewed(Err(anyhow::anyhow!(
            "review cancelled"
        ))));
        // 回六项菜单的读取中；旧完整报告与 Plan 模式保留。
        assert!(app.review_transcript.is_none());
        assert!(matches!(app.state, State::ReadingPlan));
        assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadPlan)));
        assert_eq!(app.workflow.mode, WorkMode::Plan);
        assert_eq!(
            app.workflow
                .review
                .as_ref()
                .map(|report| report.content.as_str()),
            Some("old findings")
        );
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Ready,
        ))));
        let panel = app.plan_panel.as_ref().expect("menu panel");
        assert!(panel.action_enabled(1));
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn review_success_focuses_report_and_never_autostarts_execution() {
        let (mut app, requests, _events) = plan_app();
        app.workflow = ready_plan_workflow();
        start_review(&mut app, &requests);
        app.receive_event_for_test(WorkerEvent::PlanReviewed(Ok(PlanReviewReport {
            plan_id: "0123456789abcdef0123456789abcdef".into(),
            revision: 2,
            model_reference: "provider/model-x".into(),
            content: "final findings".into(),
        })));
        assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadPlan)));
        assert_eq!(
            app.workflow
                .review
                .as_ref()
                .map(|report| report.content.as_str()),
            Some("final findings")
        );

        // 外部编辑期间产生新修订：刷新返回 r3 与针对 r2 的旧报告。
        let mut review = plan_review_fixture(PlanStatus::Ready);
        review.document.revision = 3;
        review.document.content = "- [ ] step one\n- [ ] step two\n- [ ] step three\n".into();
        review.token.revision = 3;
        review.report = Some(PlanReviewReport {
            plan_id: review.document.id.clone(),
            revision: 2,
            model_reference: "provider/model-x".into(),
            content: "stale findings".into(),
        });
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(review)));
        let panel = app.plan_panel.as_ref().expect("report panel");
        assert!(panel.on_review_body());
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("outdated"));
        assert!(screen.contains("stale findings"), "{screen}");
        // 报告只是辅助材料：不产生实施请求，也不重新进入审查。
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn failed_read_after_review_shows_report_without_execution_token() {
        let (mut app, requests, _events) = plan_app();
        app.workflow = ready_plan_workflow();
        start_review(&mut app, &requests);
        app.receive_event_for_test(WorkerEvent::PlanReviewed(Ok(PlanReviewReport {
            plan_id: "0123456789abcdef0123456789abcdef".into(),
            revision: 2,
            model_reference: "provider/model-x".into(),
            content: "final findings".into(),
        })));
        assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadPlan)));
        app.receive_event_for_test(WorkerEvent::PlanRead(Err(anyhow::anyhow!(
            "plan file vanished"
        ))));
        // 报告展示但旧执行 token 失效：只读面板禁用全部动作。
        let panel = app.plan_panel.as_ref().expect("degraded panel");
        assert!(panel.token().is_none());
        for index in 0..4 {
            assert!(!panel.action_enabled(index));
        }
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("final findings"));
        assert!(screen.contains("plan file vanished"));
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    }
}
