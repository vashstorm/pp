use crate::tui::{
    App, CancellationToken, CursorMove, Event, FUNCTION_MODIFIER, Instant, KeyCode, KeyEventKind,
    KeyModifiers, Level, MOUSE_SCROLL_LINES, MenuAction, MenuEvent, MouseAction, MouseEventKind,
    PendingKind, State, WorkerCommand, editor, plan, sessions, skills, tree,
};

impl App {
    pub(in crate::tui) fn handle(&mut self, event: Event, dirty: &mut bool) -> bool {
        if !matches!(&event, Event::Key(key) if key.code == KeyCode::Esc
            && key.modifiers.is_empty() && key.kind == KeyEventKind::Press)
        {
            self.escape_tap.reset();
        }
        if self.ask_panel.is_none()
            && let Some(panel) = &mut self.subagent_panel
        {
            if let Event::Key(key) = event {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return true;
                }
                let close = panel.key(key);
                self.subagent_autowake |= panel.wake_parent;
                panel.wake_parent = false;
                if close {
                    self.subagent_panel = None;
                }
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
