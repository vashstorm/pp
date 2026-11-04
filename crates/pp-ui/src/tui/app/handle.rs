use crate::tui::{
    App, CancellationToken, CursorMove, Event, FUNCTION_MODIFIER, Instant, KeyCode, KeyEventKind,
    KeyModifiers, Level, MOUSE_SCROLL_LINES, MenuAction, MenuEvent, MouseEventKind, PendingKind,
    State, WorkerCommand, editor, plan, sessions, skills, tree,
};

impl App {
    pub(in crate::tui) fn handle(&mut self, event: Event, dirty: &mut bool) -> bool {
        match &event {
            Event::Mouse(mouse) if self.resource_input_available() && self.menu.is_none() => {
                let (handled, changed) = self.image_view.mouse(*mouse, Instant::now());
                *dirty |= changed;
                if handled {
                    return false;
                }
            }
            Event::Key(_) | Event::Paste(_) | Event::Resize(_, _) => {
                *dirty |= self.image_view.close();
            }
            _ => {}
        }
        let load_references = matches!(&event, Event::Key(key) if matches!(key.code, KeyCode::Enter | KeyCode::Tab) || matches!(key.code, KeyCode::Char(c) if c.is_whitespace()))
            || matches!(&event, Event::Paste(_));
        let clipboard_key = matches!(&event, Event::Key(key) if key.code == KeyCode::Char('v') && key.modifiers == KeyModifiers::CONTROL);
        let input_focused = self.completion_input_focused();
        // 普通文本输入不维护图片编辑记录，也不复制整个草稿。
        if self.resources.images.is_empty() && self.resources.job.is_none() && !clipboard_key {
            let before = load_references.then(|| self.input_text());
            let quit = self.handle_inner(event, dirty);
            if before.is_some_and(|before| before != self.input_text()) {
                self.load_image_references(false, PendingKind::Steering);
            }
            if input_focused != self.completion_input_focused() {
                self.refresh_candidates(false);
            }
            return quit;
        }
        let before = self.input_text();
        let previous = self.resources.bindings.clone();
        let undo = matches!(&event, Event::Key(key) if key.code == KeyCode::Char('u') && key.modifiers == KeyModifiers::CONTROL);
        let redo = FUNCTION_MODIFIER != KeyModifiers::CONTROL
            && matches!(&event, Event::Key(key) if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL);
        if matches!(&event, Event::Key(key) if key.code == KeyCode::Esc)
            && self.resources.job.is_some()
        {
            self.resources.cancel();
            self.notify(Level::Normal, "已取消输入资源加载，草稿保留。".into());
            *dirty = true;
            return false;
        }
        if matches!(&event, Event::Key(key) if key.code == KeyCode::Char('v') && key.modifiers == KeyModifiers::CONTROL && key.kind != KeyEventKind::Release)
            && self.resource_input_available()
        {
            self.start_resource_job(
                crate::tui::resources::Operation::Clipboard,
                PendingKind::Steering,
            );
            *dirty = true;
            return false;
        }
        if self.resource_input_available() {
            let cursor = crate::tui::attachment_edit::offset(&self.input, self.input.cursor());
            crate::tui::attachment_edit::normalize(
                &mut self.input,
                &self.resources.bindings,
                cursor,
            );
            if (undo || redo)
                && matches!(&event, Event::Key(key) if key.kind != KeyEventKind::Release)
            {
                self.resources.undo_input(&mut self.input, redo);
                self.resources.cancel();
                self.refresh_candidates(true);
                *dirty = true;
                return false;
            }
        }
        let replacement = matches!(&event, Event::Paste(_));
        let epoch = self.resources.epoch;
        let selection = self.input.selection_range().map(|((a, b), (c, d))| {
            crate::tui::attachment_edit::offset(&self.input, ratatui_textarea::DataCursor(a, b))
                ..crate::tui::attachment_edit::offset(
                    &self.input,
                    ratatui_textarea::DataCursor(c, d),
                )
        });
        let old_cursor = crate::tui::attachment_edit::offset(&self.input, self.input.cursor());
        let quit = self.handle_inner(event, dirty);
        if epoch == self.resources.epoch {
            let after = self.input_text();
            let range = (before != after
                || (replacement && selection.as_ref().is_some_and(|range| !range.is_empty())))
            .then(|| {
                crate::tui::attachment_edit::edited_range(
                    &before,
                    &after,
                    old_cursor,
                    crate::tui::attachment_edit::offset(&self.input, self.input.cursor()),
                    selection,
                )
            });
            self.resources
                .edited(before.clone(), after, previous, range);
            crate::tui::attachment_edit::normalize(
                &mut self.input,
                &self.resources.bindings,
                old_cursor,
            );
        }
        if self
            .resources
            .job
            .as_ref()
            .is_some_and(|job| job.source != self.input_text() || job.cursor != self.input.cursor())
            || !self.resource_input_available()
        {
            self.resources.cancel();
        }
        if load_references && before != self.input_text() {
            self.load_image_references(false, PendingKind::Steering);
        }
        if input_focused != self.completion_input_focused() {
            self.refresh_candidates(false);
        }
        quit
    }

    pub(in crate::tui) fn handle_inner(&mut self, event: Event, dirty: &mut bool) -> bool {
        if !matches!(&event, Event::Key(key) if key.code == KeyCode::Esc
            && key.modifiers.is_empty() && key.kind == KeyEventKind::Press)
        {
            self.escape_tap.reset();
        }
        if let Some(panel) = &mut self.mcp_panel {
            if matches!(&event, Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) && key.kind == KeyEventKind::Press)
            {
                return true;
            }
            if panel.handle(&event) {
                self.mcp_panel = None;
                self.menu_dismissed = true;
            }
            *dirty = true;
            return false;
        }
        if self.ask_panel.is_none() && self.subagent_panel.is_some() {
            if let Event::Key(key) = &event
                && key.code == KeyCode::Char('c')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                return true;
            }
            if crate::tui::app::hub::is_hub_toggle(&event) {
                self.close_hub();
                *dirty = true;
                return false;
            }
            let (close, commands) = self.hub_handle(event);
            self.apply_hub_commands(commands);
            if close {
                self.close_hub();
            }
            *dirty = true;
            return false;
        }
        if let Some(panel) = &mut self.auth_panel {
            if matches!(&event, Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                if let Some(token) = &self.auth_cancellation {
                    token.cancel();
                }
                return matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press);
            }
            let action = panel.handle(event);
            self.apply_auth_action(action);
            *dirty = true;
            return false;
        }
        if let Some(panel) = &mut self.roles_panel {
            if matches!(&event, Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press);
            }
            let action = panel.handle(event);
            self.apply_roles_action(action);
            *dirty = true;
            return false;
        }
        // 放弃确认独占输入；粘贴、鼠标和全局快捷键不能绕过确认。
        if let Some(panel) = &mut self.plan_leave_confirmation {
            if let Event::Key(key) = event {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return key.kind == KeyEventKind::Press;
                }
                let action = panel.key(key);
                self.apply_plan_leave_action(action);
            }
            *dirty = true;
            return false;
        }
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                let control = key.modifiers.contains(KeyModifiers::CONTROL);
                if control && key.code == KeyCode::Char('c') {
                    return key.kind == KeyEventKind::Press;
                }
                // F8 是全局入口：即使面板已打开也重绑到最近的 run。
                if key.code == KeyCode::F(8)
                    && key.modifiers.is_empty()
                    && self.pipes_entry_allowed()
                {
                    if key.kind == KeyEventKind::Press {
                        self.open_pipes(None);
                    }
                    *dirty = true;
                    return false;
                }
                // 面板独占按键：Esc 只关闭，Ctrl+O 等全局键不得穿透。
                if let Some(panel) = &mut self.pipes_panel {
                    let action = panel.key(key);
                    self.escape_tap.reset();
                    self.apply_pipes_action(action);
                    *dirty = true;
                    return false;
                }
                // Alt+A 只在主视图切换 Hub；未获得绑定时也允许打开空态。
                if crate::tui::app::hub::is_hub_toggle(&Event::Key(key))
                    && self.hub_toggle_allowed()
                {
                    self.open_hub(false);
                    *dirty = true;
                    return false;
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
                // Esc 收起或历史召回后，显式 Tab 可重新唤起补全。
                if key.code == KeyCode::Tab
                    && key.modifiers.is_empty()
                    && self.menu.is_none()
                    && self.menu_dismissed
                    && self.completion_input_focused()
                {
                    self.refresh_candidates(true);
                    if self.model_panel.is_some() {
                        return false;
                    }
                }
                // 文件菜单独占左右方向键，普通聊天与计划讨论共用同一导航行为。
                if self.completion_input_focused()
                    && self.file_query.is_some()
                    && (self.menu.is_some() || self.files_pending)
                    && key.modifiers.is_empty()
                    && matches!(key.code, KeyCode::Left | KeyCode::Right)
                {
                    if !self.files_pending && key.kind == KeyEventKind::Press {
                        let action = if key.code == KeyCode::Left {
                            (!self.file_directory.is_empty()).then_some(MenuAction::FileParent)
                        } else {
                            self.menu.as_ref().and_then(|menu| {
                                menu.selected_index().map(|index| menu.items()[index].value)
                            })
                        };
                        if let Some(action) = action {
                            self.apply_menu_action(action, false);
                        }
                    }
                    return false;
                }
                if self
                    .plan_panel
                    .as_ref()
                    .is_some_and(|panel| panel.input_focused())
                    && self.file_query.is_some()
                {
                    if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                        self.file_query = None;
                        self.menu = None;
                        self.menu_dismissed = true;
                        return false;
                    }
                    if self.files_pending && matches!(key.code, KeyCode::Enter | KeyCode::Tab) {
                        return false;
                    }
                    let action = self
                        .menu
                        .as_mut()
                        .map_or(MenuEvent::Ignored, |menu| menu.handle_key(key));
                    match action {
                        MenuEvent::Confirm(action) => {
                            self.apply_menu_action(action, true);
                            return false;
                        }
                        MenuEvent::Complete(action) => {
                            self.apply_menu_action(action, false);
                            return false;
                        }
                        MenuEvent::Handled => return false,
                        MenuEvent::Cancel => {
                            self.file_query = None;
                            self.menu = None;
                            self.menu_dismissed = true;
                            return false;
                        }
                        MenuEvent::Ignored => {}
                    }
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
                                self.plan_progress = pp_core::plan::task_progress(&plan.content);
                            }
                        }
                        plan::Action::Approve(context) => self.approve_plan(context),
                        plan::Action::Review => self.review_plan(),
                        plan::Action::SaveAndLeave => self.save_plan_and_leave(),
                        plan::Action::EditInput(key) => {
                            if key.code == KeyCode::Char('j')
                                && key.modifiers == KeyModifiers::CONTROL
                            {
                                self.input.insert_newline();
                            } else {
                                self.edit_input(key);
                            }
                            self.refresh_candidates(true);
                        }
                        plan::Action::DiscussInput => {
                            let text = self.input.lines().join("\n");
                            if !text.trim().is_empty() && matches!(self.state, State::Ready) {
                                let source = text.clone();
                                let text = panel.compose_discussion(text);
                                if self.resources.job.is_some() {
                                    return false;
                                }
                                if self.prepare_resource_input(
                                    &source,
                                    &text,
                                    PendingKind::Steering,
                                ) {
                                    self.resources.plan_discussion = true;
                                    return false;
                                }
                                self.queue_paused = false;
                                if self.send_task(Some(text.clone())) {
                                    self.plan_panel = None;
                                    self.plan_list_panel = None;
                                    self.plan_list_pending = None;
                                    self.input = editor();
                                    self.append("You", &text);
                                    self.remember_text(text);
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
                if self.session_panel.is_some() {
                    let action = self
                        .session_panel
                        .as_mut()
                        .expect("active session panel")
                        .key(key);
                    // 查询/范围变化立即发出异步 Search；结果只按最新请求安装。
                    self.search_session_panel();
                    match action {
                        sessions::Action::None => {}
                        sessions::Action::Cancel => self.close_session_panel(),
                        sessions::Action::Pin(index) => self.pin_session(index),
                        sessions::Action::Resume(index) => {
                            let id = self
                                .session_panel
                                .as_ref()
                                .and_then(|panel| panel.id(index))
                                .expect("selection indexes the session list")
                                .to_owned();
                            if self.session_id.as_deref() == Some(id.as_str()) {
                                // 当前会话：只关闭列表，不重锁自己的 journal。
                                self.close_session_panel();
                            } else {
                                self.session_panel
                                    .as_mut()
                                    .expect("active session panel")
                                    .restore_started();
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
                            let id = self
                                .session_panel
                                .as_ref()
                                .and_then(|panel| panel.id(index))
                                .expect("selection indexes the session list")
                                .to_owned();
                            // 删除仍由 Agent worker 取得会话锁并删除 JSONL；
                            // 面板存在即阻止队列派发与编辑器输入，故保持 Ready。
                            if !self.send(WorkerCommand::DeleteSession(id), State::Ready) {
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
                if key.modifiers == FUNCTION_MODIFIER && self.menu.is_none() {
                    match key.code {
                        KeyCode::Char('t') => {
                            if key.kind == KeyEventKind::Press {
                                self.todo_panel.toggle();
                            }
                            return false;
                        }
                        KeyCode::PageUp | KeyCode::PageDown if self.todo_panel.expanded => {
                            self.todo_panel.scroll(
                                key.code == KeyCode::PageUp,
                                usize::from(self.todo_panel.area.height.saturating_sub(1)).max(1),
                            );
                            return false;
                        }
                        _ => {}
                    }
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
                                | State::RunningTools
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
                            State::Running
                                | State::RunningTools
                                | State::ManagingMcp
                                | State::Compacting
                                | State::ReviewingPlan
                        )
                        && let Some(cancellation) = &self.active_cancellation
                    {
                        cancellation.cancel();
                        self.queue_paused = true;
                        match self.state {
                            State::ManagingMcp => {
                                self.secondary_status = Some("Cancelling MCP management…".into());
                            }
                            State::Compacting => {
                                self.secondary_status = Some("Cancelling context...".into());
                            }
                            State::RunningTools => {
                                self.secondary_status = Some("Cancelling local test...".into());
                            }
                            State::ReviewingPlan => {
                                // 取消 reviewer；保持 ReviewingPlan，等待 worker
                                // 的 PlanReviewed 受控收尾后回五项菜单。
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
                        self.edit_input(key);
                        // 先在显示行之间移动，越过首末显示行时才浏览历史。
                        if self.input.cursor() == cursor && self.model_picker_draft.is_none() {
                            self.navigate_history(key.code == KeyCode::Up);
                        } else {
                            self.refresh_candidates(false);
                        }
                    }
                    _ => {
                        let edited = self.edit_input(key);
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
            Event::Paste(_) if self.pipes_panel.is_some() => {
                *dirty = true;
            }
            Event::Mouse(mouse) if self.pipes_panel.is_some() => {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    self.pipes_panel
                        .as_mut()
                        .expect("active pipes panel")
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
                if !self.plan_panel.as_mut().expect("当前计划面板").paste(&text) {
                    if pp_runtime::user_input::possible_image_paste(&text)
                        && self.resource_input_available()
                    {
                        self.paste_resource_input(text);
                    } else {
                        self.input
                            .insert_str(text.replace("\r\n", "\n").replace('\r', "\n"));
                        self.refresh_candidates(true);
                    }
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
            Event::Mouse(mouse)
                if self.ask_panel.is_some()
                    && !self
                        .todo_panel
                        .area
                        .contains(ratatui::layout::Position::new(mouse.column, mouse.row)) =>
            {
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
                if self.session_panel.is_some() {
                    self.search_session_panel();
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
                if self.resource_input_available()
                    && pp_runtime::user_input::possible_image_paste(&text)
                {
                    // 路径识别也访问文件系统，统一放入后台线程。
                    self.paste_resource_input(text);
                    *dirty = true;
                    return false;
                }
                if self.queue_menu {
                    self.close_pending_menu();
                }
                self.insert_input(text.replace("\r\n", "\n").replace('\r', "\n"));
                self.refresh_candidates(true);
                *dirty = true;
            }
            Event::Mouse(mouse)
                if self
                    .todo_panel
                    .area
                    .contains(ratatui::layout::Position::new(mouse.column, mouse.row)) =>
            {
                if matches!(
                    mouse.kind,
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                ) {
                    self.todo_panel
                        .scroll(mouse.kind == MouseEventKind::ScrollUp, MOUSE_SCROLL_LINES);
                    *dirty = true;
                }
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
}
