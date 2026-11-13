//! 键盘、粘贴与滚轮路由：只推进交互状态，不绘制。

use super::*;

impl Panel {
    pub(in crate::tui) fn key(&mut self, key: KeyEvent) -> Action {
        if self.submitted {
            return Action::None;
        }
        if self.questions.questions.is_empty() {
            return Action::Cancel;
        }
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        if key.kind == KeyEventKind::Repeat && key.code == KeyCode::Enter {
            return Action::None;
        }
        if self.editor.is_some() {
            return self.editor_key(key);
        }
        match self.page {
            Page::Question(index) => self.question_key(index, key),
            Page::Review => self.review_key(key),
        }
    }

    pub(in crate::tui) fn paste(&mut self, text: &str) -> Action {
        if self.editor.is_none()
            && let Page::Question(index) = self.page
            && self.states[index].menu.selected_index()
                == Some(self.questions.questions[index].options.len())
        {
            self.confirm(index, self.questions.questions[index].options.len());
        }
        if let Some((_, editor)) = self.editor.as_mut() {
            insert_custom_text(editor, text);
        }
        Action::None
    }

    /// 仅当滚轮命中面板 overlay 时导航选项列表；否则交还主界面滚动。
    pub(in crate::tui) fn wheel(&mut self, up: bool, column: u16, row: u16) -> bool {
        let Some(area) = self.overlay else {
            return false;
        };
        if column < area.x || column >= area.right() || row < area.y || row >= area.bottom() {
            return false;
        }
        if self.questions.questions.is_empty() || self.editor.is_some() {
            return true;
        }
        let key = KeyEvent::new(
            if up { KeyCode::Up } else { KeyCode::Down },
            KeyModifiers::NONE,
        );
        match self.page {
            Page::Question(index) => {
                self.states[index].menu.handle_key(key);
            }
            Page::Review => {
                if up {
                    self.review_scroll = self.review_scroll.saturating_sub(1);
                } else {
                    self.review_scroll = self.review_scroll.saturating_add(1);
                }
            }
        }
        true
    }

    // ---- 键盘路由 ----

    fn editor_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => {
                // Esc 只丢弃本次编辑，回到 picker。
                self.editor = None;
                self.error = None;
                Action::None
            }
            KeyCode::Enter => {
                let accepted = match self.editor.as_ref() {
                    Some((index, editor)) => {
                        let text = editor.lines().join("\n");
                        (!text.trim().is_empty()).then_some((*index, text))
                    }
                    None => None,
                };
                let Some((index, text)) = accepted else {
                    return Action::None;
                };
                let multi = self.questions.questions[index].multi;
                let state = &mut self.states[index];
                state.custom = text;
                if !multi {
                    state.selected.clear();
                }
                self.editor = None;
                self.rebuild_menu(index);
                self.error = None;
                self.advance(index)
            }
            KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => {
                if let Some((_, editor)) = self.editor.as_mut() {
                    editor.insert_newline();
                }
                Action::None
            }
            _ => {
                if let Some((_, editor)) = self.editor.as_mut() {
                    editor.input(key);
                }
                Action::None
            }
        }
    }

    fn question_key(&mut self, index: usize, key: KeyEvent) -> Action {
        let multi = self.questions.questions[index].multi;
        let option_count = self.questions.questions[index].options.len();
        let focused = self.states[index].menu.selected_index().unwrap_or(0);
        match key.code {
            KeyCode::Esc => Action::Cancel,
            KeyCode::Up if key.modifiers == KeyModifiers::CONTROL => Action::None,
            KeyCode::Down if key.modifiers == KeyModifiers::CONTROL => Action::None,
            KeyCode::Left | KeyCode::BackTab => {
                self.step_page(-1);
                Action::None
            }
            KeyCode::Right | KeyCode::Tab => {
                self.step_page(1);
                Action::None
            }
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                if focused == option_count
                    && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
            {
                self.confirm(index, focused);
                self.editor_key(key)
            }
            KeyCode::Char(' ')
                if key.kind == KeyEventKind::Press
                    && matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
                    && focused < option_count =>
            {
                let state = &mut self.states[index];
                if !multi {
                    state.selected = vec![focused];
                    state.custom.clear();
                } else if let Some(position) = state
                    .selected
                    .iter()
                    .position(|&selected| selected == focused)
                {
                    state.selected.remove(position);
                } else {
                    state.selected.push(focused);
                    state.selected.sort_unstable();
                }
                self.rebuild_menu(index);
                self.error = None;
                Action::None
            }
            _ => match self.states[index].menu.handle_key(key) {
                MenuEvent::Confirm(item) => self.confirm(index, item),
                _ => Action::None,
            },
        }
    }

    fn review_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc => Action::Cancel,
            KeyCode::Enter => self.submit(),
            KeyCode::Up => {
                self.review_scroll = self.review_scroll.saturating_sub(1);
                Action::None
            }
            KeyCode::Down => {
                self.review_scroll = self.review_scroll.saturating_add(1);
                Action::None
            }
            KeyCode::PageUp => {
                self.review_scroll = self.review_scroll.saturating_sub(8);
                Action::None
            }
            KeyCode::PageDown => {
                self.review_scroll = self.review_scroll.saturating_add(8);
                Action::None
            }
            KeyCode::Home => {
                self.review_scroll = 0;
                Action::None
            }
            KeyCode::End => {
                self.review_scroll = usize::MAX;
                Action::None
            }
            KeyCode::Left | KeyCode::BackTab => {
                self.step_page(-1);
                Action::None
            }
            KeyCode::Right | KeyCode::Tab => {
                self.step_page(1);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn confirm(&mut self, index: usize, item: usize) -> Action {
        let option_count = self.questions.questions[index].options.len();
        if item == option_count {
            let mut editor = super::super::editor();
            editor.set_placeholder_text(OTHER_LABEL);
            editor.set_style(Style::default().bg(ASK_BG));
            insert_custom_text(&mut editor, &self.states[index].custom);
            self.editor = Some((index, editor));
            self.error = None;
            return Action::None;
        }
        if self.questions.questions[index].multi {
            // 多选 Enter 只前进，不额外勾选焦点项。
            self.error = None;
            return self.advance(index);
        }
        let state = &mut self.states[index];
        state.selected = vec![item];
        state.custom.clear();
        self.rebuild_menu(index);
        self.error = None;
        self.advance(index)
    }

    fn step_page(&mut self, delta: isize) {
        if !self.has_review() {
            return;
        }
        let pages = self.questions.questions.len() as isize + 1;
        let current = match self.page {
            Page::Question(index) => index as isize,
            Page::Review => pages - 1,
        };
        let next = (current + delta).rem_euclid(pages) as usize;
        self.error = None;
        if next == pages as usize - 1 {
            self.page = Page::Review;
            self.review_scroll = 0;
        } else {
            self.page = Page::Question(next);
        }
    }

    fn advance(&mut self, index: usize) -> Action {
        if index + 1 < self.questions.questions.len() {
            self.page = Page::Question(index + 1);
            return Action::None;
        }
        if self.has_review() {
            self.page = Page::Review;
            self.review_scroll = 0;
            return Action::None;
        }
        self.submit()
    }

    fn submit(&mut self) -> Action {
        for (index, question) in self.questions.questions.iter().enumerate() {
            if question.multi {
                continue;
            }
            let state = &self.states[index];
            if state.selected.is_empty() && state.custom.trim().is_empty() {
                self.error = Some(REQUIRED_MESSAGE.into());
                self.page = Page::Question(index);
                return Action::None;
            }
        }
        self.submitted = true;
        Action::Submit(self.collect_answers())
    }
}
