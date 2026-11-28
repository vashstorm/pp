//! Plan panel keys: focus switching, action selection, scrolling and paste.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pp_core::plan::ExecutionContext;

use super::{ACTIONS, Action, Focus, Panel};

impl Panel {
    /// Returning false hands the paste to the main input box; a read-only panel swallows it.
    pub(in crate::tui) fn paste(&mut self, _text: &str) -> bool {
        if self.token.is_some() && !self.readonly() {
            self.focus = Focus::Input;
            false
        } else {
            true
        }
    }

    pub(in crate::tui) fn key(&mut self, key: KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return Action::None;
        }
        if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            // Esc only closes the panel: no degradation, no execution, mode and body are kept.
            return Action::Close;
        }
        if self.focus != Focus::Input
            && self.report.is_some()
            && key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Left | KeyCode::Right)
        {
            self.tab = self.tab.toggled();
            return Action::None;
        }
        if self.focus != Focus::Input
            && key.modifiers.is_empty()
            && key.kind == KeyEventKind::Press
            && let KeyCode::Char(number @ '1'..='5') = key.code
        {
            let index = (number as u8 - b'1') as usize;
            if self.action_enabled(index) {
                self.focus = Focus::Actions;
                self.action = index;
            }
            return Action::None;
        }
        if matches!(key.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
        {
            let with_input = self.token.is_some() && !self.readonly();
            let mut focuses = Vec::with_capacity(4);
            if with_input {
                focuses.extend([Focus::Actions, Focus::Input]);
            }
            focuses.push(Focus::Body);
            if self.toc_available() {
                focuses.push(Focus::Toc);
            }
            let current = focuses
                .iter()
                .position(|focus| *focus == self.focus)
                .unwrap_or(0);
            let next = if key.code == KeyCode::BackTab {
                (current + focuses.len() - 1) % focuses.len()
            } else {
                (current + 1) % focuses.len()
            };
            self.focus = focuses[next];
            return Action::None;
        }
        match self.focus {
            Focus::Toc => self.toc_key(key),
            Focus::Body => self.body_key(key),
            Focus::Actions => self.action_key(key),
            Focus::Input => {
                if key.code == KeyCode::Enter && key.modifiers.is_empty() {
                    if key.kind == KeyEventKind::Press {
                        Action::DiscussInput
                    } else {
                        Action::None
                    }
                } else {
                    Action::EditInput(key)
                }
            }
        }
    }

    pub(super) fn toc_available(&self) -> bool {
        self.toc_visible
            && self.layouts[self.tab.index()]
                .as_ref()
                .is_some_and(|layout| !layout.headings.is_empty())
    }

    fn toc_key(&mut self, key: KeyEvent) -> Action {
        let index = self.tab.index();
        let Some(layout) = &self.layouts[index] else {
            return Action::None;
        };
        let selected = &mut self.toc_selected[index];
        let last = layout.headings.len().saturating_sub(1);
        let page = self.toc_viewport.saturating_sub(1).max(1);
        *selected = match key.code {
            KeyCode::Up => selected.saturating_sub(1),
            KeyCode::Down => selected.saturating_add(1).min(last),
            KeyCode::PageUp => selected.saturating_sub(page),
            KeyCode::PageDown => selected.saturating_add(page).min(last),
            KeyCode::Home => 0,
            KeyCode::End => last,
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                if let Some(heading) = layout.headings.get(*selected) {
                    self.scroll[index] = heading.row;
                }
                *selected
            }
            _ => *selected,
        };
        Action::None
    }

    fn body_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::PageUp => {
                self.scroll_page(false);
            }
            KeyCode::PageDown => {
                self.scroll_page(true);
            }
            KeyCode::Up => self.scroll_by(false, 1),
            KeyCode::Down => self.scroll_by(true, 1),
            KeyCode::Left | KeyCode::Right if self.report.is_some() => {
                // Left/right switch the Plan/Review tabs; each keeps its own scroll position.
                self.tab = self.tab.toggled();
            }
            _ => {}
        }
        Action::None
    }

    fn action_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Up | KeyCode::Left => {
                if let Some(index) = (0..self.action)
                    .rev()
                    .find(|&index| self.action_enabled(index))
                {
                    self.action = index;
                }
            }
            KeyCode::Down | KeyCode::Right => {
                if let Some(index) =
                    (self.action + 1..ACTIONS.len()).find(|&index| self.action_enabled(index))
                {
                    self.action = index;
                }
            }
            KeyCode::Enter if key.kind == KeyEventKind::Press => {
                if !self.action_enabled(self.action) {
                    return Action::None;
                }
                return match self.action {
                    0 => Action::Approve(ExecutionContext::Fresh),
                    1 => Action::Approve(ExecutionContext::Keep),
                    2 => Action::Approve(ExecutionContext::Compact),
                    3 => Action::Review,
                    4 => Action::SaveAndLeave,
                    _ => Action::None,
                };
            }
            KeyCode::Char(_) if self.token.is_some() && !self.readonly() => {
                self.focus = Focus::Input;
                return Action::EditInput(key);
            }
            _ => {}
        }
        Action::None
    }

    pub(in crate::tui) fn wheel(&mut self, up: bool, lines: usize) {
        self.scroll_by(!up, lines);
    }

    fn scroll_by(&mut self, down: bool, lines: usize) {
        let slot = &mut self.scroll[self.tab.index()];
        if down {
            *slot = slot.saturating_add(lines);
        } else {
            *slot = slot.saturating_sub(lines);
        }
    }

    fn scroll_page(&mut self, down: bool) {
        let page = self.viewport.max(1).saturating_sub(1);
        self.scroll_by(down, page);
    }
}

#[cfg(test)]
mod tests {
    use super::super::Tab;
    use super::super::fixtures::*;
    use super::*;
    use pp_core::plan::PlanReviewReport;
    use pp_core::plan::PlanStatus;

    #[test]
    fn plan_toc_keeps_plan_and_review_navigation_independent() {
        let content = format!(
            "# Plan start\n\n{}\n\n## Plan end\n\n{}",
            "- item\n".repeat(30),
            "- last\n".repeat(30)
        );
        let mut panel = Panel::progress(document(PlanStatus::Executed, &content));
        panel.report = Some(PlanReviewReport {
            plan_id: panel.document.id.clone(),
            revision: 1,
            model_reference: "fixture/primary".into(),
            content: format!(
                "# Review start\n\n{}\n\n## Review end\n\n{}",
                "- evidence\n".repeat(30),
                "- result\n".repeat(30)
            ),
        });
        render_panel(&mut panel, 14);
        panel.key(key_event(KeyCode::Tab));
        panel.key(key_event(KeyCode::Down));
        panel.key(key_event(KeyCode::Enter));
        render_panel(&mut panel, 14);
        let plan_scroll = panel.scroll[0];
        panel.key(key_event(KeyCode::Right));
        let rows = render_panel(&mut panel, 14);
        assert!(rows.iter().any(|row| row.contains("❯ Review start")));
        panel.key(key_event(KeyCode::Down));
        panel.key(key_event(KeyCode::Enter));
        render_panel(&mut panel, 14);
        assert!(panel.scroll[1] > 0);
        panel.key(key_event(KeyCode::Left));
        let rows = render_panel(&mut panel, 14);
        assert_eq!(panel.scroll[0], plan_scroll);
        assert!(rows.iter().any(|row| row.contains("❯   Plan end")));
    }

    #[test]
    fn plan_actions_number_keys_and_arrows_respect_enabled_options() {
        let mut panel = review_panel(PlanStatus::Ready);
        assert_eq!(
            panel.key(key_event(KeyCode::Enter)),
            Action::Approve(ExecutionContext::Fresh)
        );
        panel.key(key_event(KeyCode::Tab));
        assert!(panel.input_focused());
        assert_eq!(
            panel.key(key_event(KeyCode::Char('1'))),
            Action::EditInput(key_event(KeyCode::Char('1')))
        );
        assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::DiscussInput);
        panel.key(key_event(KeyCode::Tab));
        assert_eq!(panel.focus, Focus::Body);
        assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::None);
        panel.key(key_event(KeyCode::Tab));
        assert_eq!(panel.focus, Focus::Actions);

        for (number, expected) in [
            ('1', Action::Approve(ExecutionContext::Fresh)),
            ('2', Action::Approve(ExecutionContext::Keep)),
            ('3', Action::Approve(ExecutionContext::Compact)),
            ('4', Action::Review),
            ('5', Action::SaveAndLeave),
        ] {
            let mut panel = review_panel(PlanStatus::Ready);
            assert_eq!(panel.key(key_event(KeyCode::Char(number))), Action::None);
            assert_eq!(panel.key(key_event(KeyCode::Enter)), expected);
        }

        for (status, first) in [(PlanStatus::Draft, 4), (PlanStatus::Ready, 0)] {
            let mut panel = review_panel(status);
            assert_eq!(panel.action, first);
            for up in [KeyCode::Up, KeyCode::Left] {
                panel.key(key_event(up));
                assert_eq!(panel.action, first);
            }
            for down in [KeyCode::Down, KeyCode::Right] {
                for expected in first + 1..ACTIONS.len() {
                    panel.key(key_event(down));
                    assert_eq!(panel.action, expected);
                    assert!(panel.action_enabled(panel.action));
                }
                panel.key(key_event(down));
                assert_eq!(panel.action, ACTIONS.len() - 1);
                for expected in (first..ACTIONS.len() - 1).rev() {
                    panel.key(key_event(KeyCode::Up));
                    assert_eq!(panel.action, expected);
                    assert!(panel.action_enabled(panel.action));
                }
            }
        }
    }

    #[test]
    fn draft_and_executed_status_enforce_permissions_and_esc() {
        let mut draft = review_panel(PlanStatus::Draft);
        for index in 0..4 {
            assert!(!draft.action_enabled(index));
        }
        assert!(draft.action_enabled(4));
        assert!(!draft.action_enabled(5));
        draft.key(key_event(KeyCode::Char('1')));
        draft.key(key_event(KeyCode::Char('4')));
        assert_eq!(draft.action, 4);
        assert_eq!(draft.key(key_event(KeyCode::Enter)), Action::SaveAndLeave);
        assert_eq!(
            draft.key(key_event(KeyCode::Char('q'))),
            Action::EditInput(key_event(KeyCode::Char('q')))
        );
        assert!(draft.input_focused());
        assert_eq!(draft.key(key_event(KeyCode::Enter)), Action::DiscussInput);

        let mut executed = review_panel(PlanStatus::Executed);
        executed.key(key_event(KeyCode::Tab));
        for index in 0..ACTIONS.len() {
            assert!(!executed.action_enabled(index));
        }
        assert_eq!(executed.key(key_event(KeyCode::Enter)), Action::None);

        let mut ready = review_panel(PlanStatus::Ready);
        ready.key(key_event(KeyCode::Tab));
        assert!(ready.input_focused());
        assert_eq!(ready.key(key_event(KeyCode::Esc)), Action::Close);
        assert_eq!(ready.document.status, PlanStatus::Ready);
        ready.key(key_event(KeyCode::Tab));
        ready.key(key_event(KeyCode::Tab));
        ready.key(key_event(KeyCode::Char('4')));
        assert_eq!(ready.key(key_event(KeyCode::Enter)), Action::Review);
    }

    #[test]
    fn report_tabs_preserve_separate_scroll_positions() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.report = Some(PlanReviewReport {
            plan_id: panel.document.id.clone(),
            revision: 1,
            model_reference: "model".into(),
            content: "report".into(),
        });
        panel.focus = Focus::Body;
        panel.scroll[0] = 3;
        panel.scroll[1] = 5;
        panel.key(key_event(KeyCode::Right));
        assert_eq!(panel.tab, Tab::Review);
        assert_eq!(panel.scroll[1], 5);
        panel.key(key_event(KeyCode::Left));
        assert_eq!(panel.tab, Tab::Plan);
        assert_eq!(panel.scroll[0], 3);
    }
}
