mod ask;
mod files;
mod markdown;
mod menu;
mod notification;
mod scrollback;
mod sessions;
mod skills;
mod statusline;
mod transcript;
mod transcript_style;
mod tree;
mod welcome;

use crate::{
    agent::{Agent, context::CompactionOutcome},
    cancellation::{CancellationToken, RunOutcome},
    commands::{self, COMMANDS, Command},
    config::{ModelChoice, NotificationOptions, ThinkLevel},
    events::AgentEvent,
};
use anyhow::{Context, Result};
use crossterm::{
    SynchronizedUpdate,
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
};
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

const WARNING: &str = "write/edit only modify files inside the startup directory; bash has no directory isolation, so run in a trusted or temporary directory.\nForeground bash has an execution timeout; managed background tasks survive tool calls and stop at this user task's end or controlled exit. Ctrl-C stops managed background groups, but cleanup of other foreground children is not guaranteed. No shell/file side-effect rollback; deliberately detached descendants are not covered.";
const SHORTCUTS: &str = "Enter send · ↑↓ move/history/menu · Shift-Tab cycle thinking · Ctrl-S save selected model/effort as default in its menu · Ctrl-P choose model · F1 help · Tab complete · Esc cancel task/dismiss · Esc Esc session tree (empty input) · Ctrl-A/E line start/end · Ctrl-K delete to end of line · Ctrl-J newline · Ctrl-O expand/collapse bash/skill/diff · Wheel/PgUp/PgDn scroll · Ctrl-C quit";
const EFFORT_HINT: &str = "Enter select · Ctrl-S set selected effort as default · Esc cancel";
const MODEL_HINT: &str = "Enter select · Ctrl-S set selected model as default · Esc cancel";
const MOUSE_SCROLL_LINES: usize = 3;

pub(crate) fn run(
    agent: Result<Agent>,
    cwd: &Path,
    skills: crate::skills::SharedSkills,
    file_commands: commands::FileCommands,
    auto_continue: bool,
    notification_options: NotificationOptions,
) -> Result<()> {
    // Construct before try_init so partially initialized terminals are restored too.
    let guard = TerminalGuard;
    let mut terminal = ratatui::try_init().context("initializing terminal")?;
    execute!(io::stdout(), EnableBracketedPaste, EnableMouseCapture)
        .context("enabling terminal input modes")?;
    let mut app = App::new_with_skills(agent, skills, file_commands, cwd, notification_options)?;
    if auto_continue && matches!(app.state, State::Ready) {
        // -c: replay is already loaded; resend the saved request without touching the editor.
        app.send_task(None);
    }
    let mut workspace = WorkspaceStatus::new(cwd)?;
    let mut dirty = true;
    loop {
        dirty |= app.receive();
        dirty |= workspace.receive();
        dirty |= app.notifications.tick(Instant::now());
        dirty |= app.conversation.tick(Instant::now());
        dirty |= app.token_speed.tick(Instant::now());
        if dirty {
            terminal
                .draw(|frame| app.draw(frame, &workspace.context))
                .context("drawing terminal")?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(50)).context("polling terminal input")? {
            let event = event::read().context("reading terminal input")?;
            if app.handle(event, &mut dirty) {
                app.shutdown_background_tasks()?;
                app.tree_panel = None;
                app.notifications.clear();
                // 仅在内存中准备退出快照，避免退出前额外刷新备用屏幕。
                let snapshot = {
                    let mut frame = terminal.get_frame();
                    app.draw(&mut frame, &workspace.context);
                    frame.buffer_mut().clone()
                };
                // 屏幕切换与完整历史回放共用一次同步更新，隐藏中间画面。
                return io::stdout()
                    .lock()
                    .sync_update(|stdout| {
                        drop(terminal);
                        drop(guard);
                        app.conversation.write_scrollback(stdout, &snapshot)
                    })
                    .context("synchronizing terminal exit")?
                    .context("writing terminal transcript");
            }
        }
    }
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        ratatui::restore();
    }
}

enum WorkerCommand {
    Run {
        prompt: String,
        cancellation: CancellationToken,
    },
    Continue {
        cancellation: CancellationToken,
    },
    Compact {
        cancellation: CancellationToken,
    },
    Reset,
    SwitchModel(String),
    SetThinkLevel(ThinkLevel),
    SaveDefaultThinkLevel(ThinkLevel),
    SaveDefaultModel(String),
    ListSessions,
    DeleteSession(String),
    Resume(String),
    ReadTree,
    NavigateTree(String),
}
enum WorkerEvent {
    Ask {
        request: crate::interaction::AskRequest,
        reply: mpsc::SyncSender<Vec<crate::interaction::AskAnswer>>,
    },
    Progress(AgentEvent),
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
    SessionsListed(Result<crate::agent::persistence::SessionListing>),
    SessionDeleted(String, Result<()>),
    SessionResumed(Result<ResumeState>),
    TreeRead(Result<crate::agent::persistence::tree::View>),
    TreeNavigated(Result<crate::agent::TreeNavigation>),
}

fn request_ask(
    events: &mpsc::SyncSender<WorkerEvent>,
    request: crate::interaction::AskRequest,
    cancellation: &CancellationToken,
) -> Result<Vec<crate::interaction::AskAnswer>> {
    crate::cancellation::check(cancellation)?;
    let (reply, answers) = mpsc::sync_channel(1);
    if events.send(WorkerEvent::Ask { request, reply }).is_err() {
        cancellation.cancel();
        return Err(crate::cancellation::Cancelled.into());
    }
    loop {
        crate::cancellation::check(cancellation)?;
        match answers.recv_timeout(Duration::from_millis(50)) {
            Ok(answers) => {
                crate::cancellation::check(cancellation)?;
                return Ok(answers);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                cancellation.cancel();
                return Err(crate::cancellation::Cancelled.into());
            }
        }
    }
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
struct ResumeState {
    id: String,
    replay: Vec<crate::agent::persistence::ReplayRecord>,
}

#[derive(Clone, Copy)]
enum State {
    Ready,
    Running,
    Cancelling,
    Compacting,
    Resetting,
    SwitchingModel,
    SavingDefaultModel,
    SettingThinkLevel,
    ListingSessions,
    Resuming,
    ReadingTree,
    NavigatingTree,
    Unavailable,
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
}

struct ModelPickerDraft {
    input: TextArea<'static>,
    history_index: Option<usize>,
    history_draft: Option<TextArea<'static>>,
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
    notifications: Notifications,
    show_welcome: bool,
    input: TextArea<'static>,
    input_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: Option<TextArea<'static>>,
    state: State,
    active_cancellation: Option<CancellationToken>,
    ask_panel: Option<ask::Panel>,
    ask_reply: Option<mpsc::SyncSender<Vec<crate::interaction::AskAnswer>>>,
    commands: Option<Sender<WorkerCommand>>,
    events: Option<Receiver<WorkerEvent>>,
    unavailable: Option<String>,
    choices: Vec<ModelChoice>,
    current_model: Option<ModelChoice>,
    default_model_reference: Option<String>,
    think_level: ThinkLevel,
    default_think_level: ThinkLevel,
    menu: Option<SelectionMenu<MenuAction>>,
    menu_dismissed: bool,
    model_picker_draft: Option<ModelPickerDraft>,
    secondary_status: Option<String>,
    skill_worker: skills::Worker,
    skill_panel: Option<skills::Panel>,
    skills_pending: bool,
    session_panel: Option<sessions::Panel>,
    tree_panel: Option<tree::Panel>,
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
            conversation: Transcript::new(),
            notifications: Notifications::new(notification_options),
            show_welcome: true,
            input: editor(),
            input_history: Vec::new(),
            history_index: None,
            history_draft: None,
            state: State::Unavailable,
            active_cancellation: None,
            ask_panel: None,
            ask_reply: None,
            commands: None,
            events: None,
            unavailable: None,
            choices: Vec::new(),
            current_model: None,
            default_model_reference: None,
            think_level: ThinkLevel::None,
            default_think_level: ThinkLevel::default(),
            menu: None,
            menu_dismissed: false,
            model_picker_draft: None,
            secondary_status: None,
            skill_worker: skills::Worker::new(skills)?,
            skill_panel: None,
            skills_pending: false,
            session_panel: None,
            tree_panel: None,
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
                app.default_model_reference = Some(agent.default_model_reference().to_owned());
                let (commands, receiver) = mpsc::channel();
                let (sender, events) = mpsc::sync_channel(256);
                let ask_events = sender.clone();
                agent.set_ask_handler(Box::new(move |request, cancellation| {
                    request_ask(&ask_events, request, cancellation)
                }));
                let usage = agent.context_usage().ok();
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
                            let mut snapshot = None;
                            let event = match command {
                                WorkerCommand::Run {
                                    prompt,
                                    cancellation,
                                } => WorkerEvent::completed(
                                    started,
                                    agent.run(&prompt, &cancellation, &mut emit),
                                ),
                                WorkerCommand::Continue { cancellation } => WorkerEvent::completed(
                                    started,
                                    agent.continue_session(&cancellation, &mut emit),
                                ),
                                WorkerCommand::Compact { cancellation } => {
                                    let result = agent.compact(&cancellation, &mut emit);
                                    // 终态事件之前先发布最终用量快照；
                                    // 成功、失败、取消、未变化都会刷新状态栏。
                                    if let Some(event) = usage_snapshot(&agent)
                                        && sender.send(event).is_err()
                                    {
                                        break;
                                    }
                                    WorkerEvent::Compacted(result)
                                }
                                WorkerCommand::Reset => {
                                    agent.reset();
                                    snapshot = usage_snapshot(&agent);
                                    WorkerEvent::Reset
                                }
                                WorkerCommand::SetThinkLevel(level) => {
                                    let result =
                                        agent.set_think_level(level).map(|()| agent.think_level());
                                    if result.is_ok() {
                                        snapshot = usage_snapshot(&agent);
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
                                WorkerCommand::SwitchModel(reference) => {
                                    let result = agent.switch_model(&reference).map(|()| {
                                        (agent.current_model().clone(), agent.think_level())
                                    });
                                    if result.is_ok() {
                                        snapshot = usage_snapshot(&agent);
                                    }
                                    WorkerEvent::ModelSwitched(result)
                                }
                                WorkerCommand::ReadTree => {
                                    WorkerEvent::TreeRead(agent.session_tree())
                                }
                                WorkerCommand::NavigateTree(id) => {
                                    let result = agent.navigate_tree(&id);
                                    if result.is_ok() {
                                        snapshot = usage_snapshot(&agent);
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
                                WorkerCommand::Resume(id) => {
                                    let result = agent.resume(&id).map(|()| ResumeState {
                                        id,
                                        replay: agent.take_replay(),
                                    });
                                    if result.is_ok() {
                                        snapshot = usage_snapshot(&agent);
                                    }
                                    WorkerEvent::SessionResumed(result)
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
                            if let Some(event) = snapshot
                                && sender.send(event).is_err()
                            {
                                break;
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
        self.conversation = Transcript::new();
        self.input_history = Vec::new();
        let error = self.conversation.replay(replay, &mut self.input_history);
        self.input = editor();
        self.history_index = None;
        self.history_draft = None;
        self.model_picker_draft = None;
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
        self.active_cancellation = None;
        self.secondary_status = None;
        self.commands = None;
        self.events = None;
        self.unavailable = Some(message.into());
        self.tree_panel = None;
        self.escape_tap.reset();
        // worker 已死时不再等待恢复回复，会话面板不能继续吞掉按键。
        self.session_panel = None;
        self.conversation.fail();
        self.notify(Level::Error, message.into());
    }

    fn receive(&mut self) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.skill_worker.replies.try_recv() {
            self.skills_pending = false;
            match reply.result {
                Ok(snapshot) if !reply.saved => {
                    self.skill_panel = Some(skills::Panel::new(snapshot));
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
                            self.token_speed.progress(output_bytes)
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
                        event => self.conversation.progress(event),
                    }
                    changed = true;
                }
                Ok(WorkerEvent::SessionStarted(id)) => {
                    self.notify(Level::Normal, format!("Session: {id}"));
                    self.session_id = Some(id);
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
                    let label = match result {
                        Ok(RunOutcome::Completed) => "Completed",
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
                    self.conversation = Transcript::new();
                    self.clear_input();
                    self.notifications.clear();
                    self.show_welcome = true;
                    self.state = State::Ready;
                    self.session_id = None;
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
                            let id = state.id;
                            let error = self.adopt_history(state.replay);
                            self.session_id = Some(id.clone());
                            self.session_panel = None;
                            // 恢复只重新估算当前请求；不沿用上一会话的用量。
                            self.usage = None;
                            self.token_speed.clear();
                            match error {
                                Some(error) => self.notify(
                                    Level::Error,
                                    format!("Resumed session: {id}\n{error}"),
                                ),
                                None => {
                                    self.notify(Level::Normal, format!("Resumed session: {id}"))
                                }
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
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.fail_worker();
                    return true;
                }
            }
        }
        if changed {
            self.escape_tap.reset();
            self.refresh_candidates(false);
        }
        changed
    }

    fn refresh_candidates(&mut self, edited: bool) {
        if edited {
            self.menu_dismissed = false;
        }
        if self.skill_panel.is_some()
            || self.skills_pending
            || self.session_panel.is_some()
            || self.tree_panel.is_some()
            || self.menu_dismissed
            || !matches!(self.state, State::Ready | State::Unavailable)
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
        if self.model_picker_draft.is_some() && !matches!(argument, Some(("/model", _))) {
            self.file_query = None;
            self.menu = None;
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
            Some(("/model", argument)) if matches!(self.state, State::Ready) => {
                let prefix = argument.trim();
                let items: Vec<_> = self
                    .choices
                    .iter()
                    .enumerate()
                    .filter(|(_, choice)| {
                        choice.reference.starts_with(prefix) || choice.model.starts_with(prefix)
                    })
                    .map(|(index, choice)| MenuItem {
                        value: MenuAction::Model(index),
                        label: if self.default_model_reference.as_deref()
                            == Some(choice.reference.as_str())
                        {
                            format!("{} (default)", choice.reference)
                        } else {
                            choice.reference.clone()
                        },
                        description: format!("{} [{}]", choice.model, choice.protocol),
                        icon: None,
                        current: self
                            .current_model
                            .as_ref()
                            .is_some_and(|current| current.reference == choice.reference),
                    })
                    .collect();
                preferred = items
                    .iter()
                    .position(|item| {
                        matches!(item.value, MenuAction::Model(index) if self.choices[index].reference == prefix)
                    })
                    .or_else(|| items.iter().position(|item| item.current))
                    .unwrap_or(0);
                items
            }
            None if input.trim().starts_with('/') => COMMANDS
                .iter()
                .enumerate()
                .filter(|(_, spec)| spec.name.starts_with(input.trim()))
                .map(|(index, spec)| MenuItem {
                    value: MenuAction::Command(index),
                    label: spec.name.trim_start_matches('/').into(),
                    description: spec.description.into(),
                    icon: Some(spec.icon),
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
                            label: command.name.trim_start_matches('/').into(),
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
            let hint = match items.first().map(|item| item.value) {
                Some(MenuAction::Effort(_)) => Some(EFFORT_HINT),
                Some(MenuAction::Model(_)) => Some(MODEL_HINT),
                _ => None,
            };
            let menu = SelectionMenu::new(items, preferred);
            self.menu = Some(if let Some(hint) = hint {
                menu.with_hint(hint)
            } else {
                menu
            });
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

    fn clear_input(&mut self) {
        self.input = editor();
        self.history_index = None;
        self.history_draft = None;
        self.refresh_candidates(true);
    }

    fn remember_input(&mut self) {
        let text = self.input.lines().join("\n");
        if !text.trim().is_empty() {
            self.input_history.push(text);
        }
        self.history_index = None;
        self.history_draft = None;
    }

    fn navigate_history(&mut self, previous: bool) {
        let index = match (self.history_index, previous) {
            (None, true) if !self.input_history.is_empty() => {
                self.history_draft = Some(self.input.clone());
                self.input_history.len() - 1
            }
            (Some(0), true) => return,
            (Some(index), true) => index - 1,
            (Some(index), false) if index + 1 < self.input_history.len() => index + 1,
            (Some(_), false) => {
                self.input = self.history_draft.take().unwrap_or_else(editor);
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
        self.input.insert_str(&self.input_history[index]);
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

    fn send_task(&mut self, prompt: Option<String>) -> bool {
        let cancellation = CancellationToken::new();
        let command = match prompt {
            Some(prompt) => WorkerCommand::Run {
                prompt,
                cancellation: cancellation.clone(),
            },
            None => WorkerCommand::Continue {
                cancellation: cancellation.clone(),
            },
        };
        if self.send(command, State::Running) {
            self.active_cancellation = Some(cancellation);
            self.conversation.begin_run();
            true
        } else {
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

    fn open_tree(&mut self) {
        self.escape_tap.reset();
        if !matches!(self.state, State::Ready)
            || self.skills_pending
            || self.skill_panel.is_some()
            || self.session_panel.is_some()
            || self.ask_panel.is_some()
            || self.tree_panel.is_some()
        {
            return;
        }
        self.menu = None;
        self.file_query = None;
        self.tree_panel = Some(tree::Panel::new());
        self.send(WorkerCommand::ReadTree, State::ReadingTree);
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
        self.append("Tips", SHORTCUTS);
        self.append("Help", WARNING);
    }

    fn shortcut_ready(&mut self) -> bool {
        if self.skills_pending {
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
        if self.model_picker_draft.is_some() || !self.shortcut_ready() {
            return;
        }
        self.model_picker_draft = Some(ModelPickerDraft {
            input: std::mem::replace(&mut self.input, editor()),
            history_index: self.history_index.take(),
            history_draft: self.history_draft.take(),
            menu: self.menu.take(),
            menu_dismissed: self.menu_dismissed,
        });
        self.input.insert_str("/model ");
        self.refresh_candidates(true);
    }

    fn restore_model_picker_draft(&mut self) {
        if let Some(draft) = self.model_picker_draft.take() {
            self.input = draft.input;
            self.history_index = draft.history_index;
            self.history_draft = draft.history_draft;
            self.menu = draft.menu;
            self.menu_dismissed = draft.menu_dismissed;
            self.refresh_candidates(false);
        }
    }

    fn switch_model(&mut self, reference: String) {
        if self.model_picker_draft.is_some() {
            self.send(WorkerCommand::SwitchModel(reference), State::SwitchingModel);
            self.restore_model_picker_draft();
        } else if self.send_submission(WorkerCommand::SwitchModel(reference), State::SwitchingModel)
        {
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

    fn submit(&mut self) -> bool {
        if self.skills_pending || self.skill_panel.is_some() {
            return false;
        }
        let prompt = self.input.lines().join("\n");
        if prompt.trim().is_empty() {
            return false;
        }
        let command = match commands::parse(&prompt, &self.file_commands) {
            Ok(command) => command,
            Err(error) => {
                self.notify(Level::Error, format!("{error:#}"));
                return false;
            }
        };
        if self.model_picker_draft.is_some() && !matches!(command, Some(Command::Model(_))) {
            self.notify(
                Level::Error,
                "The model picker only accepts /model; press Esc to restore your draft.".into(),
            );
            return false;
        }
        match command {
            Some(Command::Quit) => return true,
            Some(Command::Help) => {
                self.show_help();
                self.remember_input();
                self.clear_input();
                return false;
            }
            _ => {}
        }
        if let Some(Command::Skills(selection)) = command {
            if !matches!(self.state, State::Ready | State::Unavailable) {
                return false;
            }
            let request = match selection {
                None => skills::Request::Open,
                Some((name, enabled)) => skills::Request::Save(vec![(name.to_owned(), enabled)]),
            };
            self.request_skills(request);
            return false;
        }
        match self.state {
            State::Running
            | State::Cancelling
            | State::Compacting
            | State::Resetting
            | State::SwitchingModel
            | State::SavingDefaultModel
            | State::SettingThinkLevel
            | State::ListingSessions
            | State::Resuming
            | State::ReadingTree
            | State::NavigatingTree => {}
            State::Unavailable => {
                if let Some(error) = self.unavailable.take() {
                    self.notify(Level::Error, error.clone());
                    self.unavailable = Some(error);
                }
            }
            State::Ready => match command {
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
                Some(Command::Model(None)) => {
                    self.input = editor();
                    self.input.insert_str("/model ");
                    self.refresh_candidates(true);
                }
                None => {
                    if self.send_task(Some(prompt.clone())) {
                        self.remember_input();
                        self.append("You", &prompt);
                        self.clear_input();
                    }
                }
                Some(Command::Prompt(expanded)) => {
                    // /init 回显用户命令；文件命令继续回显展开正文，输入历史均保留原文。
                    if self.send_task(Some(expanded.clone())) {
                        self.remember_input();
                        self.append(
                            "You",
                            if prompt.trim() == "/init" {
                                &prompt
                            } else {
                                &expanded
                            },
                        );
                        self.clear_input();
                    }
                }
                Some(Command::Help | Command::Quit | Command::Skills(_)) => unreachable!(),
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
        if matches!(
            action,
            MenuAction::File(_) | MenuAction::FileDirectory(_) | MenuAction::FileParent
        ) && (self.file_query.is_none() || files::query(&self.input) != self.file_query)
        {
            self.refresh_candidates(false);
            return false;
        }
        match action {
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
                return confirm && self.submit();
            }
            MenuAction::FileCommand(index) => {
                self.input = editor();
                self.input
                    .insert_str(self.file_commands.entries[index].name.as_str());
                self.refresh_candidates(true);
                return confirm && self.submit();
            }
            MenuAction::Model(index) if confirm => {
                let choice = &self.choices[index];
                if self
                    .current_model
                    .as_ref()
                    .is_some_and(|current| current.reference == choice.reference)
                {
                    if self.model_picker_draft.is_some() {
                        self.restore_model_picker_draft();
                    } else {
                        self.remember_input();
                        self.clear_input();
                    }
                } else {
                    self.switch_model(choice.reference.clone());
                }
            }
            MenuAction::Effort(level) if confirm => self.set_think_level(level),
            MenuAction::Model(index) => {
                self.input = editor();
                self.input
                    .insert_str(format!("/model {}", self.choices[index].reference));
                self.refresh_candidates(true);
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
                if self.ask_panel.is_some()
                    || self.skill_panel.is_some()
                    || self.session_panel.is_some()
                    || self.menu.is_some()
                    || self.model_picker_draft.is_some()
                    || self.file_query.is_some()
                {
                    self.escape_tap.reset();
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
                                if !self.send(WorkerCommand::Resume(id), State::Resuming) {
                                    self.session_panel = None;
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
                if key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL {
                    if key.kind == KeyEventKind::Press
                        && let Some(action) = self.menu.as_ref().and_then(|menu| {
                            menu.selected_index().map(|index| menu.items()[index].value)
                        })
                        && self.shortcut_ready()
                    {
                        let request = match action {
                            MenuAction::Effort(level) => Some((
                                WorkerCommand::SaveDefaultThinkLevel(level),
                                State::SettingThinkLevel,
                            )),
                            MenuAction::Model(index) => Some((
                                WorkerCommand::SaveDefaultModel(
                                    self.choices[index].reference.clone(),
                                ),
                                State::SavingDefaultModel,
                            )),
                            _ => None,
                        };
                        if let Some((command, state)) = request
                            && self.send(command, state)
                        {
                            if self.model_picker_draft.is_some() {
                                self.restore_model_picker_draft();
                            } else {
                                self.clear_input();
                            }
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
                if key.code == KeyCode::Char('p') && key.modifiers == KeyModifiers::CONTROL {
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
                if key.code == KeyCode::Esc && self.model_picker_draft.is_some() {
                    self.restore_model_picker_draft();
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
                        KeyCode::Char('o') => {
                            self.conversation.toggle_tool_output();
                            return false;
                        }
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
                    if key.kind == KeyEventKind::Press
                        && matches!(self.state, State::Running | State::Compacting)
                        && let Some(cancellation) = &self.active_cancellation
                    {
                        cancellation.cancel();
                        if matches!(self.state, State::Compacting) {
                            self.secondary_status = Some("Cancelling context...".into());
                        } else {
                            self.conversation.cancelling();
                        }
                        self.state = State::Cancelling;
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
                match key.code {
                    KeyCode::Enter => {
                        if key.kind == KeyEventKind::Press && self.submit() {
                            return true;
                        }
                    }
                    KeyCode::PageUp => self.conversation.page_up(),
                    KeyCode::PageDown => self.conversation.page_down(),
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
                        self.refresh_candidates(edited);
                    }
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
            Event::Paste(text) => {
                self.input
                    .insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
                self.refresh_candidates(true);
                *dirty = true;
            }
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.conversation.scroll_up(MOUSE_SCROLL_LINES);
                    *dirty = true;
                }
                MouseEventKind::ScrollDown => {
                    self.conversation.scroll_down(MOUSE_SCROLL_LINES);
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
        let area = frame.area();
        if area.width == 0 || area.height == 0 {
            return;
        }
        if let Some(panel) = &mut self.skill_panel {
            panel.draw(frame, area);
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
            );
            return;
        }
        if let Some(panel) = &mut self.session_panel {
            panel.draw(frame, area);
            self.notifications.draw(
                frame,
                Rect::new(area.x, area.y, area.width, area.height.saturating_sub(2)),
            );
            return;
        }
        if let Some(panel) = &mut self.tree_panel
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
        let menu_height = self.menu.as_ref().map_or(0, |menu| {
            menu.height(available.saturating_sub(input_height))
        });
        let conversation_height = available.saturating_sub(input_height + menu_height);
        let conversation = Rect::new(area.x, area.y, area.width, conversation_height);
        let input = Rect::new(
            area.x,
            area.y.saturating_add(conversation_height),
            area.width,
            input_height,
        );
        let status = Rect::new(
            area.x,
            input.bottom().saturating_add(menu_height),
            area.width,
            status_height,
        );
        let conversation = if self.show_welcome {
            welcome::draw(frame, conversation)
        } else {
            conversation
        };
        self.conversation.draw(frame, conversation);
        if let Some(panel) = &self.tree_panel {
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
                .border_style(Style::default().fg(if self.menu.is_some() {
                    menu::SEPARATOR
                } else {
                    statusline::ACCENT
                }));
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
            menu.draw(
                frame,
                Rect::new(area.x, input.bottom(), area.width, menu_height),
            );
        }
        if status.height > 0 {
            frame.render_widget(
                statusline::primary(
                    self.current_model.as_ref(),
                    self.think_level,
                    context,
                    status.width,
                ),
                status,
            );
        }
        if let Some(panel) = &mut self.ask_panel {
            panel.draw(
                frame,
                Rect::new(area.x, area.y, area.width, input.y.saturating_sub(area.y)),
            );
        }
        self.notifications.draw(
            frame,
            Rect::new(area.x, area.y, area.width, input.y.saturating_sub(area.y)),
        );
    }

    fn draw_input_status(&self, frame: &mut Frame, area: Rect) {
        // 上下文固定在右端；压缩等临时状态显示在它左侧，空间不足时优先保留上下文。
        let mut right = area.right();
        if let Some(usage) = self.usage.as_ref() {
            let (text, color) = statusline::context_field(usage);
            let line = Line::styled(format!(" {text} "), Style::default().fg(color))
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
        if let Some(usage) = self.usage.as_ref() {
            let metrics = statusline::metric_fields(
                usage.last_request.as_ref(),
                self.token_speed.value(Instant::now()),
                (right - area.x).saturating_sub(extra_width),
            );
            let width = u16::try_from(metrics.width()).unwrap_or(u16::MAX);
            right -= width;
            frame.render_widget(metrics, Rect::new(right, area.y, width, 1));
        }
        if let Some(extra) = extra {
            frame.render_widget(
                extra,
                Rect::new(right - extra_width, area.y, extra_width, 1),
            );
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
        app.handle(control('p'), &mut dirty);
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

    #[test]
    fn ask_focus_submission_keeps_main_draft_and_sends_once() {
        let mut app = app();
        app.input.insert_str("主草稿🙂");
        app.input.move_cursor(CursorMove::Back);
        let cursor = app.input.cursor();
        let (answers, token) = ask_fixture(&mut app);
        let mut dirty = false;
        app.handle(control('p'), &mut dirty);
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
            Event::Key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
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
            .write_scrollback(&mut scrollback, &snapshot)
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
        assert!(screen.contains("█▀▀█  █▀▀█"));
        assert!(!screen.contains("configuration unavailable"));
        assert!(!screen.contains("channel closed"));
        assert_eq!(app.input.lines(), &["草稿"]);
    }

    fn menu_values(app: &App) -> Vec<MenuAction> {
        app.menu.as_ref().map_or_else(Vec::new, |menu| {
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
        app.clear_input();
        app.handle(Event::Paste("@".into()), &mut dirty);
        scans.try_recv().unwrap();
        app.handle(control('p'), &mut dirty);
        assert!(app.file_query.is_none());
        replies.send(Ok(vec!["src/main.rs".into()])).unwrap();
        app.receive();
        assert_eq!(app.input.lines(), &["/model "]);
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
            (State::Running, "busy draft"),
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
        assert!(screen.contains("█▀▀█  █▀▀█"));
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
    fn model_space_menu_renders_below_input_and_preserves_selection_on_resize() {
        let mut app = app();
        app.state = State::Ready;
        app.choices = (0..12)
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
        assert_eq!(menu_values(&app), vec![MenuAction::Command(1)]);
        app.handle(key(KeyCode::Char(' ')), &mut dirty);
        let screen = draw(&mut app, 100, 30);
        assert!(screen.find("❯ /model").unwrap() < screen.find("fixture/model-0").unwrap());
        assert!(screen.contains("❯ fixture/model-2"));
        assert!(
            screen
                .replace(' ', "")
                .contains("vendor-2[response](current)")
        );
        assert!(!screen.contains('┌'));
        app.handle(key(KeyCode::PageDown), &mut dirty);
        assert_eq!(app.menu.as_ref().unwrap().selected_index(), Some(10));
        for (width, height) in [(20, 6), (1, 1), (100, 30)] {
            draw(&mut app, width, height);
            assert_eq!(app.input.lines(), &["/model "]);
            assert_eq!(app.menu.as_ref().unwrap().selected_index(), Some(10));
        }
        app.handle(key(KeyCode::End), &mut dirty);
        assert!(draw(&mut app, 100, 30).contains("❯ fixture/model-11"));
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::SwitchModel(reference)) if reference == "fixture/model-11")
        );
        assert_eq!(
            app.current_model.as_ref().unwrap().reference,
            "fixture/model-2"
        );
        assert!(app.menu.is_none());
        app.input.insert_str("后续草稿");
        events
            .send(WorkerEvent::ModelSwitched(Ok((
                app.choices[11].clone(),
                app.think_level,
            ))))
            .unwrap();
        app.receive();
        assert_eq!(
            app.current_model.as_ref().unwrap().reference,
            "fixture/model-11"
        );
        assert_eq!(app.input.lines(), &["后续草稿"]);
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
        assert_eq!(
            app.input.lines(),
            &["/model gateway/team/model with spaces"]
        );
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.menu.is_none());
        app.refresh_candidates(false);
        assert!(app.menu.is_none());
        app.handle(key(KeyCode::Backspace), &mut dirty);
        assert!(app.menu.is_some());
        app.handle(Event::Paste("\nnext".into()), &mut dirty);
        assert!(app.menu.is_none());
        app.input = editor();
        app.input.insert_str("/model missing");
        app.refresh_candidates(true);
        assert!(app.menu.is_none());
        app.input = editor();
        app.input.insert_str("/model ");
        for state in [State::Running, State::SwitchingModel, State::Unavailable] {
            app.state = state;
            app.refresh_candidates(true);
            assert!(app.menu.is_none());
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
        assert_eq!(app.menu.as_ref().unwrap().selected_index(), Some(1));
        app.handle(key(KeyCode::Char('m')), &mut dirty);
        assert_eq!(
            menu_values(&app),
            vec![MenuAction::Model(0), MenuAction::Model(1)]
        );
        assert_eq!(app.menu.as_ref().unwrap().selected_index(), Some(0));
    }

    #[test]
    fn command_menu_renders_below_input_with_columns_and_yellow_selection() {
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
        // 布局随命令数量变化，按内容定位输入行与其下方的菜单。
        let input_row = (0..30)
            .find(|&y| buffer[(0, y)].symbol() == "❯" && buffer[(2, y)].symbol() == "/")
            .expect("input row renders the prompt and draft");
        assert!(!buffer[(3, input_row)].modifier.contains(Modifier::REVERSED));
        assert_eq!(terminal.backend().cursor_position(), (3, input_row).into());
        let separator_row = input_row + 1;
        for x in 0..100 {
            assert_eq!(buffer[(x, separator_row)].symbol(), "─");
            assert_eq!(buffer[(x, separator_row)].fg, Color::Rgb(190, 133, 222));
        }
        let menu_top = separator_row + 1;
        for (index, spec) in COMMANDS.iter().take(8).enumerate() {
            let y = menu_top + index as u16;
            let active = index == 1;
            assert_eq!(buffer[(0, y)].symbol(), if active { "❯" } else { " " });
            assert_eq!(buffer[(2, y)].symbol(), spec.icon);
            assert_eq!(buffer[(4, y)].symbol(), &spec.name[1..2]);
            assert_eq!(
                buffer[(4, y)].fg,
                if active { Color::Yellow } else { Color::Reset }
            );
            assert_eq!(
                buffer[(47, y)].fg,
                if active {
                    Color::Yellow
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
        app.handle(control('p'), &mut dirty);
        assert_eq!(app.input.lines(), &["/model "]);
        app.handle(control('p'), &mut dirty);
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &["/model fixture/first"]);
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert_eq!(app.input.lines(), &["旧任务"]);
        assert_eq!(app.history_index, Some(0));
        app.navigate_history(false);
        assert_eq!(app.input.lines(), &["草稿", "🙂结尾"]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));

        app.handle(control('p'), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert_eq!(app.input.lines(), &["草稿", "🙂结尾"]);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        for succeeds in [false, true] {
            app.handle(control('p'), &mut dirty);
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
            app.handle(control('p'), &mut dirty);
            assert_eq!(app.input.lines(), &["草稿🙂"]);
            assert!(app.model_picker_draft.is_none());
        }
        app.state = State::Ready;
        app.skills_pending = true;
        app.handle(key(KeyCode::BackTab), &mut dirty);
        app.handle(control('p'), &mut dirty);
        app.skills_pending = false;
        for mut key in [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
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
        assert!(screen.contains("Shift-Tab"));
        assert!(screen.contains("Ctrl-P"));
        assert_eq!(app.input.lines(), &["未发送🙂"]);
        assert_eq!(app.input.cursor(), cursor);
        app.state = State::Ready;
        app.handle(control('p'), &mut dirty);
        app.handle(Event::Paste("missing".into()), &mut dirty);
        assert!(app.menu.is_none());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert_eq!(app.input.lines(), &["未发送🙂"]);
        app.handle(control('p'), &mut dirty);
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
        let mut repeat = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        repeat.kind = KeyEventKind::Repeat;
        app.handle(Event::Key(repeat), &mut dirty);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        app.handle(control('s'), &mut dirty);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::SaveDefaultThinkLevel(ThinkLevel::Xhigh))
        ));
        assert_eq!(app.think_level, ThinkLevel::High);
        app.handle(control('s'), &mut dirty);
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
            .write_scrollback(&mut scrollback, &snapshot)
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
        app.handle(control('s'), &mut dirty);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
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
        assert!(compact.find("❯/effort").unwrap() < compact.find("Disablethinking").unwrap());
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
        app.submit();
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
        // 忙碌与配置不可用时不发送、不回显、不清草稿。
        for state in [State::Running, State::Unavailable] {
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
            .write_scrollback(&mut scrollback, &buffer)
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
            last_request: None,
            auto_compact: false,
            threshold_percent: 80,
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
        assert_eq!(app.token_speed.value(Instant::now()), Some((150.0, false)));
        app.fail_worker();
        assert_eq!(app.token_speed.value(Instant::now()), Some((150.0, false)));
        app.events = Some(mpsc::channel().1);
        app.token_speed.start(Instant::now());
        app.token_speed.progress(600);
        app.fail_worker();
        assert!(!app.token_speed.is_active());
        assert_eq!(app.token_speed.value(Instant::now()), None);
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
                app.token_speed.value(Instant::now()),
                if success { Some((100.0, true)) } else { None }
            );
        }
    }

    #[test]
    fn input_uses_horizontal_rules_and_reserves_secondary_status_on_the_right() {
        let mut app = app();
        app.state = State::Ready;
        app.input.insert_str("draft-中\nsecond line");
        for state in [
            State::Ready,
            State::Running,
            State::Resetting,
            State::SwitchingModel,
            State::SettingThinkLevel,
            State::Unavailable,
        ] {
            app.state = state;
            let screen = draw(&mut app, 100, 30);
            assert_eq!(screen.lines().nth(25).unwrap(), "─".repeat(100));
        }
        let screen = draw(&mut app, 100, 30);
        let lines: Vec<_> = screen.lines().collect();
        assert!(lines[26].starts_with("❯ draft-中"));
        assert!(lines[27].starts_with("  second line"));
        assert_eq!(lines[28], "─".repeat(100));
        assert!(lines[29].starts_with(" pp · \u{ec19} — · \u{f0766} off · \u{f07c} fixture-cwd"));

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
        assert!(!app.submit());
        assert!(!app.skills_pending);
        assert_eq!(app.input.lines(), &["/skills"]);
    }
}
