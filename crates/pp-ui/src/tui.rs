mod app;
mod ask;
mod attachment_edit;
mod auth;
#[cfg(test)]
mod auth_tests;
mod changes;
mod edit_diff;
mod files;
mod frame_clock;
mod history;
mod image_terminal;
mod images;
mod input_shim;
mod lsp;
mod markdown;
mod mcp;
mod menu;
#[cfg(debug_assertions)]
mod mock_test;
mod model;
mod notification;
mod pipes;
mod plan;
mod plan_leave;
mod plan_list;
mod render_worker;
mod resources;
mod roles;
mod scrollback;
mod sessions;
mod skills;
mod statusline;
mod stream_buffer;
mod subagents;
mod surface;
mod tips;
mod todo;
mod tool_display;
mod transcript;
mod transcript_style;
mod tree;
mod welcome;
mod worker_bridge;

use worker_bridge::{
    ModelListing, ResumeState, WorkerCommand, WorkerEvent, publish_pipe_store, publish_prepared,
    publish_transition_failure, push_runtime_snapshot, push_workflow_snapshot, request_ask,
    usage_snapshot,
};

use crate::commands;
use crate::commands::COMMANDS;
use crate::commands::Command;
use crate::commands::PlanCommand;
use crate::shortcuts::FUNCTION_MODIFIER;
use crate::shortcuts::platform_text;
use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use menu::{MenuEvent, MenuItem, SelectionMenu};
use notification::Level;
use pp_config::config::ModelChoice;
use pp_config::model_state::Selection;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::RunOutcome;
use pp_core::events::AgentEvent;
use pp_core::model::ModelSource;
use pp_core::model::ThinkLevel;
use pp_core::plan::ExecutionContext;
use pp_core::plan::PlanLocation;
use pp_core::plan::PlanReview;
use pp_core::plan::PlanReviewReport;
use pp_core::plan::PlanStatus;
use pp_core::plan::ReviewToken;
use pp_core::plan::WorkMode;
use pp_core::plan::WorkflowState;
use pp_runtime::agent::Agent;
use pp_runtime::agent::context::CompactionOutcome;
use pp_runtime::agent::input::InputQueue;
use pp_runtime::agent::input::PendingKind;
use pp_runtime::agent::input::QueueSnapshot;
use pp_runtime::agent::plan_management::PlanOpenIntent;
use pp_runtime::agent::plan_management::PlanSelection;
#[cfg(test)]
use pp_runtime::tools::ChangesView;
#[cfg(test)]
use pp_runtime::tools::UndoResult;
#[cfg(test)]
use pp_runtime::tools::UndoToken;
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Widget},
};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};
use statusline::{StatusContext, WorkspaceStatus};
use std::{
    collections::BTreeMap,
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
    " · ↑↓ move/history/menu · Shift + Tab cycle thinking · Ctrl + F favorite selected model · Ctrl + S save selected model as default · ⌥ + S save selected effort as default · ⌥ + P choose model · F1 help · Tab complete · Esc cancel task/dismiss · Esc Esc session tree (empty input) · Ctrl + A / Ctrl + E line start/end · Ctrl + K delete to end of line · Shift + Enter / Ctrl + J newline · Ctrl + V paste image/text · Ctrl + U undo · @ local resources · Ctrl + O expand/collapse all tool output · F8 pipe streams · ⌥ + T expand/collapse TODO · ⌥ + PgUp/PgDn scroll TODO · Wheel/PgUp/PgDn scroll · Ctrl + C quit",
    " · ↑↓ move/history/menu · Shift + Tab cycle thinking · Ctrl + F favorite selected model · Ctrl + S save selected model/effort as default in its menu · Ctrl + P choose model · F1 help · Tab complete · Esc cancel task/dismiss · Esc Esc session tree (empty input) · Ctrl + A / Ctrl + E line start/end · Ctrl + K delete to end of line · Shift + Enter / Ctrl + J newline · Ctrl + V paste image/text · Ctrl + U undo · @ local resources · Ctrl + O expand/collapse all tool output · F8 pipe streams · Ctrl + T expand/collapse TODO · Ctrl + PgUp/PgDn scroll TODO · Wheel/PgUp/PgDn scroll · Ctrl + C quit",
);
const EFFORT_HINT: &str = platform_text(
    "Enter select · ⌥ + S set selected effort as default · Esc cancel",
    "Enter select · Ctrl + S set selected effort as default · Esc cancel",
);
const MOUSE_SCROLL_LINES: usize = 3;

#[allow(clippy::too_many_arguments)]
pub fn run(
    agent: Result<Agent>,
    cwd: &Path,
    skills: pp_config::skills::SharedSkills,
    file_commands: commands::FileCommands,
    auto_continue: bool,
    open_resume: bool,
    options: pp_config::config::TuiSettings,
    cost_display: pp_config::config::CostDisplay,
    bootstrap: Option<pp_runtime::agent::Bootstrap>,
    initial_command: Option<String>,
) -> Result<()> {
    // 初始化中途失败和 panic 同样恢复终端模式。
    let _guard = TerminalGuard;
    surface::initialize().context("initializing terminal")?;
    // 能力探测独占 stdin，必须在 crossterm 首次读取事件之前完成。
    let (picker, mut initial_events, pending_reply) = image_terminal::detect();
    // tmux 默认以 xterm modifyOtherKeys 编码回报扩展按键，这里接管 fd 0 把它
    // 翻译成 crossterm 支持的 CSI-u；探测结束后安装，避免争抢原始终端。
    input_shim::install(pending_reply).context("installing input translator")?;
    let mut surface = surface::Surface::new(io::stdout());
    let mut canvas = ratatui::Terminal::new(ratatui::backend::TestBackend::new(1, 1))?;
    // 持久输入历史与 agent 配置目录同级；无可定位目录时明确只保留进程内回忆。
    let history_path = bootstrap
        .as_ref()
        .map(|bootstrap| bootstrap.dir.join("history.db"));
    let mut app = App::new_with_skills(agent, skills, file_commands, cwd, history_path)?;
    app.image_view.configure(picker);
    app.cost_display = cost_display;
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
    // `pp -r`：启动后立即打开 /resume 会话选择面板，等同于空闲时提交 /resume。
    if open_resume && matches!(app.state, State::Ready) {
        app.open_session_panel();
    }
    // `pp /COMMAND`：与按 Enter 提交走同一路径；`/quit` 沿用既有退出收尾。
    let mut initial_quit = false;
    if let Some(command) = initial_command {
        initial_quit = app.submit_initial_command(&command);
    }
    let mut workspace = WorkspaceStatus::new(cwd)?;
    app.renderer = Some(render_worker::Worker::start().context("starting layout worker")?);
    let mut dirty = true;
    let mut generation = app.conversation.native_generation();
    let mut rebuilding_history = true;
    let mut clock = frame_clock::FrameClock::new(options.max_fps);
    loop {
        // 先采纳已经到达的后台结果，再用最新状态解释下一批按键。
        dirty |= app.receive();
        let mut quit = initial_quit;
        for event in initial_events.drain(..) {
            quit |= app.handle(event, &mut dirty);
        }
        let input_started = Instant::now();
        let mut input_changed = false;
        for _ in 0..64 {
            if input_started.elapsed() >= Duration::from_millis(2)
                || !event::poll(Duration::ZERO).context("polling terminal input")?
            {
                break;
            }
            let event = event::read().context("reading terminal input")?;
            quit |= app.handle(event, &mut input_changed);
            if quit {
                break;
            }
        }
        dirty |= input_changed;
        dirty |= app.receive();
        dirty |= app
            .renderer
            .as_ref()
            .is_some_and(render_worker::Worker::changed);
        dirty |= workspace.receive();
        dirty |= app.conversation.tick(Instant::now());
        dirty |= app.hub_tick();
        dirty |= app.review_transcript_tick(Instant::now());
        dirty |= app.token_speed.tick(Instant::now());
        dirty |= app.retry_tick(Instant::now());
        if quit {
            app.flush_stream();
            app.conversation.finish_markdown_streams();
            if let Some(cancellation) = &app.active_cancellation {
                cancellation.cancel();
            }
            let cleanup = app.shutdown_background_tasks();
            if let Err(error) = &cleanup {
                app.notify(Level::Error, format!("{error:#}"));
            }
            app.shutdown_history();
            paint_surface(
                &mut app,
                &workspace.context,
                &mut canvas,
                &mut surface,
                &mut rebuilding_history,
                true,
            )
            .context("finishing terminal transcript")?;
            surface
                .finish(canvas.backend().buffer().area.height)
                .context("restoring shell cursor")?;
            drop(_guard);
            crate::cli::write_resume_hint(&mut io::stderr(), app.session_id.as_deref())?;
            return cleanup;
        }
        if dirty && clock.ready(Instant::now()) {
            clock.started(Instant::now());
            app.flush_stream();
            if generation != app.conversation.native_generation() {
                surface.reset_history();
                generation = app.conversation.native_generation();
                rebuilding_history = true;
                app.history_browser = false;
            }
            dirty = paint_surface(
                &mut app,
                &workspace.context,
                &mut canvas,
                &mut surface,
                &mut rebuilding_history,
                false,
            )
            .context("drawing terminal")?;
        }
        let wait = if dirty {
            clock
                .remaining(Instant::now())
                .min(Duration::from_millis(5))
        } else if app
            .renderer
            .as_ref()
            .is_some_and(render_worker::Worker::pending)
        {
            Duration::from_millis(5)
        } else {
            Duration::from_millis(10)
        };
        let _ = event::poll(wait).context("polling terminal input")?;
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
    rebuilding_history: &mut bool,
    exiting: bool,
) -> Result<bool> {
    app.image_view.suspended = exiting;
    if exiting {
        app.image_view.close();
    }
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
        loop {
            let Some(batch) =
                app.conversation
                    .next_native_batch(width, conversation_height, exiting)
            else {
                if exiting && app.conversation.render_pending() {
                    thread::park_timeout(Duration::from_millis(1));
                    continue;
                }
                // 排版等待期间仍可能产生后续批次，不能在批次间隙显示光标。
                if !app.conversation.render_pending() {
                    *rebuilding_history = false;
                }
                break;
            };
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
            // 历史重建跨越多帧，只在全部批次提交后恢复输入光标。
            let cursor = (canvas.backend().cursor_visible() && !*rebuilding_history)
                .then(|| canvas.backend().cursor_position());
            surface.paint_images(buffer, cursor, false, &batch.rows, &app.image_view.graphics)?;
            app.conversation.acknowledge_native_batch(batch);
            if !exiting {
                return Ok(true);
            }
        }
    }
    let mut rendered = None;
    canvas.draw(|frame| {
        app.draw(frame, context);
        rendered = Some(frame.buffer_mut().clone());
    })?;
    let cursor = (canvas.backend().cursor_visible() && (alternate || !*rebuilding_history))
        .then(|| canvas.backend().cursor_position());
    let buffer = if exiting {
        app.normal_snapshot.as_ref().expect("normal frame")
    } else {
        rendered.as_ref().expect("rendered frame")
    };
    surface.paint_images(buffer, cursor, alternate, &[], &app.image_view.graphics)?;
    Ok(false)
}

#[derive(Clone, Copy)]
enum State {
    ManagingMcp,
    Ready,
    Running,
    RunningShell,
    /// 本地测试专用运行态；非 dev/test 构建仍保留状态机形状，仅专用命令已编译排除。
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    RunningTools,
    Cancelling,
    Compacting,
    Resetting,
    SwitchingModel,
    SavingModelSettings,
    // 仅剩 `⌥ + S` / `Ctrl + S` 的全局默认值保存；会话档位切换不再经过 worker。
    SavingThinkDefault,
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
    McpSubcommand(usize),
    McpServer(usize),
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
    resources: resources::Draft,
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
    stream: stream_buffer::StreamBuffer,
    review_stream: stream_buffer::StreamBuffer,
    renderer: Option<render_worker::Worker>,
    model_control: Option<pp_runtime::model_control::Service>,
    // 控制报告与 worker 快照走不同通道；旧快照不能覆盖已应用的新选择。
    model_revision: u64,
    pending_model: Option<(u64, String)>,
    // 当前生效模型的来源；`Default` 不增加状态栏噪声。
    current_source: Option<ModelSource>,
    // `a` 的手动覆盖快照；只用于 `/Roles` 只读展示与清除，不参与决议。
    override_selection: Option<Selection>,
    // 期望档位：`0` 表示已提交但控制线程尚未确认，其余为 `ThinkQueued` 的 revision。
    // 切档不是 worker 命令，因此这里的变化绝不同步改写 `state`。
    pending_think: Option<(u64, ThinkLevel)>,
    auth_worker: auth::Worker,
    stats_worker: Option<crate::stats::Worker>,
    auth_panel: Option<auth::Panel>,
    auth_cancellation: Option<CancellationToken>,
    auth_pending: bool,
    bootstrap: Option<pp_runtime::agent::Bootstrap>,
    model_refresh_pending: bool,
    conversation: Transcript,
    todo_panel: todo::Panel,
    native_render: bool,
    normal_snapshot: Option<ratatui::buffer::Buffer>,
    history_browser: bool,
    show_welcome: bool,
    input: TextArea<'static>,
    resources: resources::State,
    image_view: images::View,
    input_limits: pp_config::config::ToolLimits,
    shell_mode: bool,
    input_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: Option<(TextArea<'static>, bool)>,
    // 持久输入回忆：独立 worker 拥有 history.db，启动 cwd 固定不变。
    history_worker: Option<history::Worker>,
    history_cwd: std::path::PathBuf,
    // 已加载的最老记录 id；None 表示初始页尚未返回或库为空。
    history_oldest_id: Option<i64>,
    history_has_more: bool,
    // 正在等待的 LoadInputs request_id；同刻只有一个翻页请求。
    history_page_pending: Option<u64>,
    history_request_id: u64,
    // Up 在已加载最老条目上等待下一页；重复 Up 合并为一次等待导航。
    history_waiting_up: bool,
    // 脱敏失败提示只发一次，避免每次追加都重复弹窗。
    history_failure_notified: bool,
    // 会话搜索面板的代次；每次打开单调递增。
    session_generation: u64,
    // 单元测试独占的临时目录，保证 history.db 生命周期覆盖 App。
    #[cfg(test)]
    history_dir: Option<tempfile::TempDir>,
    state: State,
    next_shell_id: u64,
    active_cancellation: Option<CancellationToken>,
    input_queue: InputQueue,
    queue_paused: bool,
    queue_menu: bool,
    queue_menu_revision: Option<u64>,
    queue_snapshot: Option<QueueSnapshot>,
    ask_panel: Option<ask::Panel>,
    ask_reply: Option<mpsc::SyncSender<Vec<pp_core::interaction::AskAnswer>>>,
    ask_id: Option<String>,
    ask_cancellation: Option<CancellationToken>,
    commands: Option<Sender<WorkerCommand>>,
    events: Option<Receiver<WorkerEvent>>,
    // 隔离的 /test-* worker 与开始前保存的正常展示状态；不参与正常会话清理。
    #[cfg(debug_assertions)]
    mock_worker: Option<mock_test::Worker>,
    #[cfg(debug_assertions)]
    mock_saved: Option<mock_test::MockUiSnapshot>,
    unavailable: Option<String>,
    choices: Vec<ModelChoice>,
    current_model: Option<ModelChoice>,
    default_model_reference: Option<String>,
    favorite_models: Vec<String>,
    // 最近一次 worker 发布的常用排序快照；绘制/按键不读数据库。
    model_usage: BTreeMap<String, u64>,
    think_level: ThinkLevel,
    default_think_level: ThinkLevel,
    menu: Option<SelectionMenu<MenuAction>>,
    menu_dismissed: bool,
    model_picker_draft: Option<ModelPickerDraft>,
    model_panel: Option<model::Panel>,
    roles_panel: Option<roles::Panel>,
    model_roles: pp_config::config::ModelRoles,
    secondary_status: Option<String>,
    skill_worker: skills::Worker,
    skill_panel: Option<skills::Panel>,
    skills_pending: bool,
    session_panel: Option<sessions::Panel>,
    tree_panel: Option<tree::Panel>,
    changes_panel: Option<changes::Panel>,
    subagent_panel: Option<subagents::Panel>,
    subagent_autowake: bool,
    // 当前根的观察修订号与正在运行的子代理数。
    subagent_status: Option<(u64, usize)>,
    subagent_control: std::sync::Arc<std::sync::Mutex<Option<pp_runtime::subagent::Binding>>>,
    // Hub 观察线程与根/view epoch；草稿只保留在当前根内。
    hub_worker: Option<subagents::Worker>,
    hub_root_epoch: u64,
    hub_view_epoch: u64,
    hub_request: u64,
    hub_bound: Option<pp_runtime::subagent::Binding>,
    hub_sends: BTreeMap<u64, app::hub::HubSend>,
    child_drafts: subagents::Drafts,
    /// worker 最近发布的根会话临时管道 registry；面板据此判断换根。
    pipe_store: Option<pp_runtime::tools::PipeStore>,
    /// 根会话实例代次；每次 `same_instance` 判定为换根时递增。
    pipe_store_epoch: u64,
    pipes_panel: Option<pipes::Panel>,
    pipes_worker: Option<pipes::Worker>,
    /// 面板 view 代次；选择 run/stream 或关闭时递增，旧回复直接丢弃。
    pipes_view_epoch: u64,
    plan_list_panel: Option<plan_list::Panel>,
    // 列表动作读取 selection 后继续执行的后续步骤。
    plan_list_pending: Option<plan_list::PendingOp>,
    // `n` 在 Plan 模式下先解除绑定（LeavePlan），成功后立即重新进入 Plan 待命。
    plan_reenter_standby: bool,
    plan_leave_confirmation: Option<plan_leave::Panel>,
    escape_tap: EscapeTap,
    session_id: Option<String>,
    // 最近一次 ContextUpdated 快照；渲染直接使用，不逐帧重新估算。
    usage: Option<pp_core::context::ContextUsage>,
    token_speed: statusline::TokenSpeed,
    // 自动重试等待期的固定信息框；不进入对话、不写历史。
    retry_notice: Option<app::retry::Notice>,
    task_status_view: pp_config::task_status::View,
    task_status: Option<pp_config::task_status::Snapshot>,
    cost_display: pp_config::config::CostDisplay,
    lsp_cleanup: Option<pp_runtime::lsp::Cleanup>,
    mcp_view: pp_runtime::mcp::View,
    mcp_panel: Option<mcp::Panel>,
    background_cleanup: Option<pp_runtime::tools::BackgroundCleanup>,
    file_worker: files::Worker,
    file_paths: Vec<String>,
    file_directory: String,
    file_query: Option<files::Query>,
    files_pending: bool,
    file_scan_root: String,
    file_commands: commands::FileCommands,
    // worker 发布的 workflow 快照；UI 不是权限权威，只用于展示与派发守卫。
    workflow: WorkflowState,
    plan_panel: Option<plan::Panel>,
    // 从计划面板发起的讨论完成且队列清空后，安全重读并恢复面板。
    reopen_plan_after_discussion: bool,
    // 仅在正文变化时重算的 (completed, total) 进度缓存。
    plan_progress: (usize, usize),
    // ExecutionPrepared 中 replay 重绘产生的显示错误，等待 SessionResumed 一并通知。
    resume_note: Option<String>,
    // EnterPlan 失败：下一次 /plan 优先重发 EnterPlan 重试。
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
    Prompt(String, Option<pp_runtime::user_input::UserInput>),
}

fn editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_style(Style::default().fg(transcript_style::TEXT));
    editor.set_wrap_mode(WrapMode::WordOrGlyph);
    editor.set_cursor_line_style(Style::default());
    editor.set_cursor_style(Style::default());
    editor.set_max_histories(1000);
    editor
}

/// 编辑器在给定宽度下占用的屏幕行数；主输入框与 child 详情共用同一套折行测量。
pub(in crate::tui) fn input_rows(editor: &TextArea<'_>, width: u16, max_rows: u16) -> u16 {
    if width == 0 || max_rows == 0 {
        return max_rows;
    }
    // 在有界内存缓冲中刷新组件自己的折行映射，保持单词、宽字符和 Tab 规则一致。
    let area = Rect::new(0, 0, width, max_rows);
    editor.render(area, &mut Buffer::empty(area));
    let mut previous = editor.screen_to_data(0, 0);
    for row in 1..max_rows {
        let current = editor.screen_to_data(usize::from(row), 0);
        // 超出正文的屏幕行会映射到最后一行；重复位置即表示正文已经结束。
        if current == previous {
            return row;
        }
        previous = current;
    }
    max_rows
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
        self.flush_stream();
        self.events = original;
    }
}

#[cfg(test)]
mod tests;

fn dialog_guard<'a>(
    queue: &'a pp_core::interaction::DialogQueue,
    token: &CancellationToken,
) -> Result<pp_core::interaction::DialogPermit<'a>> {
    queue.acquire(token)
}
