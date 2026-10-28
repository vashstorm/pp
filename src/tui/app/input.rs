use crate::tui::{
    App, COMMANDS, CancellationToken, EFFORT_HINT, Level, MenuAction, MenuItem, PendingKind,
    SelectionMenu, State, ThinkLevel, WorkerCommand, editor, files, platform_text,
    transcript_style,
};

impl App {
    pub(in crate::tui) fn refresh_candidates(&mut self, edited: bool) {
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
            || self.plan_leave_confirmation.is_some()
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

    pub(in crate::tui) fn refresh_file_menu(&mut self, changed: bool) {
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

    pub(in crate::tui) fn complete_file_directory(&mut self) {
        files::insert(
            &mut self.input,
            self.file_query.as_ref().expect("checked current query"),
            &self.file_directory,
        );
        self.file_query = files::query(&self.input);
        self.refresh_file_menu(true);
    }

    pub(in crate::tui) fn at_input_prefix(&self) -> bool {
        let ratatui_textarea::DataCursor(row, column) = self.input.cursor();
        row == 0
            && !self.input.is_selecting()
            && self.input.lines()[0]
                .chars()
                .take(column)
                .all(char::is_whitespace)
    }

    pub(in crate::tui) fn insert_input(&mut self, mut text: String) {
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

    pub(in crate::tui) fn input_text(&self) -> String {
        let text = self.input.lines().join("\n");
        if self.shell_mode {
            format!("!{text}")
        } else {
            text
        }
    }

    pub(in crate::tui) fn clear_input(&mut self) {
        self.input = editor();
        self.shell_mode = false;
        self.history_index = None;
        self.history_draft = None;
        self.refresh_candidates(true);
    }

    pub(in crate::tui) fn remember_input(&mut self) {
        let text = self.input_text();
        if !text.trim().is_empty() {
            self.input_history.push(text);
        }
        self.history_index = None;
        self.history_draft = None;
    }

    pub(in crate::tui) fn navigate_history(&mut self, previous: bool) {
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

    pub(in crate::tui) fn send(&mut self, command: WorkerCommand, state: State) -> bool {
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

    pub(in crate::tui) fn close_pending_menu(&mut self) {
        self.queue_menu = false;
        self.queue_menu_revision = None;
        self.menu = None;
        self.menu_dismissed = true;
    }

    pub(in crate::tui) fn refresh_pending_menu(&mut self) {
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

    pub(in crate::tui) fn toggle_pending_menu(&mut self) {
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

    pub(in crate::tui) fn recall_pending(&mut self, id: u64) {
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

    pub(in crate::tui) fn resume_queue(&mut self) {
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

    pub(in crate::tui) fn refresh_queue(&mut self) -> bool {
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

    pub(in crate::tui) fn has_pending(&mut self) -> bool {
        self.refresh_queue();
        self.queue_snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.items.is_empty())
    }

    /// 派发下一条待处理消息；返回是否真的派发（状态离开 Ready）。
    /// Plan/Ready 自动派发直接拒绝；显式提交/恢复队列操作仍可派发（后端先降 Draft）。
    pub(in crate::tui) fn dispatch_next(&mut self, explicit: bool) -> bool {
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
            || self.plan_leave_confirmation.is_some()
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

    pub(in crate::tui) fn send_task(&mut self, prompt: Option<String>) -> bool {
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

    pub(in crate::tui) fn send_submission(&mut self, command: WorkerCommand, state: State) -> bool {
        if self.send(command, state) {
            self.remember_input();
            true
        } else {
            false
        }
    }

    pub(in crate::tui) fn send_shell(&mut self, command: String) {
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
    pub(in crate::tui) fn send_compaction(&mut self) {
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
}
