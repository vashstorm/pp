//! 输入框上方的只读任务区；只持有已提交状态的展示快照。
use super::{tool_display, transcript_style as palette};
use crate::{
    events::TodoState,
    plan::PlanStatus,
    shortcuts::platform_text,
    tools::todo::{TodoItem, TodoStatus},
};
use ratatui::{Frame, layout::Rect, style::Style, text::Line, widgets::Paragraph};

pub(super) const TOGGLE_HINT: &str = platform_text("⌥T", "Ctrl+T");

#[derive(Default)]
pub(super) struct Panel {
    state: TodoState,
    pub(super) expanded: bool,
    scroll: usize,
    width: Option<u16>,
    rows: Vec<Line<'static>>,
    pub(super) area: Rect,
}

impl Panel {
    pub(super) fn update(&mut self, state: TodoState) {
        if self.state == state {
            return;
        }
        if self.state.plan.as_ref().map(|plan| &plan.id) != state.plan.as_ref().map(|plan| &plan.id)
        {
            self.reset_view();
        }
        self.state = state;
        self.width = None;
        self.rows.clear();
        if self.empty() || self.closed() == self.tasks().count() {
            self.reset_view();
        }
    }

    pub(super) fn reset_view(&mut self) {
        self.expanded = false;
        self.scroll = 0;
        self.area = Rect::default();
    }

    pub(super) fn empty(&self) -> bool {
        self.state.phases.iter().all(|phase| phase.tasks.is_empty())
    }

    fn tasks(&self) -> impl Iterator<Item = &TodoItem> {
        self.state.phases.iter().flat_map(|phase| &phase.tasks)
    }

    fn closed(&self) -> usize {
        self.tasks()
            .filter(|task| matches!(task.status, TodoStatus::Completed | TodoStatus::Abandoned))
            .count()
    }

    fn preview(&self) -> bool {
        self.state
            .plan
            .as_ref()
            .is_some_and(|plan| matches!(plan.status, PlanStatus::Draft | PlanStatus::Ready))
    }

    fn prepare(&mut self, width: u16) {
        if self.width != Some(width) {
            self.rows = tool_display::todo_checklist(&self.state.phases, width);
            self.width = Some(width);
        }
    }

    pub(super) fn height(&mut self, width: u16, terminal_height: u16, budget: u16) -> u16 {
        if self.empty() || width == 0 || budget == 0 {
            return 0;
        }
        let cap = budget.min((terminal_height / 3).max(1));
        if self.expanded && cap >= 3 {
            self.prepare(width);
            return cap.min(self.rows.len().saturating_add(1).min(u16::MAX as usize) as u16);
        }
        if self.closed() == self.tasks().count() || cap < 3 {
            1
        } else {
            3
        }
    }

    pub(super) fn toggle(&mut self) {
        if !self.empty() {
            self.expanded = !self.expanded;
            self.scroll = 0;
        }
    }

    pub(super) fn scroll(&mut self, up: bool, amount: usize) {
        if !self.expanded || self.area.height < 3 {
            return;
        }
        self.scroll = if up {
            self.scroll.saturating_sub(amount)
        } else {
            self.scroll.saturating_add(amount)
        };
        self.clamp_scroll();
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(
            self.rows
                .len()
                .saturating_sub(usize::from(self.area.height.saturating_sub(1))),
        );
    }

    fn header(&self) -> String {
        let total = self.tasks().count();
        let closed = self.closed();
        let status = if self.preview() {
            " · Preview"
        } else if closed == total {
            if self
                .tasks()
                .any(|task| task.status == TodoStatus::Abandoned)
            {
                " · Closed"
            } else {
                " · Complete ✓"
            }
        } else {
            ""
        };
        let action = if self.expanded { "collapse" } else { "expand" };
        let position =
            if self.expanded && self.rows.len() > usize::from(self.area.height.saturating_sub(1)) {
                format!(" · {}/{}", self.scroll + 1, self.rows.len())
            } else {
                String::new()
            };
        format!("─ TODO · {closed}/{total}{status} · {TOGGLE_HINT} {action}{position}")
    }

    fn brief(task: &TodoItem) -> String {
        if task.status == TodoStatus::Blocked {
            match &task.blocker {
                Some(reason) => format!("{} (blocked: {reason})", task.content),
                None => format!("{} (blocked)", task.content),
            }
        } else {
            task.content.clone()
        }
    }

    fn compact(&self) -> Vec<Line<'static>> {
        let current = self
            .tasks()
            .find(|task| !self.preview() && task.status == TodoStatus::InProgress)
            .or_else(|| self.tasks().find(|task| task.status == TodoStatus::Pending))
            .or_else(|| self.tasks().find(|task| task.status == TodoStatus::Blocked));
        let mut rows = Vec::new();
        if let Some(current) = current {
            let prefix = if current.status == TodoStatus::InProgress && !self.preview() {
                "→ "
            } else {
                "☐ "
            };
            rows.push(Line::styled(
                one_line(&format!("{prefix}{}", Self::brief(current))),
                Style::default().fg(palette::ACCENT),
            ));
            if let Some(next) = self
                .tasks()
                .find(|task| task.status == TodoStatus::Pending && !std::ptr::eq(*task, current))
            {
                rows.push(Line::styled(
                    one_line(&format!("  Next: {}", next.content)),
                    Style::default().fg(palette::MUTED),
                ));
            }
        }
        rows
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        if area.width == 0 || area.height == 0 {
            return;
        }
        if self.expanded && area.height >= 3 {
            self.prepare(area.width);
            self.clamp_scroll();
        }
        frame.render_widget(
            Line::styled(self.header(), Style::default().fg(palette::MUTED)),
            Rect::new(area.x, area.y, area.width, 1),
        );
        let body = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
        if body.height == 0 {
            return;
        }
        if self.expanded && area.height >= 3 {
            // 按物理行切片，避免 Paragraph 的 u16 scroll 在超长清单中截断。
            let end = self
                .scroll
                .saturating_add(usize::from(body.height))
                .min(self.rows.len());
            frame.render_widget(Paragraph::new(self.rows[self.scroll..end].to_vec()), body);
        } else {
            frame.render_widget(Paragraph::new(self.compact()), body);
        }
    }
}

fn one_line(text: &str) -> String {
    palette::display_text(text).replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::todo::TodoPhase;
    use ratatui::{Terminal, backend::TestBackend, style::Modifier};

    fn state(tasks: &[(&str, TodoStatus)]) -> TodoState {
        TodoState {
            phases: vec![TodoPhase {
                name: "验证阶段".into(),
                tasks: tasks
                    .iter()
                    .map(|(text, status)| TodoItem {
                        content: (*text).into(),
                        status: *status,
                        blocker: (*status == TodoStatus::Blocked).then(|| "外部等待".into()),
                    })
                    .collect(),
            }],
            plan: None,
        }
    }

    fn render(panel: &mut Panel, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .chunks(usize::from(width))
            .map(|row| {
                let mut skip = 0;
                row.iter()
                    .filter_map(|cell| {
                        if skip > 0 {
                            skip -= 1;
                            return None;
                        }
                        skip = ratatui::text::Span::raw(cell.symbol())
                            .width()
                            .saturating_sub(1);
                        Some(cell.symbol())
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn tui_todo_compact_selects_active_then_next_and_blocked_fallback() {
        let mut panel = Panel::default();
        panel.update(state(&[
            ("已完成", TodoStatus::Completed),
            ("当前任务", TodoStatus::InProgress),
            ("下一任务", TodoStatus::Pending),
            ("隐藏任务", TodoStatus::Pending),
        ]));
        assert_eq!(panel.height(80, 24, 20), 3);
        let text = render(&mut panel, 80, 3);
        assert!(text.contains("1/4"));
        assert!(text.contains("→ 当前任务"));
        assert!(text.contains("Next: 下一任务"));
        assert!(!text.contains("隐藏任务"));
        assert!(!text.contains("已完成"));
        panel.update(state(&[("受阻任务", TodoStatus::Blocked)]));
        assert!(render(&mut panel, 80, 3).contains("受阻任务 (blocked: 外部等待)"));
    }

    #[test]
    fn tui_todo_completed_abandoned_empty_and_tiny_heights() {
        let mut panel = Panel::default();
        assert_eq!(panel.height(80, 24, 20), 0);
        panel.update(state(&[("完成", TodoStatus::InProgress)]));
        panel.toggle();
        panel.update(state(&[("完成", TodoStatus::Completed)]));
        assert!(!panel.expanded);
        assert_eq!(panel.height(80, 24, 20), 1);
        panel.toggle();
        assert_eq!(panel.height(80, 24, 20), 3);
        panel.toggle();
        assert!(render(&mut panel, 80, 1).contains("Complete ✓"));
        panel.update(state(&[("放弃", TodoStatus::Abandoned)]));
        let text = render(&mut panel, 80, 1);
        assert!(text.contains("Closed"));
        assert!(!text.contains("Complete"));
        panel.update(state(&[("待办", TodoStatus::Pending)]));
        assert_eq!(panel.height(80, 24, 2), 1);
        assert_eq!(panel.height(80, 24, 0), 0);
        assert_eq!(panel.height(0, 24, 20), 0);
        panel.toggle();
        assert_eq!(panel.height(80, 6, 5), 1);
        panel.update(TodoState::default());
        assert!(!panel.expanded);
        assert_eq!(panel.height(80, 24, 20), 0);
    }

    #[test]
    fn tui_todo_preview_never_claims_execution() {
        let mut panel = Panel::default();
        let mut snapshot = state(&[("计划任务", TodoStatus::Pending)]);
        snapshot.plan = Some(crate::events::TodoPlanState {
            id: "plan-1".into(),
            revision: 1,
            status: PlanStatus::Draft,
        });
        panel.update(snapshot);
        let text = render(&mut panel, 80, 3);
        assert!(text.contains("Preview"));
        assert!(text.contains("☐ 计划任务"));
        assert!(!text.contains('→'));
    }

    #[test]
    fn tui_todo_expanded_wraps_safe_text_and_preserves_styles() {
        let mut panel = Panel::default();
        panel.update(state(&[
            ("读取\u{1b}[31m状态\u{7f}\t列", TodoStatus::Completed),
            ("很长的中文任务名称必须完整换行展示", TodoStatus::Blocked),
        ]));
        panel.toggle();
        panel.height(12, 90, 30);
        let text = render(&mut panel, 12, 30);
        assert!(text.contains("验证阶段"));
        let joined = text.replace(['\n', ' '], "");
        assert!(joined.contains("很长的中文任务名称必须完整换行展示"));
        assert!(joined.contains("外部等待"));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{7f}'));
        assert!(
            panel
                .rows
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.style.add_modifier.contains(Modifier::CROSSED_OUT))
        );
    }

    #[test]
    fn tui_todo_full_checklist_keeps_duplicate_hints_and_all_states() {
        let mut panel = Panel::default();
        panel.update(state(&[
            ("重复任务", TodoStatus::InProgress),
            ("重复任务", TodoStatus::Pending),
            ("已放弃", TodoStatus::Abandoned),
            ("已完成", TodoStatus::Completed),
        ]));
        panel.toggle();
        let text = render(&mut panel, 100, 10);
        assert!(text.contains("I. 验证阶段 (2/4)"));
        assert!(text.contains("phase \"验证阶段\" occurrence 1"));
        assert!(text.contains("phase \"验证阶段\" occurrence 2"));
        assert!(text.contains("已放弃 (dropped)"));
        assert!(text.contains("☑ 已完成"));
    }

    #[test]
    fn tui_todo_expanded_scroll_clamps_after_updates_and_resize() {
        let mut panel = Panel::default();
        panel.update(state(
            &(0..30)
                .map(|_| ("待办", TodoStatus::Pending))
                .collect::<Vec<_>>(),
        ));
        panel.toggle();
        assert_eq!(panel.height(80, 24, 20), 8);
        render(&mut panel, 80, 8);
        panel.scroll(false, usize::MAX);
        let previous = panel.scroll;
        assert!(previous > 0);
        render(&mut panel, 20, 8);
        assert!(panel.scroll <= panel.rows.len() - 7);
        panel.update(state(&[("最后任务", TodoStatus::Pending)]));
        let text = render(&mut panel, 80, 8);
        assert!(text.contains("最后任务"));
        assert_eq!(panel.scroll, 0);
        panel.reset_view();
        assert!(!panel.expanded);
    }
}
