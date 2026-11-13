//! 计划面板按键：焦点切换、操作选择、滚动与粘贴。

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use pp_core::plan::ExecutionContext;

use super::{ACTIONS, Action, Focus, Panel};

impl Panel {
    /// 返回 false 时由主输入框接收粘贴；只读面板吞掉粘贴。
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
            // Esc 仅关闭面板：不降级、不执行，保留模式与正文。
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
                // 左右键切换 Plan/Review 标签；各自保留滚动位置。
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
    fn plan_actions_start_focused_and_tab_reaches_input_and_body() {
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
    }

    #[test]
    fn plan_number_keys_select_all_five_actions_without_executing() {
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
    }

    #[test]
    fn draft_allows_discussion_and_save_but_requires_submission_to_execute() {
        let mut draft = review_panel(PlanStatus::Draft);
        for index in 0..4 {
            assert!(!draft.action_enabled(index));
        }
        assert!(draft.action_enabled(4));
        assert!(!draft.action_enabled(5));
        draft.key(key_event(KeyCode::Char('1')));
        assert_eq!(draft.action, 4);
        assert_eq!(draft.key(key_event(KeyCode::Enter)), Action::SaveAndLeave);
        assert_eq!(
            draft.key(key_event(KeyCode::Char('q'))),
            Action::EditInput(key_event(KeyCode::Char('q')))
        );
        assert!(draft.input_focused());
        assert_eq!(draft.key(key_event(KeyCode::Enter)), Action::DiscussInput);
    }

    #[test]
    fn plan_action_arrows_only_select_enabled_options_and_stop_at_boundaries() {
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
    fn executed_documents_are_read_only() {
        let mut panel = review_panel(PlanStatus::Executed);
        panel.key(key_event(KeyCode::Tab));
        for index in 0..ACTIONS.len() {
            assert!(!panel.action_enabled(index));
        }
        assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::None);
    }

    #[test]
    fn esc_closes_from_input_and_keeps_ready_state() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.key(key_event(KeyCode::Tab));
        assert!(panel.input_focused());
        assert_eq!(panel.key(key_event(KeyCode::Esc)), Action::Close);
        assert_eq!(panel.document.status, PlanStatus::Ready);
    }

    #[test]
    fn review_action_dispatches_for_ready_documents_only() {
        let mut panel = review_panel(PlanStatus::Ready);
        panel.key(key_event(KeyCode::Char('4')));
        assert_eq!(panel.key(key_event(KeyCode::Enter)), Action::Review);
        let mut draft = review_panel(PlanStatus::Draft);
        draft.key(key_event(KeyCode::Char('4')));
        assert_eq!(draft.action, 4);
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
