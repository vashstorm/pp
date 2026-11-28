mod app;
mod ask;
mod attachment_edit;
mod auth;
#[cfg(test)]
mod auth_tests;
mod context_panel;
mod desktop_notifications;
mod dialog;
mod edit_diff;
mod files;
mod frame_clock;
mod help;
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
mod permissions;
mod pipes;
mod plan;
mod plan_leave;
mod plan_list;
mod render_worker;
mod resources;
mod roles;
mod scrollback;
mod sessions;
mod settings;
mod skills;
mod statusline;
mod stream_buffer;
mod subagents;
mod surface;
mod themes;
mod tips;
mod todo;
mod tool_display;
mod transcript;
mod transcript_style;
mod tree;
#[cfg(debug_assertions)]
mod web_search_test;
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
    // A failure or panic in the middle of initialization also restores terminal mode.
    let _guard = TerminalGuard;
    surface::initialize().context("initializing terminal")?;
    pp_core::logging::perf::mark("surface_ready");
    // Capability probing takes exclusive ownership of stdin, so it must finish before
    // crossterm first reads events.
    let (picker, mut initial_events, pending_reply) = image_terminal::detect();
    pp_core::logging::perf::mark("image_probe_ready");
    // tmux reports extended keys as xterm modifyOtherKeys by default; this takes over
    // fd 0 and translates them into the CSI-u form crossterm understands. Install it
    // after probing finishes so the raw terminal is not contended.
    input_shim::install(pending_reply).context("installing input translator")?;
    surface::enable_focus_reporting().context("enabling terminal focus reporting")?;
    themes::set_active_appearance(options.theme, options.icons, options.tool_style);
    let mut surface = surface::Surface::new(io::stdout());
    let mut canvas = ratatui::Terminal::new(ratatui::backend::TestBackend::new(1, 1))?;
    // Persistent input history lives next to the agent config directory; when no such
    // directory is locatable, recall is explicitly limited to this process.
    let history_path = bootstrap
        .as_ref()
        .map(|bootstrap| bootstrap.dir.join("history.db"));
    let settings_path = bootstrap
        .as_ref()
        .map(|bootstrap| bootstrap.dir.join("settings.yml"));
    let mut app = App::new_with_skills(agent, skills, file_commands, cwd, history_path)?;
    let notification_mode = options.desktop_notifications;
    let desktop_notifier = desktop_notifications::Worker::start(notification_mode);
    pp_core::logging::perf::mark("app_ready");
    app.apply_appearance(themes::Appearance {
        theme: options.theme,
        icons: options.icons,
        tools: options.tool_style,
    });
    app.theme = options.theme;
    app.icons = options.icons;
    app.tool_style = options.tool_style;
    app.settings_path = settings_path;
    app.image_view.configure(picker);
    app.cost_display = cost_display;
    app.bootstrap = bootstrap;
    app.bind_bootstrap_permissions();
    if auto_continue && matches!(app.state, State::Ready) {
        // -c: Execute keeps the existing continue behavior; Plan/Draft prompts for more
        // input; Plan/Ready opens review.
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
    // `pp -r`: opens the /resume session picker right after startup, equivalent to
    // submitting /resume while idle.
    if open_resume && matches!(app.state, State::Ready) {
        app.open_session_panel();
    }
    // `pp /COMMAND`: same path as pressing Enter to submit; `/quit` keeps the existing
    // shutdown cleanup.
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
    let mut first_frame = false;
    loop {
        // Adopt background results that already arrived first, then interpret the next
        // key batch against the newest state.
        dirty |= app.receive();
        if let Some(fps) = app.pending_max_fps.take() {
            clock = frame_clock::FrameClock::new(fps);
        }
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
        // Use the focus updated by this terminal input batch and drop conversations
        // closed within the same batch.
        if !quit {
            app.flush_desktop_notifications(notification_mode, |notice| {
                if let Some(worker) = &desktop_notifier {
                    worker.send(notice);
                }
            });
        }
        if let Some(fps) = app.pending_max_fps.take() {
            clock = frame_clock::FrameClock::new(fps);
        }
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
        dirty |= app.mcp_tip_tick();
        if quit {
            drop(desktop_notifier);
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
            if !first_frame {
                first_frame = true;
                pp_core::logging::perf::mark("first_frame");
            }
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

/// Geometry shared by the main screen and native-history commits; TODO never enters
/// the conversation viewport.
pub(in crate::tui) struct MainLayout {
    pub(in crate::tui) conversation: Rect,
    todo: Rect,
    subagents: Rect,
    ask: Rect,
    activity: Rect,
    // Automatic-retry notice box; always above the input box and never inside the
    // conversation area.
    retry: Rect,
    slash_completion: Option<Rect>,
    pub(in crate::tui) input: Rect,
    attachments: Rect,
    status: Rect,
    bordered: bool,
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
    // Input box and panel layout run once per frame: native batches and the full-frame
    // draw share the same geometry.
    let layout = (!alternate).then(|| app.main_layout(area));
    if !alternate {
        let conversation_height = layout.as_ref().expect("native layout").conversation.height;
        loop {
            let Some(batch) =
                app.conversation
                    .next_native_batch(width, conversation_height, exiting)
            else {
                if exiting && app.conversation.render_pending() {
                    thread::park_timeout(Duration::from_millis(1));
                    continue;
                }
                // Later batches may still appear while layout is pending, so the cursor
                // must not be shown between batches.
                if !app.conversation.render_pending() {
                    *rebuilding_history = false;
                }
                break;
            };
            app.conversation.preview_native_batch(&batch);
            let rendered = draw_frame(app, canvas, context, layout.as_ref(), exiting)?;
            let buffer = if exiting {
                app.normal_snapshot.as_ref().expect("normal frame").clone()
            } else {
                rendered.expect("rendered frame")
            };
            // History rebuilding spans multiple frames; the input cursor returns only
            // after every batch is committed.
            let cursor = (canvas.backend().cursor_visible() && !*rebuilding_history)
                .then(|| canvas.backend().cursor_position());
            surface.paint_images(buffer, cursor, false, &batch.rows, &app.image_view.graphics)?;
            app.conversation.acknowledge_native_batch(batch);
            if !exiting {
                return Ok(true);
            }
        }
    }
    let rendered = draw_frame(app, canvas, context, layout.as_ref(), exiting)?;
    let cursor = (canvas.backend().cursor_visible() && (alternate || !*rebuilding_history))
        .then(|| canvas.backend().cursor_position());
    let buffer = if exiting {
        app.normal_snapshot.as_ref().expect("normal frame").clone()
    } else {
        rendered.expect("rendered frame")
    };
    surface.paint_images(buffer, cursor, alternate, &[], &app.image_view.graphics)?;
    Ok(false)
}

/// Draws one frame: the exit path writes a full-frame snapshot, other paths keep a
/// single copy.
fn draw_frame(
    app: &mut App,
    canvas: &mut ratatui::Terminal<ratatui::backend::TestBackend>,
    context: &StatusContext,
    layout: Option<&MainLayout>,
    exiting: bool,
) -> Result<Option<std::sync::Arc<Buffer>>> {
    let mut rendered = None;
    canvas.draw(|frame| {
        app.draw_with_layout(frame, context, layout, exiting);
        if !exiting {
            rendered = Some(std::sync::Arc::new(frame.buffer_mut().clone()));
        }
    })?;
    Ok(rendered)
}

#[derive(Clone, Copy)]
enum State {
    ManagingMcp,
    Ready,
    Running,
    RunningShell,
    /// Run state used only by local tests; non-dev/test builds keep the state machine
    /// shape and simply compile the dedicated commands out.
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    RunningTools,
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    RunningWebSearch,
    Cancelling,
    Compacting,
    InspectingContext,
    Resetting,
    SwitchingModel,
    SavingModelSettings,
    // Only the `⌥ + S` / `Ctrl + S` global default save remains; session effort switches
    // no longer go through the worker.
    SavingThinkDefault,
    Resuming,
    ReadingTree,
    NavigatingTree,
    ListingPlans,
    ReadingListedPlan,
    OpeningListedPlan,
    RenamingPlan,
    DeletingPlan,
    Unavailable,
    ChangingPlan,
    ReadingPlan,
    PreparingExecution,
    // A standalone reviewer is running: approval/dispatch/commands all pause while the
    // draft stays editable.
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
    SandboxMode(pp_config::config::SandboxMode),
    DumpMode(pp_runtime::agent::DumpMode),
    PermissionMode(pp_config::config::PermissionMode),
    SettingTab(settings::Tab),
    ThemeIcon(pp_config::config::IconMode),
    ThemeColor(pp_config::config::Theme),
    ThemeTool(pp_config::config::ToolStyle),
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
    // Control reports and worker snapshots travel on different channels; an old
    // snapshot must not overwrite a newer applied selection.
    model_revision: u64,
    pending_model: Option<(u64, String)>,
    // Where the currently effective model came from; `Default` adds no status-line
    // noise.
    current_source: Option<ModelSource>,
    // Manual override snapshot for `a`; used only for `/roles` read-only display and
    // clearing, never for resolution.
    override_selection: Option<Selection>,
    // Desired effort: `0` means submitted but not yet acknowledged by the control
    // thread, otherwise it is the `ThinkQueued` revision. Effort switching is not a
    // worker command, so changes here never rewrite `state`.
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
    normal_snapshot: Option<std::sync::Arc<ratatui::buffer::Buffer>>,
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
    // Persistent input recall: a dedicated worker owns history.db and the startup cwd
    // stays fixed.
    history_worker: Option<history::Worker>,
    history_cwd: std::path::PathBuf,
    // Oldest loaded record id; None means the initial page has not returned yet or the
    // store is empty.
    history_oldest_id: Option<i64>,
    history_has_more: bool,
    // The LoadInputs request_id currently awaited; only one page request at a time.
    history_page_pending: Option<u64>,
    history_request_id: u64,
    // Up on the oldest loaded entry waits for the next page; repeated Up collapses into
    // one pending navigation.
    history_waiting_up: bool,
    // The redaction-failure notice fires once, so repeated appends do not re-pop it.
    history_failure_notified: bool,
    // Generation of the session search panel; increments monotonically on every open.
    session_generation: u64,
    // Temp directory owned by unit tests, so the history.db lifetime covers App.
    #[cfg(test)]
    history_dir: Option<tempfile::TempDir>,
    state: State,
    permission: pp_permission::Gate,
    sandbox: std::sync::Arc<pp_sandbox::Sandbox>,
    permission_mode: Option<pp_config::config::PermissionMode>,
    permission_worker: Option<permissions::Worker>,
    permission_overlay: Option<permissions::Overlay>,
    next_shell_id: u64,
    active_cancellation: Option<CancellationToken>,
    input_queue: InputQueue,
    queue_paused: bool,
    queue_menu: bool,
    queue_menu_revision: Option<u64>,
    queue_snapshot: Option<QueueSnapshot>,
    ask_panel: Option<dialog::Panel>,
    ask_reply: Option<mpsc::SyncSender<Vec<pp_core::interaction::AskAnswer>>>,
    desktop_notifications: desktop_notifications::Pending,
    ask_id: Option<String>,
    ask_cancellation: Option<CancellationToken>,
    commands: Option<Sender<WorkerCommand>>,
    events: Option<Receiver<WorkerEvent>>,
    // Isolated /test-* worker plus the normal display state saved before it started;
    // not part of normal session cleanup.
    #[cfg(debug_assertions)]
    mock_worker: Option<mock_test::Worker>,
    #[cfg(debug_assertions)]
    mock_saved: Option<mock_test::MockUiSnapshot>,
    #[cfg(debug_assertions)]
    web_search_worker: Option<web_search_test::Worker>,
    unavailable: Option<String>,
    choices: Vec<ModelChoice>,
    current_model: Option<ModelChoice>,
    default_model_reference: Option<String>,
    favorite_models: Vec<String>,
    // Most recent usage-ranking snapshot published by the worker; drawing and key
    // handling never read the database.
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
    cached_skills: Option<pp_config::skills::Skills>,
    theme: pp_config::config::Theme,
    icons: pp_config::config::IconMode,
    tool_style: pp_config::config::ToolStyle,
    settings_path: Option<std::path::PathBuf>,
    settings_worker: settings::Worker,
    settings_panel: Option<settings::Panel>,
    cached_settings: Option<pp_config::config::Settings>,
    pending_max_fps: Option<u16>,
    themes_worker: themes::Worker,
    themes_panel: Option<themes::Panel>,
    session_panel: Option<sessions::Panel>,
    tree_panel: Option<tree::Panel>,
    context_panel: Option<context_panel::Panel>,
    context_snapshot: Option<pp_runtime::agent::context_diagnosis::Snapshot>,
    subagent_panel: Option<subagents::Panel>,
    subagent_autowake: bool,
    // Observation revision of the current root and the number of running subagents.
    subagent_status: Option<(u64, usize)>,
    subagent_control: std::sync::Arc<std::sync::Mutex<Option<pp_runtime::subagent::Binding>>>,
    // Hub observer thread plus the root/view epoch; drafts live only inside the
    // current root.
    hub_worker: Option<subagents::Worker>,
    hub_root_epoch: u64,
    hub_view_epoch: u64,
    hub_request: u64,
    hub_bound: Option<pp_runtime::subagent::Binding>,
    hub_attached: bool,
    hub_projection: subagents::Projection,
    hub_preload_error: Option<String>,
    hub_sends: BTreeMap<u64, app::hub::HubSend>,
    child_drafts: subagents::Drafts,
    /// Temp pipe registry of the root session most recently published by the worker;
    /// the panel uses it to detect a root switch.
    pipe_store: Option<pp_bash::PipeStore>,
    /// Root session instance generation; increments whenever `same_instance` reports a
    /// root switch.
    pipe_store_epoch: u64,
    pipes_panel: Option<pipes::Panel>,
    pipes_worker: Option<pipes::Worker>,
    /// Panel view generation; increments when a run/stream is selected or the panel
    /// closes, and stale replies are dropped immediately.
    pipes_view_epoch: u64,
    plan_list_panel: Option<plan_list::Panel>,
    // The follow-up step a list action runs after reading the selection.
    plan_list_pending: Option<plan_list::PendingOp>,
    // `n` unbinds first in Plan mode (LeavePlan), then re-enters Plan standby on
    // success.
    plan_reenter_standby: bool,
    plan_leave_confirmation: Option<plan_leave::Panel>,
    escape_tap: EscapeTap,
    session_id: Option<String>,
    // Most recent ContextUpdated snapshot; rendering uses it directly and never
    // re-estimates per frame.
    usage: Option<pp_core::context::ContextUsage>,
    token_speed: statusline::TokenSpeed,
    // Fixed notice box during automatic retry waits; never enters the conversation or
    // the history.
    retry_notice: Option<app::retry::Notice>,
    // Startup MCP connect hints: multi-line below the welcome page, yielding after the
    // first task.
    mcp_tip: Vec<String>,
    mcp_tip_dismissed: bool,
    task_status_view: pp_config::task_status::View,
    task_status: Option<pp_config::task_status::Snapshot>,
    cost_display: pp_config::config::CostDisplay,
    lsp_cleanup: Option<pp_lsp::Cleanup>,
    mcp_view: pp_mcp::View,
    mcp_panel: Option<mcp::Panel>,
    background_cleanup: Option<pp_bash::BackgroundCleanup>,
    file_worker: files::Worker,
    file_paths: Vec<String>,
    file_directory: String,
    file_query: Option<files::Query>,
    files_pending: bool,
    file_scan_root: String,
    file_commands: commands::FileCommands,
    // Workflow snapshot published by the worker; the UI is not the permission
    // authority and uses it only for display and dispatch guards.
    workflow: WorkflowState,
    plan_panel: Option<plan::Panel>,
    // Once a discussion started from the plan panel finishes and the queue drains,
    // safely re-read and restore the panel.
    reopen_plan_after_discussion: bool,
    // (completed, total) progress cache recomputed only when the body changes.
    plan_progress: (usize, usize),
    // Display error produced by the replay redraw in ExecutionPrepared, notified together
    // with the waiting SessionResumed.
    resume_note: Option<String>,
    // EnterPlan failed: the next /plan re-sends EnterPlan first to retry.
    plan_retry_enter: bool,
    // After EnterPlan succeeds, hint standby or send the prompt; on failure nothing may
    // be sent.
    plan_after_enter: Option<PlanEnterAction>,
    // Temporary transcript of the standalone review tab; review progress never enters
    // the main conversation and never updates usage.
    review_transcript: Option<Transcript>,
    // Final report awaiting the next ReadPlan after a successful review (refresh the
    // panel or degrade to plain display).
    plan_review_pending: Option<PlanReviewReport>,
}

enum PlanEnterAction {
    // Standby hint: entering Plan neither binds the old draft nor opens the review
    // panel.
    Standby,
    Prompt(String, Option<pp_runtime::user_input::UserInput>),
}

fn editor() -> TextArea<'static> {
    let mut editor = TextArea::default();
    editor.set_style(Style::default().fg(transcript_style::text()));
    editor.set_wrap_mode(WrapMode::WordOrGlyph);
    editor.set_cursor_line_style(Style::default());
    editor.set_cursor_style(Style::default());
    editor.set_max_histories(1000);
    editor
}

/// Screen rows the editor occupies at a given width; the main input box and child
/// details share the same wrap measurement.
pub(in crate::tui) fn input_rows(editor: &TextArea<'_>, width: u16, max_rows: u16) -> u16 {
    if width == 0 || max_rows == 0 {
        return max_rows;
    }
    // Refresh the widget's own wrap map in a bounded in-memory buffer, keeping words,
    // wide characters and Tab rules identical.
    let area = Rect::new(0, 0, width, max_rows);
    editor.render(area, &mut Buffer::empty(area));
    let mut previous = editor.screen_to_data(0, 0);
    for row in 1..max_rows {
        let current = editor.screen_to_data(usize::from(row), 0);
        // Screen rows past the body map to the last line; a repeated position means the
        // body has ended.
        if current == previous {
            return row;
        }
        previous = current;
    }
    max_rows
}

/// Display form of the rate-limit wait: whole seconds without decimals, otherwise one
/// decimal place.
pub(crate) fn format_wait(wait_ms: u64) -> String {
    if wait_ms.is_multiple_of(1000) {
        format!("{}s", wait_ms / 1000)
    } else {
        format!("{:.1}s", wait_ms as f64 / 1000.0)
    }
}

/// Derives the lifecycle directory from the registered relative path; read-only display
/// only, not an authorization basis.
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
