//! 输入框上方的只读任务区；只持有已提交状态的展示快照。
use super::{tool_display, transcript_style as palette};
use crate::{
    events::TodoState,
    plan::PlanStatus,
    shortcuts::platform_text,
    tools::todo::{TodoItem, TodoStatus},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

pub(super) const TOGGLE_HINT: &str = platform_text("⌥ + T", "Ctrl + T");

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

    fn completed(&self) -> bool {
        !self.empty()
            && !self.preview()
            && self
                .tasks()
                .all(|task| task.status == TodoStatus::Completed)
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
        if width < 4 || cap < 3 {
            return 1;
        }
        if self.expanded {
            self.prepare(width);
            return cap.min(self.rows.len().saturating_add(2).min(u16::MAX as usize) as u16);
        }
        if self.closed() == self.tasks().count() {
            2
        } else {
            cap.min(4)
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
                .saturating_sub(usize::from(self.body().height)),
        );
    }

    fn body(&self) -> Rect {
        Rect::new(
            self.area.x,
            self.area.y + 1,
            self.area.width,
            self.area.height.saturating_sub(2),
        )
    }

    fn summary(&self) -> Line<'static> {
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
                " · Complete"
            }
        } else {
            ""
        };
        let color = if self.completed() {
            palette::ADDED
        } else if self.preview() {
            palette::CODE_BLOCK
        } else {
            palette::ACCENT
        };
        Line::from(vec![
            Span::styled(
                if self.completed() { "✔ TODO" } else { "TODO" },
                Style::default()
                    .fg(if self.completed() {
                        color
                    } else {
                        palette::ACCENT
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(palette::DIM)),
            Span::styled(
                format!("{closed}/{total}{status}"),
                Style::default().fg(color),
            ),
        ])
    }

    fn header(&self, width: u16) -> Line<'static> {
        let action = if self.expanded { "collapse" } else { "expand" };
        let position = if self.expanded && self.rows.len() > usize::from(self.body().height) {
            format!(" · {}/{}", self.scroll + 1, self.rows.len())
        } else {
            String::new()
        };
        let mut summary = self.summary();
        let width = usize::from(width);
        // 先缩短操作和状态文字，尽量保留快捷键提示。
        let controls = [
            format!("{TOGGLE_HINT} {action}{position}"),
            format!("{TOGGLE_HINT} {action}"),
            TOGGLE_HINT.to_owned(),
        ];
        let hint = controls
            .iter()
            .find(|hint| summary.width() + Span::raw(hint.as_str()).width() + 3 <= width);
        let hint = match hint {
            Some(hint) => hint.as_str(),
            None => {
                summary.spans.truncate(1);
                if summary.width() + Span::raw(TOGGLE_HINT).width() + 3 > width {
                    return if Span::raw(TOGGLE_HINT).width() <= width {
                        Line::styled(TOGGLE_HINT, Style::default().fg(palette::MUTED))
                    } else {
                        summary
                    };
                }
                TOGGLE_HINT
            }
        };
        summary
            .spans
            .push(Span::styled(" · ", Style::default().fg(palette::DIM)));
        summary.spans.push(Span::styled(
            hint.to_owned(),
            Style::default().fg(palette::MUTED),
        ));
        summary
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
            let (symbol, marker, text) = tool_display::todo_task_styles(current.status);
            rows.push(Line::from(vec![
                Span::styled("├─ ", Style::default().fg(palette::DIM)),
                Span::styled(format!("{symbol} "), marker),
                Span::styled(one_line(&Self::brief(current)), text),
            ]));
            if let Some(next) = self
                .tasks()
                .find(|task| task.status == TodoStatus::Pending && !std::ptr::eq(*task, current))
            {
                rows.push(Line::from(vec![
                    Span::styled("├─ ", Style::default().fg(palette::DIM)),
                    Span::styled("Next: ", Style::default().fg(palette::MUTED)),
                    Span::styled(
                        one_line(&next.content),
                        Style::default().fg(palette::ACTIVITY),
                    ),
                ]));
            }
        }
        rows
    }

    pub(super) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        if area.width == 0 || area.height == 0 {
            return;
        }
        if area.width < 4 || area.height < 2 {
            frame.render_widget(
                self.header(area.width),
                Rect::new(area.x, area.y, area.width, 1),
            );
            return;
        }
        let body = self.body();
        if self.expanded && body.height > 0 {
            self.prepare(body.width);
            self.clamp_scroll();
        }
        frame.render_widget(
            self.header(area.width),
            Rect::new(area.x, area.y, area.width, 1),
        );
        frame.render_widget(
            Line::styled(
                "└────",
                Style::default().fg(if self.completed() {
                    palette::ADDED
                } else {
                    palette::DIM
                }),
            ),
            Rect::new(area.x, area.bottom() - 1, area.width, 1),
        );
        if body.height == 0 {
            return;
        }
        let mut rows = if self.expanded {
            // 按物理行切片，避免 Paragraph 的 u16 scroll 在超长清单中截断。
            let end = self
                .scroll
                .saturating_add(usize::from(body.height))
                .min(self.rows.len());
            self.rows[self.scroll..end].to_vec()
        } else {
            self.compact()
        };
        rows.resize(
            usize::from(body.height),
            Line::styled("│", Style::default().fg(palette::DIM)),
        );
        frame.render_widget(Paragraph::new(rows), body);
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
        assert_eq!(panel.height(80, 24, 20), 4);
        let text = render(&mut panel, 80, 4);
        assert!(text.contains("1/4"));
        assert!(text.contains("→ 当前任务"));
        assert!(text.contains("Next: 下一任务"));
        assert!(!text.contains("隐藏任务"));
        assert!(!text.contains("已完成"));
        panel.update(state(&[("受阻任务", TodoStatus::Blocked)]));
        assert!(render(&mut panel, 80, 3).contains("受阻任务 (blocked: 外部等待)"));
    }

    #[test]
    fn tui_todo_tree_connects_compact_and_expanded_content() {
        for expanded in [false, true] {
            for width in [4, 12, 80] {
                let mut panel = Panel::default();
                panel.update(state(&[
                    ("当前任务包含很长的中文内容", TodoStatus::InProgress),
                    ("下一任务", TodoStatus::Pending),
                ]));
                panel.expanded = expanded;
                let height = panel.height(width, 24, 20);
                let text = render(&mut panel, width, height);
                assert!(text.lines().nth(1).unwrap().starts_with('├'));
                assert!(text.lines().last().unwrap().starts_with("└──"));
                assert!(!text.contains('╭'));
                assert!(!text.contains('╯'));
                assert!(
                    panel
                        .rows
                        .iter()
                        .all(|row| row.width() <= usize::from(width))
                );
            }
        }
        let mut panel = Panel::default();
        let mut snapshot = state(&[
            ("FIRST", TodoStatus::Completed),
            ("SECOND", TodoStatus::Pending),
        ]);
        snapshot.phases.push(TodoPhase {
            name: "Next phase".into(),
            tasks: vec![TodoItem {
                content: "THIRD".into(),
                status: TodoStatus::Pending,
                blocker: None,
            }],
        });
        panel.update(snapshot);
        panel.toggle();
        let text = render(&mut panel, 80, 8);
        let lines = text.lines().map(str::trim_end).collect::<Vec<_>>();
        assert_eq!(
            &lines[1..6],
            &[
                "├─ I. 验证阶段 (1/2)",
                "│  ├─ ☑ FIRST",
                "│  └─ ☐ SECOND",
                "├─ II. Next phase (0/1)",
                "│  └─ ☐ THIRD",
            ]
        );
    }

    #[test]
    fn tui_todo_completed_abandoned_empty_and_tiny_heights() {
        let mut panel = Panel::default();
        assert_eq!(panel.height(80, 24, 20), 0);
        panel.update(state(&[("完成", TodoStatus::InProgress)]));
        panel.toggle();
        panel.update(state(&[("完成", TodoStatus::Completed)]));
        assert!(!panel.expanded);
        assert_eq!(panel.height(80, 24, 20), 2);
        panel.toggle();
        assert_eq!(panel.height(80, 24, 20), 4);
        panel.toggle();
        assert!(render(&mut panel, 80, 1).contains("✔ TODO · 1/1 · Complete"));
        panel.update(state(&[("放弃", TodoStatus::Abandoned)]));
        let text = render(&mut panel, 80, 2);
        assert!(text.contains("Closed"));
        assert!(!text.contains("Complete"));
        assert!(!text.contains('✔'));
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
    fn tui_todo_colors_indentation_and_completion_marker() {
        let mut panel = Panel::default();
        panel.update(state(&[
            ("ACTIVE", TodoStatus::InProgress),
            ("PENDING", TodoStatus::Pending),
            ("BLOCKED", TodoStatus::Blocked),
            ("DONE", TodoStatus::Completed),
        ]));
        let mut terminal = Terminal::new(TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|frame| panel.draw(frame, Rect::new(0, 0, 80, 4)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(3, 1)].symbol(), "→");
        assert_eq!(buffer[(5, 1)].fg, palette::CODE_BLOCK);
        assert_eq!(buffer[(3, 2)].symbol(), "N");
        assert_eq!(buffer[(3, 2)].fg, palette::MUTED);
        assert_eq!(buffer[(9, 2)].fg, palette::ACTIVITY);
        assert!(buffer[(5, 1)].modifier.contains(Modifier::BOLD));
        panel.toggle();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for (y, symbol, marker, text) in [
            (2, "→", palette::CODE_BLOCK, palette::CODE_BLOCK),
            (3, "☐", palette::MUTED, palette::ACTIVITY),
            (4, "!", palette::ACCENT, palette::ACCENT),
            (5, "☑", palette::ADDED, palette::ADDED),
        ] {
            assert_eq!(buffer[(6, y)].symbol(), symbol);
            assert_eq!(buffer[(6, y)].fg, marker);
            assert_eq!(buffer[(8, y)].fg, text);
        }
        assert!(buffer[(8, 2)].modifier.contains(Modifier::BOLD));
        assert!(buffer[(8, 5)].modifier.contains(Modifier::CROSSED_OUT));
        assert!(!buffer[(6, 5)].modifier.contains(Modifier::CROSSED_OUT));
        panel.update(state(&[("DONE", TodoStatus::Completed)]));
        terminal
            .draw(|frame| panel.draw(frame, Rect::new(0, 0, 80, 2)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "✔");
        assert_eq!(buffer[(0, 1)].fg, palette::ADDED);
        assert_eq!(buffer[(0, 0)].fg, palette::ADDED);
        panel.toggle();
        terminal
            .draw(|frame| panel.draw(frame, Rect::new(0, 0, 80, 4)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(3, 1)].fg, palette::ADDED);
        assert_eq!(buffer[(8, 2)].fg, palette::ADDED);
    }

    #[test]
    fn tui_todo_title_keeps_toggle_hint_when_narrow() {
        let mut panel = Panel::default();
        panel.update(state(&[("任务", TodoStatus::Pending)]));
        for width in [16, 24, 80] {
            for expanded in [false, true] {
                panel.expanded = expanded;
                let text = render(&mut panel, width, 4);
                let title = text.lines().next().unwrap();
                assert!(title.starts_with("TODO"));
                assert!(title.contains(TOGGLE_HINT));
                if width == 80 {
                    let action = if expanded { "collapse" } else { "expand" };
                    assert_eq!(
                        title.trim_end(),
                        format!("TODO · 0/1 · {TOGGLE_HINT} {action}")
                    );
                }
            }
        }
        panel.update(state(&[("完成", TodoStatus::Completed)]));
        let text = render(&mut panel, 24, 2);
        assert!(text.lines().next().unwrap().contains(TOGGLE_HINT));
        assert!(text.contains("✔ TODO"));
    }

    #[test]
    fn tui_todo_wrapped_tasks_align_with_content() {
        let mut panel = Panel::default();
        panel.update(state(&[
            ("abcdefghijklmnopqrstuv\nEXPLICIT", TodoStatus::Pending),
            ("尾项", TodoStatus::Pending),
        ]));
        panel.toggle();
        render(&mut panel, 16, 8);
        let start = panel
            .rows
            .iter()
            .position(|row| row.to_string().starts_with("│  ├─ ☐ abc"))
            .unwrap();
        assert!(panel.rows[start + 1].to_string().starts_with("│  │    i"));
        assert!(
            panel
                .rows
                .iter()
                .any(|row| row.to_string() == "│  │    EXPLICIT")
        );
        assert!(
            panel
                .rows
                .iter()
                .any(|row| row.to_string() == "│  └─ ☐ 尾项")
        );
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
        let height = panel.rows.len() as u16 + 2;
        let text = render(&mut panel, 12, height);
        let joined = text.replace(['\n', ' ', '│'], "");
        assert!(joined.contains("验证阶段"));
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
        assert!(panel.scroll <= panel.rows.len() - 6);
        panel.update(state(&[("最后任务", TodoStatus::Pending)]));
        let text = render(&mut panel, 80, 8);
        assert!(text.contains("最后任务"));
        assert_eq!(panel.scroll, 0);
        panel.reset_view();
        assert!(!panel.expanded);
    }
}
