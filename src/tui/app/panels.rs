use crate::tui::{
    App, COMMANDS, Command, Level, MenuAction, ModelPickerDraft, PendingKind, PlanCommand, Result,
    SHORTCUTS_TAIL, STEERING_HINT, State, ThinkLevel, WARNING, WorkerCommand, ask, commands,
    editor, files, model, sessions, skills,
};

impl App {
    pub(in crate::tui) fn show_help(&mut self) {
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

    pub(in crate::tui) fn shortcut_ready(&mut self) -> bool {
        if self.skills_pending
            || self.changes_panel.is_some()
            || self.plan_list_panel.is_some()
            || self.roles_panel.is_some()
        {
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

    pub(in crate::tui) fn open_model_picker(&mut self) {
        self.open_model_panel("", true);
    }

    pub(in crate::tui) fn open_model_panel(&mut self, query: &str, preserve_draft: bool) {
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
        if self.bootstrap.is_some() {
            self.reload_model_catalog();
        }
    }

    pub(in crate::tui) fn close_model_panel(&mut self) {
        if self.model_panel.take().is_none() {
            return;
        }
        self.menu = None;
        self.menu_dismissed = true;
        self.restore_model_picker_draft();
    }

    pub(in crate::tui) fn apply_model_action(&mut self, action: model::Action) {
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

    pub(in crate::tui) fn restore_model_picker_draft(&mut self) {
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

    pub(in crate::tui) fn switch_model(&mut self, reference: String) {
        if self.send_submission(WorkerCommand::SwitchModel(reference), State::SwitchingModel) {
            self.clear_input();
        }
    }

    pub(in crate::tui) fn set_think_level(&mut self, level: ThinkLevel) {
        if self.send_submission(
            WorkerCommand::SetThinkLevel(level),
            State::SettingThinkLevel,
        ) {
            self.clear_input();
        }
    }

    // /init 与 /task-test 回显用户命令本身；其余展开命令回显展开正文。
    pub(in crate::tui) fn echoes_source(prompt: &str) -> bool {
        matches!(prompt.trim(), "/init" | "/task-test")
    }

    /// `/task-test` 的本地前置校验：通过时返回固定提示词，否则返回本地错误文案。
    fn task_test_prompt(&self) -> std::result::Result<String, String> {
        if self.workflow.mode != crate::plan::WorkMode::Execute {
            return Err("Exit Plan mode before using /task-test.".into());
        }
        match self.state {
            State::Unavailable => {
                return Err(self
                    .unavailable
                    .clone()
                    .unwrap_or_else(|| "Configuration unavailable.".into()));
            }
            State::Ready | State::Running => {}
            _ => {
                return Err("Finish or cancel the current task before using /task-test.".into());
            }
        }
        commands::task_test::ready(&self.model_roles).map_err(|error| error.to_string())?;
        Ok(commands::task_test::PROMPT.to_owned())
    }

    pub(in crate::tui) fn submit(&mut self, kind: PendingKind) -> bool {
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
        let mut command = if self.shell_mode {
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
        if matches!(command, Some(Command::TaskTest)) {
            match self.task_test_prompt() {
                Ok(expanded) => command = Some(Command::Prompt(expanded)),
                Err(message) => {
                    self.notify(Level::Error, message);
                    return false;
                }
            }
        }
        if matches!(command, Some(Command::Agents | Command::Subagents)) {
            self.subagent_panel = Some(crate::tui::subagents::Panel {
                binding: self
                    .subagent_control
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                roles: matches!(command, Some(Command::Agents)),
                ..Default::default()
            });
            self.remember_input();
            self.clear_input();
            return false;
        }
        if matches!(command, Some(Command::Login | Command::Logout)) {
            if self.open_auth(matches!(command, Some(Command::Logout))) {
                self.remember_input();
                self.clear_input();
            }
            return false;
        }
        if matches!(command, Some(Command::Model(_))) && matches!(self.state, State::Unavailable) {
            self.reload_model_catalog();
            return false;
        }
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
        if let Some(Command::Mcp(action)) = &command {
            if !matches!(self.state, State::Ready) || self.has_pending() {
                self.notify(
                    Level::Error,
                    "Finish or cancel the current task before managing MCP".into(),
                );
            } else if matches!(action, crate::mcp::Action::Help) {
                self.remember_input();
                self.clear_input();
                self.mcp_panel = Some(crate::tui::mcp::Panel::help());
                self.menu = None;
            } else {
                let cancellation = crate::cancellation::CancellationToken::new();
                if self.send_submission(
                    WorkerCommand::Mcp {
                        action: action.clone(),
                        cancellation: cancellation.clone(),
                    },
                    State::ManagingMcp,
                ) {
                    self.active_cancellation = Some(cancellation);
                    self.clear_input();
                    self.secondary_status = Some("Managing MCP servers…".into());
                }
            }
            return false;
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
            State::ManagingMcp
            | State::Running
            | State::RunningShell
            | State::Cancelling
            | State::Compacting
            | State::Resetting
            | State::SwitchingModel
            | State::SavingModelSettings
            | State::SettingThinkLevel
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
                Some(Command::Roles) => self.open_roles(),
                Some(Command::Agents | Command::Subagents) => unreachable!("handled above"),
                Some(Command::Shell(_)) => unreachable!("handled before the state match"),
                Some(Command::TaskTest) => unreachable!("converted before the state match"),
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
                    self.open_session_panel();
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
                Some(
                    Command::Help
                    | Command::Tips
                    | Command::Quit
                    | Command::Skills(_)
                    | Command::Mcp(_)
                    | Command::Login
                    | Command::Logout,
                ) => {
                    unreachable!()
                }
                // Plan 子命令在状态检查前已处理。
                Some(Command::Plan(_)) => unreachable!("handled before the state match"),
                Some(Command::PlanList) => unreachable!("handled before the state match"),
            },
        }
        false
    }

    pub(in crate::tui) fn request_skills(&mut self, request: skills::Request) {
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

    pub(in crate::tui) fn close_session_panel(&mut self) {
        self.session_panel = None;
        self.menu_dismissed = true;
        // 检索仍可能在 worker 中运行：迟到的回复按代次丢弃，绝不会重开面板。
    }

    /// 打开 /resume：分配新代次并向 history worker 发送首次同步检索。
    pub(in crate::tui) fn open_session_panel(&mut self) {
        let generation = self.next_session_generation();
        let mut panel = sessions::Panel::new(
            generation,
            self.session_id.clone(),
            std::env::current_dir().unwrap_or_default(),
        );
        let (request_id, cwd, query) = panel.sync_request();
        self.session_panel = Some(panel);
        if !self.history_sync_and_search(generation, request_id, cwd, query)
            && let Some(panel) = &mut self.session_panel
        {
            panel.fail(
                request_id,
                "Session search unavailable; the history database could not be opened.".into(),
            );
        }
    }

    /// 查询/范围变化后发送一次异步 Search；失败时保留编辑但清空旧行。
    pub(in crate::tui) fn search_session_panel(&mut self) {
        let request = self.session_panel.as_mut().and_then(|panel| {
            let generation = panel.generation();
            panel
                .take_search()
                .map(|(request_id, cwd, query)| (generation, request_id, cwd, query))
        });
        let Some((generation, request_id, cwd, query)) = request else {
            return;
        };
        if !self.history_search(generation, request_id, cwd, query)
            && let Some(panel) = &mut self.session_panel
        {
            panel.fail(request_id, "Session search unavailable.".into());
        }
    }

    /// history worker 的会话检索回复：只安装当前面板代次与最新请求的结果。
    pub(in crate::tui) fn receive_session_search(
        &mut self,
        panel_generation: u64,
        request_id: u64,
        result: Result<crate::agent::persistence::SessionListing>,
    ) {
        let mut skipped = 0;
        let mut failure = None;
        {
            let Some(panel) = self.session_panel.as_mut() else {
                return;
            };
            if panel.generation() != panel_generation {
                return;
            }
            match result {
                Ok(listing) => {
                    let count = listing.diagnostics.len();
                    if panel.install(request_id, listing) {
                        skipped = count;
                    }
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if panel.fail(request_id, format!("Session search failed: {message}")) {
                        failure = Some(message);
                    }
                }
            }
        }
        if skipped > 0 {
            self.notify(
                Level::Error,
                format!("Skipped {skipped} unreadable saved session file(s)."),
            );
        }
        if let Some(message) = failure {
            self.notify(
                Level::Error,
                format!("Failed to search saved sessions: {message}"),
            );
        }
    }

    pub(in crate::tui) fn apply_menu_action(&mut self, action: MenuAction, confirm: bool) -> bool {
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
            MenuAction::McpSubcommand(index) => {
                let (name, _, argument) = crate::mcp::SUBCOMMANDS[index];
                self.input = editor();
                self.input
                    .insert_str(format!("/mcp {name}{}", if argument { " " } else { "" }));
                self.refresh_candidates(true);
                return confirm && !argument && self.submit(PendingKind::Steering);
            }
            MenuAction::McpServer(index) => {
                if let Some(entry) = self.mcp_view.entries.get(index) {
                    let action = self.input.lines()[0]
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("")
                        .to_owned();
                    let name = entry.name.clone();
                    self.input = editor();
                    self.input.insert_str(format!("/mcp {action} {name}"));
                    self.menu = None;
                    self.menu_dismissed = true;
                    return confirm && self.submit(PendingKind::Steering);
                }
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

    pub(in crate::tui) fn shutdown_background_tasks(&mut self) -> Result<()> {
        self.changes_panel = None;
        self.plan_list_panel = None;
        self.plan_list_pending = None;
        self.plan_reenter_standby = false;
        if let Some(token) = &self.active_cancellation {
            token.cancel();
        }
        self.close_ask_on_exit();
        let binding = self
            .subagent_control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(binding) = binding {
            binding.shutdown()?;
        }
        if let Some(cleanup) = &self.background_cleanup {
            cleanup.shutdown()?;
        }
        Ok(())
    }

    pub(in crate::tui) fn close_ask_on_exit(&mut self) {
        if self.ask_panel.is_some() || self.ask_reply.is_some() {
            let child = self
                .ask_id
                .as_ref()
                .is_some_and(|id| id.starts_with("subagent-"));
            if let Some(token) = self.ask_cancellation.take() {
                token.cancel();
            }
            self.ask_id = None;
            if !child && let Some(token) = &self.active_cancellation {
                token.cancel();
            }
            self.ask_reply = None;
            self.ask_panel = None;
        }
    }

    pub(in crate::tui) fn apply_ask_action(&mut self, action: ask::Action) {
        match action {
            ask::Action::None => {}
            ask::Action::Cancel => {
                let child = self
                    .ask_id
                    .as_ref()
                    .is_some_and(|id| id.starts_with("subagent-"));
                self.close_ask_on_exit();
                if !child {
                    self.state = State::Cancelling;
                    self.conversation.cancelling();
                }
            }
            ask::Action::Submit(answers) => {
                let child = self
                    .ask_id
                    .as_ref()
                    .is_some_and(|id| id.starts_with("subagent-"));
                self.ask_panel = None;
                self.ask_cancellation = None;
                self.ask_id = None;
                if let Some(reply) = self.ask_reply.take()
                    && reply.send(answers).is_err()
                    && !child
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
}
