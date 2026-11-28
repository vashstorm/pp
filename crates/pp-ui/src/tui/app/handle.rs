mod escape;
mod overlay_io;
mod plan_keys;

use crate::tui::{
    App, CancellationToken, CursorMove, Event, FUNCTION_MODIFIER, Instant, KeyCode, KeyEventKind,
    KeyModifiers, Level, MenuAction, MenuEvent, PendingKind, State, WorkerCommand, sessions, tree,
};

impl App {
    pub(in crate::tui) fn handle(&mut self, event: Event, dirty: &mut bool) -> bool {
        if self.desktop_notifications.update_focus(&event) {
            return false;
        }
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
        let tab = matches!(&event, Event::Key(key) if key.code == KeyCode::Tab);
        let load_references = tab
            || matches!(&event, Event::Key(key) if key.code == KeyCode::Enter || matches!(key.code, KeyCode::Char(c) if c.is_whitespace()))
            || matches!(&event, Event::Paste(_));
        let clipboard_key = matches!(&event, Event::Key(key) if key.code == KeyCode::Char('v') && key.modifiers == KeyModifiers::CONTROL);
        let input_focused = self.completion_input_focused();
        // Plain text input keeps no image edit records and copies no whole draft.
        if self.resources.images.is_empty() && self.resources.job.is_none() && !clipboard_key {
            let before = load_references.then(|| self.input_text());
            let quit = self.handle_inner(event, dirty);
            if (!tab || self.file_query.is_none())
                && before.is_some_and(|before| before != self.input_text())
            {
                self.load_image_references(false, PendingKind::Steering);
            }
            if input_focused != self.completion_input_focused() {
                self.refresh_candidates(false);
            }
            return quit;
        }
        let before = self.input_text();
        let raw_before = self.raw_input_text();
        let previous = self.resources.bindings.clone();
        let undo = matches!(&event, Event::Key(key) if key.code == KeyCode::Char('u') && key.modifiers == KeyModifiers::CONTROL);
        let redo = FUNCTION_MODIFIER != KeyModifiers::CONTROL
            && matches!(&event, Event::Key(key) if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL);
        if matches!(&event, Event::Key(key) if key.code == KeyCode::Esc)
            && self.resources.job.is_some()
        {
            self.resources.cancel();
            self.notify(
                Level::Normal,
                "Input resource loading cancelled; draft kept.".into(),
            );
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
            let raw_after = self.raw_input_text();
            let range = (raw_before != raw_after
                || (replacement && selection.as_ref().is_some_and(|range| !range.is_empty())))
            .then(|| {
                crate::tui::attachment_edit::edited_range(
                    &raw_before,
                    &raw_after,
                    old_cursor,
                    crate::tui::attachment_edit::offset(&self.input, self.input.cursor()),
                    selection,
                )
            });
            self.resources
                .edited(raw_before, raw_after, previous, range);
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
        if load_references && (!tab || self.file_query.is_none()) && before != self.input_text() {
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
        // Permission switching is independent of task state and panel focus: it applies at any
        // time and takes effect on the next tool call.
        if let Event::Key(key) = &event
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && super::permissions::is_permission_shortcut(*key)
        {
            if key.kind == KeyEventKind::Press {
                self.apply_permission_command(crate::commands::PermissionCommand::Cycle);
            }
            *dirty = true;
            return false;
        }
        // F7 opens/closes the `/permissions` command entry in any state, including confirmation
        // dialogs and full-screen modal panels.
        if let Event::Key(key) = &event
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && super::permissions::is_permission_command_shortcut(*key)
        {
            if key.kind == KeyEventKind::Press {
                self.toggle_permission_overlay();
            }
            *dirty = true;
            return false;
        }
        if self.permission_overlay.is_some() {
            return self.handle_permission_overlay_event(event, dirty);
        }
        let input_focused = self.completion_input_focused();
        if let Some(panel) = &mut self.mcp_panel
            && (input_focused || !panel.allows_input())
            && panel.captures(&event)
        {
            if matches!(&event, Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) && key.kind == KeyEventKind::Press)
            {
                return true;
            }
            if panel.handle(&event) {
                self.mcp_panel = None;
                self.menu_dismissed = true;
                self.refresh_candidates(false);
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
        if self.active_settings_tab().is_some() {
            return self.handle_settings_event(event, dirty);
        }
        if let Some(panel) = &mut self.context_panel {
            self.escape_tap.reset();
            if matches!(&event, Event::Key(key) if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return matches!(&event, Event::Key(key) if key.kind == KeyEventKind::Press);
            }
            let action = panel.handle(event);
            self.apply_context_action(action);
            *dirty = true;
            return false;
        }
        // The abandon confirmation owns the input; paste, mouse and global shortcuts cannot
        // bypass it.
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
                // Alt+A toggles the Hub in the main view only; the empty state opens even
                // without a binding.
                if crate::tui::app::hub::is_hub_toggle(&Event::Key(key))
                    && self.hub_toggle_allowed()
                {
                    self.open_hub();
                    *dirty = true;
                    return false;
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
                // After Esc collapses or history recall, an explicit Tab reopens completion.
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
                // The file menu owns the left/right arrows; normal chat and plan discussion share
                // the same navigation behavior.
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
                        MenuEvent::Complete(_) => {
                            self.complete_input();
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
                if self.handle_plan_keys(key) {
                    return false;
                }
                if let Some(panel) = &mut self.plan_list_panel {
                    self.escape_tap.reset();
                    let action = panel.key(key);
                    self.apply_plan_list_action(action);
                    return false;
                }
                if self.ask_panel.is_some()
                    || self.session_panel.is_some()
                    || self.menu.is_some()
                    || self.model_panel.is_some()
                    || self.file_query.is_some()
                {
                    self.escape_tap.reset();
                }
                // Display toggles run before the regular menu and ask, but never through
                // full-screen panels.
                if key.code == KeyCode::Char('o')
                    && key.modifiers == KeyModifiers::CONTROL
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
                // While the session panel is open it consumes keyboard input first; the editor,
                // history browsing and shortcuts do not pass through.
                if self.session_panel.is_some() {
                    let action = self
                        .session_panel
                        .as_mut()
                        .expect("active session panel")
                        .key(key);
                    // A query/range change issues an async Search immediately; results are
                    // installed only for the newest request.
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
                                // Current session: just close the list without relocking its own
                                // journal.
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
                            // Deletion still takes the session lock and removes the JSONL through
                            // the Agent worker; while the panel exists it blocks queue dispatch and
                            // editor input, so Ready is kept.
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
                // Shift + Enter in the main input box always inserts a newline: completion/queue
                // menus, file selection and submission must not preempt it; only terminals with
                // keyboard protocols distinguish it from a plain Enter.
                if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.input.insert_newline();
                    self.refresh_candidates(true);
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
                        match action {
                            MenuAction::Effort(level) if key.modifiers == FUNCTION_MODIFIER => {
                                if self.send(
                                    WorkerCommand::SaveDefaultThinkLevel(level),
                                    State::SavingThinkDefault,
                                ) {
                                    self.clear_input();
                                }
                            }
                            MenuAction::Model(index) if key.modifiers == KeyModifiers::CONTROL => {
                                self.request_save_default(self.choices[index].reference.clone());
                            }
                            _ => {}
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
                        && self.think_switch_ready()
                    {
                        let thinking = self
                            .current_model
                            .as_ref()
                            .map(|model| model.thinking)
                            .unwrap_or_default();
                        // A registered but unapplied desired value takes part in cycling: each
                        // repeated press advances one level.
                        let current = self
                            .pending_think
                            .map_or(self.think_level, |(_, level)| level);
                        let next = thinking.next(current);
                        if next != current {
                            self.submit_think_level(next);
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
                            // The trailing newline must go too, otherwise pressing repeatedly cannot
                            // delete line by line.
                            self.input.cancel_selection();
                            if self.input.delete_line_by_end() {
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
                        MenuEvent::Complete(_) => {
                            self.complete_input();
                            return false;
                        }
                    }
                }
                if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                    self.handle_escape_key(key);
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
                        // Browsing ends only when the currently displayed transcript returns to the
                        // bottom.
                        self.active_transcript().page_down();
                        if self.active_transcript().following() {
                            self.history_browser = false;
                        }
                    }
                    KeyCode::Up | KeyCode::Down if key.modifiers.is_empty() => {
                        let cursor = self.input.cursor();
                        self.edit_input(key);
                        // Move between displayed lines first and browse history only past the first
                        // or last displayed line.
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
            other => {
                self.handle_non_key_event(other, dirty);
            }
        }
        false
    }
}
