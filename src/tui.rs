mod menu;
mod scrollback;
mod skills;
mod statusline;
mod transcript;
mod welcome;

use crate::{
    agent::Agent,
    commands::{self, COMMANDS, Command},
    config::{ModelChoice, ThinkLevel},
    events::AgentEvent,
};
use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
    },
    execute,
};
use menu::{MenuEvent, MenuItem, SelectionMenu};
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
    time::Duration,
};
use transcript::Transcript;

const WARNING: &str = "write/edit only modify files inside the startup directory; bash has no directory isolation, so run in a trusted or temporary directory.\nbash runs with an execution timeout that kills the process group; Ctrl-C exits pp immediately, with no guaranteed cleanup of child processes and no rollback of file changes.";
const SHORTCUTS: &str = "Enter send · ↑↓ move/history/menu · Shift-Tab cycle thinking · Ctrl-P choose model · F1 help · Tab complete · Esc dismiss · Ctrl-A/E line start/end · Ctrl-K delete to end of line · Ctrl-J newline · Ctrl-O expand/collapse bash/skill · Wheel/PgUp/PgDn scroll · Ctrl-C quit";
const MOUSE_SCROLL_LINES: usize = 3;

pub(crate) fn run(
    agent: Result<Agent>,
    cwd: &Path,
    skills: crate::skills::SharedSkills,
) -> Result<()> {
    // Construct before try_init so partially initialized terminals are restored too.
    let guard = TerminalGuard;
    let mut terminal = ratatui::try_init().context("initializing terminal")?;
    execute!(io::stdout(), EnableBracketedPaste, EnableMouseCapture)
        .context("enabling terminal input modes")?;
    let mut app = App::new_with_skills(agent, skills)?;
    let mut workspace = WorkspaceStatus::new(cwd)?;
    let mut dirty = true;
    loop {
        dirty |= app.receive();
        dirty |= workspace.receive();
        if dirty {
            terminal
                .draw(|frame| app.draw(frame, &workspace.context))
                .context("drawing terminal")?;
            dirty = false;
        }
        if event::poll(Duration::from_millis(50)).context("polling terminal input")? {
            let event = event::read().context("reading terminal input")?;
            if app.handle(event, &mut dirty) {
                let snapshot = terminal
                    .draw(|frame| app.draw(frame, &workspace.context))
                    .context("drawing final terminal screen")?
                    .buffer
                    .clone();
                drop(terminal);
                drop(guard);
                return app
                    .conversation
                    .write_scrollback(&mut io::stdout().lock(), &snapshot)
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
    Run(String),
    Reset,
    SwitchModel(String),
    SetThinkLevel(ThinkLevel),
}
enum WorkerEvent {
    Progress(AgentEvent),
    Completed(Result<()>),
    Reset,
    ModelSwitched(Result<ModelChoice>),
    ThinkLevelSet(ThinkLevel),
}

#[derive(Clone, Copy)]
enum State {
    Ready,
    Running,
    Resetting,
    SwitchingModel,
    SettingThinkLevel,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MenuAction {
    Command(usize),
    Model(usize),
    Effort(ThinkLevel),
}

struct ModelPickerDraft {
    input: TextArea<'static>,
    history_index: Option<usize>,
    history_draft: Option<TextArea<'static>>,
    menu: Option<SelectionMenu<MenuAction>>,
    menu_dismissed: bool,
}

struct App {
    conversation: Transcript,
    show_welcome: bool,
    input: TextArea<'static>,
    input_history: Vec<String>,
    history_index: Option<usize>,
    history_draft: Option<TextArea<'static>>,
    state: State,
    commands: Option<Sender<WorkerCommand>>,
    events: Option<Receiver<WorkerEvent>>,
    unavailable: Option<String>,
    busy_notice: bool,
    worker_failed: bool,
    choices: Vec<ModelChoice>,
    current_model: Option<ModelChoice>,
    think_level: ThinkLevel,
    menu: Option<SelectionMenu<MenuAction>>,
    menu_dismissed: bool,
    model_picker_draft: Option<ModelPickerDraft>,
    secondary_status: Option<String>,
    skill_worker: skills::Worker,
    skill_panel: Option<skills::Panel>,
    skills_pending: bool,
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
        Self::new_with_skills(agent, skills)
    }

    fn new_with_skills(agent: Result<Agent>, skills: crate::skills::SharedSkills) -> Result<Self> {
        let diagnostics = skills
            .lock()
            .map_err(|_| anyhow::anyhow!("skill registry unavailable"))?
            .diagnostics
            .clone();
        let mut app = Self {
            conversation: Transcript::new(),
            show_welcome: true,
            input: editor(),
            input_history: Vec::new(),
            history_index: None,
            history_draft: None,
            state: State::Unavailable,
            commands: None,
            events: None,
            unavailable: None,
            busy_notice: false,
            worker_failed: false,
            choices: Vec::new(),
            current_model: None,
            think_level: ThinkLevel::None,
            menu: None,
            menu_dismissed: false,
            model_picker_draft: None,
            secondary_status: None,
            skill_worker: skills::Worker::new(skills)?,
            skill_panel: None,
            skills_pending: false,
        };
        match agent {
            Ok(mut agent) => {
                app.choices = agent.model_choices();
                app.current_model = Some(agent.current_model().clone());
                app.think_level = agent.think_level();
                let (commands, receiver) = mpsc::channel();
                let (sender, events) = mpsc::sync_channel(256);
                // Deliberately detached: quitting must not wait for HTTP or foreground tools.
                thread::Builder::new()
                    .name("pp-agent".into())
                    .spawn(move || {
                        while let Ok(command) = receiver.recv() {
                            let event = match command {
                                WorkerCommand::Run(prompt) => {
                                    let mut emit = |event| {
                                        let _ = sender.send(WorkerEvent::Progress(event));
                                    };
                                    WorkerEvent::Completed(agent.run(&prompt, &mut emit))
                                }
                                WorkerCommand::Reset => {
                                    agent.reset();
                                    WorkerEvent::Reset
                                }
                                WorkerCommand::SetThinkLevel(level) => {
                                    agent.set_think_level(level);
                                    WorkerEvent::ThinkLevelSet(agent.think_level())
                                }
                                WorkerCommand::SwitchModel(reference) => {
                                    WorkerEvent::ModelSwitched(
                                        agent
                                            .switch_model(&reference)
                                            .map(|()| agent.current_model().clone()),
                                    )
                                }
                            };
                            if sender.send(event).is_err() {
                                break;
                            }
                        }
                    })
                    .context("starting agent worker")?;
                app.commands = Some(commands);
                app.events = Some(events);
                app.state = State::Ready;
            }
            Err(error) => {
                let error = format!("{error:#}");
                app.append("Error", &error);
                app.append(
                    "Note",
                    "Configuration unavailable; model tasks are disabled. Fix the config files and restart pp. /quit still exits cleanly.",
                );
                app.unavailable = Some(error);
            }
        }
        for diagnostic in diagnostics {
            app.conversation
                .append("Error", &format!("skill: {diagnostic}"));
        }
        Ok(app)
    }

    fn append(&mut self, label: &str, text: &str) {
        self.show_welcome = false;
        self.conversation.append(label, text);
    }

    fn fail_worker(&mut self) {
        let message = "Execution unavailable: agent worker channel closed; restart pp.";
        self.show_welcome = false;
        self.state = State::Unavailable;
        self.commands = None;
        self.events = None;
        self.busy_notice = false;
        self.unavailable = Some(message.into());
        self.worker_failed = true;
        self.conversation.fail(message);
    }

    fn receive(&mut self) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.skill_worker.replies.try_recv() {
            self.skills_pending = false;
            match reply.result {
                Ok(snapshot) if !reply.saved => {
                    self.skill_panel = Some(skills::Panel::new(snapshot))
                }
                Ok(_) => {
                    self.skill_panel = None;
                    self.menu = None;
                    self.menu_dismissed = true;
                }
                Err(error) => {
                    if let Some(panel) = &mut self.skill_panel {
                        panel.busy = false;
                        panel.error = Some(format!("{error:#}"));
                    } else {
                        self.append("Error", &format!("{error:#}"));
                    }
                }
            }
            changed = true;
        }
        for _ in 0..256 {
            let Some(events) = &self.events else {
                break;
            };
            match events.try_recv() {
                Ok(WorkerEvent::Progress(event)) => {
                    self.conversation.progress(event);
                    changed = true;
                }
                Ok(WorkerEvent::Completed(result)) => {
                    if let Err(error) = result {
                        self.conversation.fail(&format!("{error:#}"));
                    }
                    self.state = State::Ready;
                    self.busy_notice = false;
                    changed = true;
                }
                Ok(WorkerEvent::Reset) => {
                    self.conversation = Transcript::new();
                    self.clear_input();
                    self.append("Note", "Session reset.");
                    self.show_welcome = true;
                    self.state = State::Ready;
                    self.busy_notice = false;
                    changed = true;
                }
                Ok(WorkerEvent::ModelSwitched(result)) => {
                    match result {
                        Ok(choice) => {
                            self.append(
                                "Note",
                                &format!("Current model: {} ({})", choice.reference, choice.model),
                            );
                            self.current_model = Some(choice);
                        }
                        Err(error) => self.append("Error", &format!("{error:#}")),
                    }
                    self.state = State::Ready;
                    self.busy_notice = false;
                    changed = true;
                }
                Ok(WorkerEvent::ThinkLevelSet(level)) => {
                    self.think_level = level;
                    self.append("Note", &format!("Thinking level: {}", level.label()));
                    self.state = State::Ready;
                    self.busy_notice = false;
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
            || self.menu_dismissed
            || !matches!(self.state, State::Ready | State::Unavailable)
            || self.input.lines().len() != 1
        {
            self.menu = None;
            return;
        }
        // 保留末尾空格：输入命令后的第一个空格即展开参数菜单。
        let input = self.input.lines()[0].trim_start();
        let argument = input.split_once(char::is_whitespace);
        if self.model_picker_draft.is_some() && !matches!(argument, Some(("/model", _))) {
            self.menu = None;
            return;
        }
        let mut preferred = 0;
        let items = match argument {
            Some(("/effort", argument)) if matches!(self.state, State::Ready) => {
                let items: Vec<_> = ThinkLevel::ALL
                    .into_iter()
                    .filter(|level| level.as_str().starts_with(argument.trim()))
                    .map(|level| MenuItem {
                        value: MenuAction::Effort(level),
                        label: level.label().into(),
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
                        label: choice.reference.clone(),
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
                    .position(|item| item.label == prefix)
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
                .collect(),
            _ => Vec::new(),
        };
        if items.is_empty() {
            self.menu = None;
        } else if edited || self.menu.as_ref().is_none_or(|menu| menu.items() != items) {
            self.menu = Some(SelectionMenu::new(items, preferred));
        }
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
                return;
            }
            _ => return,
        };
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
            self.state = state;
            self.busy_notice = false;
            self.refresh_candidates(false);
            true
        } else {
            self.fail_worker();
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

    fn show_help(&mut self) {
        self.append("Note", &commands::help());
        self.append("Tips", SHORTCUTS);
        self.append("Note", WARNING);
    }

    fn shortcut_ready(&mut self) -> bool {
        if self.skills_pending {
            self.busy_notice = true;
            return false;
        }
        match self.state {
            State::Ready => true,
            State::Unavailable => {
                if let Some(error) = self.unavailable.clone() {
                    self.append("Error", &error);
                }
                false
            }
            _ => {
                self.busy_notice = true;
                false
            }
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
        let command = match commands::parse(&prompt) {
            Ok(command) => command,
            Err(error) => {
                self.append("Error", &format!("{error:#}"));
                return false;
            }
        };
        if self.model_picker_draft.is_some() && !matches!(command, Some(Command::Model(_))) {
            self.append("Error", "模型选择器仅接受 /model 命令；按 Esc 返回草稿。");
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
                self.busy_notice = true;
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
            | State::Resetting
            | State::SwitchingModel
            | State::SettingThinkLevel => self.busy_notice = true,
            State::Unavailable => {
                if let Some(error) = self.unavailable.take() {
                    self.append("Error", &error);
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
                Some(Command::Model(Some(reference))) => self.switch_model(reference.to_owned()),
                Some(Command::Model(None)) => {
                    self.input = editor();
                    self.input.insert_str("/model ");
                    self.refresh_candidates(true);
                }
                None => {
                    if self.send_submission(WorkerCommand::Run(prompt.clone()), State::Running) {
                        self.append("You", &prompt);
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
                self.menu = None;
                if let Some(panel) = &mut self.skill_panel {
                    panel.busy = true;
                }
            }
            Err(_) => self.append("Error", "skill 管理线程不可用"),
        }
    }

    fn apply_menu_action(&mut self, action: MenuAction, confirm: bool) -> bool {
        match action {
            MenuAction::Command(index) => {
                self.input = editor();
                self.input.insert_str(COMMANDS[index].name);
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
                    self.append(
                        "Note",
                        &format!("Current model: {} ({})", choice.reference, choice.model),
                    );
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

    fn handle(&mut self, event: Event, dirty: &mut bool) -> bool {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                if control && key.code == KeyCode::Char('c') {
                    return key.kind == KeyEventKind::Press;
                }
                *dirty = true;
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
                let shift_tab = (key.code == KeyCode::BackTab
                    && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT))
                    || (key.code == KeyCode::Tab && key.modifiers == KeyModifiers::SHIFT);
                if shift_tab {
                    if key.kind == KeyEventKind::Press
                        && self.menu.is_none()
                        && self.model_picker_draft.is_none()
                        && self.shortcut_ready()
                    {
                        self.send(
                            WorkerCommand::SetThinkLevel(self.think_level.next()),
                            State::SettingThinkLevel,
                        );
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
                            return false;
                        }
                        KeyCode::Char('e') => {
                            let end = self.input.lines()[row].chars().count();
                            if column < end {
                                self.input.move_cursor(CursorMove::End);
                            }
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
                        }
                    }
                    _ => {
                        let edited = self.input.input(key);
                        self.refresh_candidates(edited);
                    }
                }
            }
            Event::Paste(_) if self.skill_panel.is_some() => {}
            Event::Mouse(_) if self.skill_panel.is_some() => {}
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
            ) && row < usize::from(editor_area.height)
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
    }

    fn draw_input_status(&self, frame: &mut Frame, area: Rect) {
        let (state, color) = match self.state {
            State::Ready => ("", Color::DarkGray),
            State::Running => ("running", Color::Yellow),
            State::Resetting => ("resetting", Color::Yellow),
            State::SwitchingModel => ("switching model", Color::Yellow),
            State::SettingThinkLevel => ("setting thinking level", Color::Yellow),
            State::Unavailable if self.worker_failed => ("execution unavailable", Color::Red),
            State::Unavailable => ("config unavailable", Color::Red),
        };
        let notice = if self.busy_notice {
            ", cannot send now"
        } else {
            ""
        };
        // 上沿右半区为第二状态栏预留；未设置内容时保持横线。
        let left_width = area.width / 2;
        if !state.is_empty() {
            let state = Line::styled(format!(" {state}{notice} "), Style::default().fg(color));
            let state_width = u16::try_from(state.width())
                .unwrap_or(u16::MAX)
                .min(left_width);
            frame.render_widget(state, Rect::new(area.x, area.y, state_width, 1));
        }
        if let Some(extra) = self
            .secondary_status
            .as_deref()
            .filter(|text| !text.is_empty())
        {
            let extra = Line::styled(format!(" {extra} "), Style::default().fg(Color::DarkGray))
                .alignment(Alignment::Right);
            let extra_width = u16::try_from(extra.width())
                .unwrap_or(u16::MAX)
                .min(area.width - left_width);
            frame.render_widget(
                extra,
                Rect::new(area.right() - extra_width, area.y, extra_width, 1),
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
        App::new(Err(anyhow::anyhow!("fixture configuration unavailable"))).unwrap()
    }

    fn draw(app: &mut App, width: u16, height: u16) -> String {
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
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn menu_values(app: &App) -> Vec<MenuAction> {
        app.menu.as_ref().map_or_else(Vec::new, |menu| {
            menu.items().iter().map(|item| item.value).collect()
        })
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
            assert!(matches!(requests.try_recv(), Ok(WorkerCommand::Run(text)) if text == prompt));
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
            matches!(requests.try_recv(), Ok(WorkerCommand::Run(text)) if text == "second\n🙂末行!")
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
        events.send(WorkerEvent::Reset).unwrap();
        app.receive();
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
            .send(WorkerEvent::ModelSwitched(Ok(app.choices[11].clone())))
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
            })
            .collect();
        app.current_model = Some(app.choices[1].clone());
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
        assert_eq!(buffer[(0, 21)].symbol(), "❯");
        assert_eq!(buffer[(2, 21)].symbol(), "/");
        assert!(!buffer[(3, 21)].modifier.contains(Modifier::REVERSED));
        assert_eq!(terminal.backend().cursor_position(), (3, 21).into());
        for x in 0..100 {
            assert_eq!(buffer[(x, 22)].symbol(), "─");
            assert_eq!(buffer[(x, 22)].fg, Color::Rgb(190, 133, 222));
        }
        for (index, spec) in COMMANDS.iter().enumerate() {
            let y = 23 + index as u16;
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
        for (index, level) in [
            ThinkLevel::Low,
            ThinkLevel::Medium,
            ThinkLevel::High,
            ThinkLevel::Xhigh,
            ThinkLevel::Max,
            ThinkLevel::None,
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
            events.send(WorkerEvent::ThinkLevelSet(level)).unwrap();
            app.receive();
            assert_eq!(app.think_level, level);
            assert!(draw(&mut app, 100, 30).contains(level.label()));
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
                Ok(app.choices[1].clone())
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
            .send(WorkerEvent::ThinkLevelSet(ThinkLevel::Max))
            .unwrap();
        app.receive();
        assert_eq!(app.think_level, ThinkLevel::Max);
        assert_eq!(app.input.lines(), &["新草稿"]);
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
    fn input_uses_horizontal_rules_and_reserves_secondary_status_on_the_right() {
        let mut app = app();
        app.state = State::Ready;
        app.input.insert_str("draft-中\nsecond line");
        let screen = draw(&mut app, 100, 30);
        let lines: Vec<_> = screen.lines().collect();
        assert_eq!(lines[25], "─".repeat(100));
        assert!(lines[26].starts_with("❯ draft-中"));
        assert!(lines[27].starts_with("  second line"));
        assert_eq!(lines[28], "─".repeat(100));
        assert!(lines[29].starts_with(" \u{ec19} — · \u{f0aa3} no think · \u{f07c} fixture-cwd"));

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
        assert!(sender.send(WorkerEvent::Completed(Ok(()))).is_ok());
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
                .send(WorkerEvent::Completed(Err(anyhow::anyhow!(
                    "stream failed"
                ))))
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
        assert!(app.busy_notice);
        assert!(!app.skills_pending);
        assert_eq!(app.input.lines(), &["/skills"]);
    }
}
