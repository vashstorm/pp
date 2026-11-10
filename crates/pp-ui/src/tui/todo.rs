//! 输入框上方的只读任务区；只持有已提交状态的展示快照。
use super::{tool_display, transcript_style as palette};
use crate::shortcuts::platform_text;
use pp_core::events::TodoState;
use pp_core::plan::PlanStatus;
use pp_core::todo::TodoItem;
use pp_core::todo::TodoPhase;
use pp_core::todo::TodoStatus;
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

pub(super) const TOGGLE_HINT: &str = platform_text("⌥ + T", "Ctrl + T");

/// 收起时最多展示的任务数；阶段行不占这个额度。
const COMPACT_TASKS: usize = 3;

#[derive(Default)]
pub(super) struct Panel {
    state: TodoState,
    pub(super) expanded: bool,
    scroll: usize,
    width: Option<u16>,
    rows: Vec<Line<'static>>,
    task_rows: Vec<usize>,
    focus_pending: bool,
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
        self.task_rows.clear();
        self.focus_pending = true;
        if self.empty() {
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

    /// 按阶段展开任务，同时给出阶段序号与阶段本身；收起窗口靠它插入阶段行。
    fn listing(&self) -> impl Iterator<Item = (usize, &TodoPhase, &TodoItem)> {
        self.state
            .phases
            .iter()
            .enumerate()
            .flat_map(|(index, phase)| phase.tasks.iter().map(move |task| (index, phase, task)))
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
            let checklist = tool_display::todo_checklist(&self.state.phases, width);
            self.rows = checklist.rows;
            self.task_rows = checklist.task_rows;
            self.width = Some(width);
            self.focus_pending = true;
        }
    }

    pub(super) fn height(&mut self, width: u16, terminal_height: u16, budget: u16) -> u16 {
        if self.empty() || width == 0 || budget == 0 {
            return 0;
        }
        let cap = budget.min((terminal_height / 3).max(1));
        if width < 4 || cap < 2 {
            return 1;
        }
        if self.expanded {
            self.prepare(width);
            return cap.min(self.rows.len().saturating_add(1).min(u16::MAX as usize) as u16);
        }
        let visible = self.tasks().count().min(COMPACT_TASKS);
        cap.min(self.compact_row_count(visible) as u16 + 1)
    }

    pub(super) fn toggle(&mut self) {
        if !self.empty() {
            self.expanded = !self.expanded;
            self.scroll = 0;
            self.focus_pending = true;
        }
    }

    pub(super) fn scroll(&mut self, up: bool, amount: usize) {
        if !self.expanded || self.area.height < 2 {
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
            self.area.height.saturating_sub(1),
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
                if self.completed() {
                    "\u{f00c} TODO"
                } else {
                    "TODO"
                },
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

    fn focus_task(&self) -> usize {
        self.tasks()
            .position(|task| !self.preview() && task.status == TodoStatus::InProgress)
            .or_else(|| {
                self.tasks()
                    .position(|task| task.status == TodoStatus::Pending)
            })
            .or_else(|| {
                self.tasks()
                    .position(|task| task.status == TodoStatus::Blocked)
            })
            .unwrap_or_else(|| self.tasks().count().saturating_sub(1))
    }

    /// 收起窗口内的任务区间：尽量让焦点项居中，首尾用相邻项补齐。
    fn window(&self, visible: usize) -> (usize, usize) {
        let count = self.tasks().count();
        let visible = visible.min(count);
        if visible == 0 {
            return (0, 0);
        }
        let start = self
            .focus_task()
            .saturating_sub(visible / 2)
            .min(count - visible);
        (start, start + visible)
    }

    /// 收起窗口的行数：每个进入新阶段的位置多一行阶段头。
    fn compact_row_count(&self, visible: usize) -> usize {
        let (start, end) = self.window(visible);
        let mut rows = 0;
        let mut phase = None;
        for (index, _, _) in self.listing().skip(start).take(end - start) {
            if phase != Some(index) {
                phase = Some(index);
                rows += 1;
            }
            rows += 1;
        }
        rows
    }

    /// 收起窗口：阶段头只省略同一窗口内重复的阶段，不再整体省略。
    fn compact(&self, limit: usize, width: u16) -> Vec<Line<'static>> {
        let max = self.tasks().count().min(COMPACT_TASKS);
        if limit == 0 || max == 0 {
            return Vec::new();
        }
        let Some(visible) = (1..=max)
            .rev()
            .find(|visible| self.compact_row_count(*visible) <= limit)
        else {
            // 连阶段头都放不下时退化为单行任务，避免整块空白。
            let (start, end) = self.window(1);
            return self.compact_rows(start, end, width, false);
        };
        let (start, end) = self.window(visible);
        self.compact_rows(start, end, width, true)
    }

    fn compact_rows(
        &self,
        start: usize,
        end: usize,
        width: u16,
        headers: bool,
    ) -> Vec<Line<'static>> {
        let window: Vec<(usize, &TodoPhase, &TodoItem)> = self
            .listing()
            .skip(start)
            .take(end.saturating_sub(start))
            .collect();
        let mut rows = Vec::with_capacity(window.len().saturating_mul(2));
        for (position, (index, phase, task)) in window.iter().enumerate() {
            let next = window.get(position + 1).map(|entry| entry.0);
            let entering = position == 0 || window[position - 1].0 != *index;
            if headers && entering {
                let mut header = Line::styled(
                    tool_display::todo_phase_prefix(width),
                    Style::default().fg(palette::DIM),
                );
                header
                    .spans
                    .extend(tool_display::todo_phase_line(phase, *index).spans);
                rows.push(header);
            }
            let (symbol, marker, text) = tool_display::todo_task_styles(task.status);
            let last = next != Some(*index);
            let mut row = Line::styled(
                tool_display::todo_task_prefix(width, last),
                Style::default().fg(palette::DIM),
            );
            if width >= 6 {
                row.spans.push(Span::styled(format!("{symbol} "), marker));
            }
            row.spans
                .push(Span::styled(one_line(&Self::brief(task)), text));
            rows.push(row);
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
            if self.focus_pending {
                // 按排版时记录的任务首行定位，阶段标题和正文折行不影响寻址。
                if let Some(row) = self.task_rows.get(self.focus_task()) {
                    self.scroll = row.saturating_sub(usize::from(body.height) / 2);
                }
                self.focus_pending = false;
            }
            self.clamp_scroll();
        }
        frame.render_widget(
            self.header(area.width),
            Rect::new(area.x, area.y, area.width, 1),
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
            self.compact(usize::from(body.height), body.width)
        };
        rows.resize(usize::from(body.height), Line::default());
        frame.render_widget(Paragraph::new(rows), body);
    }
}

fn one_line(text: &str) -> String {
    pp_core::text::display_text(text).replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pp_core::todo::TodoPhase;
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
    fn tui_todo_compact_centers_active_between_neighbors_and_blocked_fallback() {
        let mut panel = Panel::default();
        panel.update(state(&[
            ("已完成", TodoStatus::Completed),
            ("当前任务", TodoStatus::InProgress),
            ("下一任务", TodoStatus::Pending),
            ("隐藏任务", TodoStatus::Pending),
        ]));
        assert_eq!(panel.height(80, 24, 20), 5);
        let text = render(&mut panel, 80, 5);
        assert!(text.contains("1/4"));
        assert!(text.contains("[ ] 当前任务"));
        assert!(text.contains("[ ] 下一任务"));
        assert!(!text.contains("隐藏任务"));
        assert!(text.contains("[\u{f00c}] 已完成"));
        // 收起时保留阶段行，首个任务行内缩在阶段下。
        assert!(text.lines().nth(1).unwrap().contains("I. 验证阶段 (1/4)"));
        assert!(text.lines().nth(1).unwrap().starts_with("├─ "));
        assert!(text.lines().nth(3).unwrap().contains("当前任务"));
        assert!(text.lines().nth(3).unwrap().starts_with("│  ├─ "));
        panel.update(state(&[("受阻任务", TodoStatus::Blocked)]));
        let text = render(&mut panel, 80, 3);
        assert!(text.contains("I. 验证阶段 (0/1)"));
        assert!(text.contains("受阻任务 (blocked: 外部等待)"));
    }

    #[test]
    fn tui_todo_compact_keeps_phase_rows_of_every_visible_phase() {
        let mut panel = Panel::default();
        let mut snapshot = state(&[("FIRST", TodoStatus::Completed)]);
        snapshot.phases.push(TodoPhase {
            name: "Next phase".into(),
            tasks: vec![
                TodoItem {
                    content: "SECOND".into(),
                    status: TodoStatus::InProgress,
                    blocker: None,
                },
                TodoItem {
                    content: "THIRD".into(),
                    status: TodoStatus::Pending,
                    blocker: None,
                },
            ],
        });
        panel.update(snapshot);
        assert_eq!(panel.height(80, 24, 20), 6);
        let text = render(&mut panel, 80, 6);
        let lines = text.lines().map(str::trim_end).collect::<Vec<_>>();
        assert_eq!(
            &lines[1..6],
            &[
                "├─ I. 验证阶段 (1/1)",
                "│  └─ [\u{f00c}] FIRST",
                "├─ II. Next phase (0/2)",
                "│  ├─ [ ] SECOND",
                "│  └─ [ ] THIRD",
            ]
        );
    }

    #[test]
    fn tui_todo_three_item_window_tracks_progress_and_keeps_completed_items() {
        for active in 0..6 {
            let mut panel = Panel::default();
            let mut snapshot = state(&[]);
            snapshot.phases[0].tasks = (0..6)
                .map(|index| TodoItem {
                    content: format!("TASK_{index}"),
                    status: if index < active {
                        TodoStatus::Completed
                    } else if index == active {
                        TodoStatus::InProgress
                    } else {
                        TodoStatus::Pending
                    },
                    blocker: None,
                })
                .collect();
            panel.update(snapshot.clone());
            let height = panel.height(80, 30, 20);
            assert_eq!(height, 5);
            let text = render(&mut panel, 80, height);
            assert_eq!(text.matches("TASK_").count(), 3);
            assert!(text.lines().nth(1).unwrap().contains("I. 验证阶段"));
            assert!(text.lines().last().unwrap().starts_with("│  └─ "));
            let active_row = if active == 0 {
                2
            } else if active == 5 {
                4
            } else {
                3
            };
            assert!(
                text.lines()
                    .nth(active_row)
                    .unwrap()
                    .contains(&format!("TASK_{active}"))
            );
            // 完成状态更新后保留任务窗口，也保留用户选择的展开状态。
            for task in &mut snapshot.phases[0].tasks {
                task.status = TodoStatus::Completed;
            }
            panel.update(snapshot.clone());
            let text = render(&mut panel, 80, 5);
            assert_eq!(text.matches("[\u{f00c}]").count(), 3);
            assert!(text.lines().last().unwrap().starts_with("│  └─ "));
            for index in 3..6 {
                assert!(text.contains(&format!("TASK_{index}")));
            }
            panel.toggle();
            snapshot.phases[0].name = "完成阶段".into();
            panel.update(snapshot);
            assert!(panel.expanded);
        }
    }

    #[test]
    fn tui_todo_expanded_centers_active_and_preserves_manual_scroll() {
        let mut panel = Panel::default();
        let mut snapshot = state(&[]);
        snapshot.phases[0].tasks = (0..12)
            .map(|index| TodoItem {
                content: format!("TASK_{index:02}"),
                status: if index == 6 {
                    TodoStatus::InProgress
                } else {
                    TodoStatus::Pending
                },
                blocker: None,
            })
            .collect();
        panel.update(snapshot);
        panel.toggle();
        let height = panel.height(80, 30, 20);
        let text = render(&mut panel, 80, height);
        assert!(text.lines().nth(5).unwrap().contains("TASK_06"));
        assert!(text.matches("TASK_").count() >= 3);
        panel.scroll(true, usize::MAX);
        let text = render(&mut panel, 80, height);
        assert!(text.contains("I. 验证阶段"));
        assert_eq!(panel.scroll, 0);
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
                if !expanded || width == 80 {
                    assert!(text.lines().nth(1).unwrap().starts_with('├'));
                } else {
                    assert!(text.lines().nth(1).unwrap().starts_with(['│', '├']));
                }
                assert!(!text.lines().any(|line| line.trim_end() == "└────"));
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
                "│  ├─ [\u{f00c}] FIRST",
                "│  └─ [ ] SECOND",
                "├─ II. Next phase (0/1)",
                "│  └─ [ ] THIRD",
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
        assert!(panel.expanded);
        assert_eq!(panel.height(80, 24, 20), 3);
        panel.toggle();
        assert_eq!(panel.height(80, 24, 20), 3);
        assert!(render(&mut panel, 80, 3).contains("[\u{f00c}] 完成"));
        // 只有一行正文时不丢任务，仅暂时省略无法放下的阶段行。
        assert!(render(&mut panel, 80, 2).contains("[\u{f00c}] 完成"));
        assert!(render(&mut panel, 80, 1).contains("\u{f00c} TODO · 1/1 · Complete"));
        panel.update(state(&[("放弃", TodoStatus::Abandoned)]));
        let text = render(&mut panel, 80, 2);
        assert!(text.contains("Closed"));
        assert!(!text.contains("Complete"));
        assert!(!text.contains('\u{f00c}'));
        panel.update(state(&[("待办", TodoStatus::Pending)]));
        assert_eq!(panel.height(80, 24, 2), 2);
        assert_eq!(panel.height(80, 24, 0), 0);
        assert_eq!(panel.height(0, 24, 20), 0);
        panel.toggle();
        assert_eq!(panel.height(80, 6, 5), 2);
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
            .draw(|frame| panel.draw(frame, Rect::new(0, 0, 80, 6)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        // 第一行是阶段行，任务行内缩两列。
        assert_eq!(buffer[(3, 1)].symbol(), "I");
        assert_eq!(buffer[(6, 2)].symbol(), "[");
        assert_eq!(buffer[(7, 2)].fg, palette::TODO_ACTIVE);
        assert_eq!(buffer[(6, 3)].symbol(), "[");
        assert_eq!(buffer[(6, 3)].fg, palette::MUTED);
        assert_eq!(buffer[(7, 3)].fg, palette::MUTED);
        assert!(buffer[(10, 2)].modifier.contains(Modifier::BOLD));
        panel.toggle();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for (y, symbol, marker, text) in [
            (2, " ", palette::TODO_ACTIVE, palette::TODO_ACTIVE),
            (3, " ", palette::MUTED, palette::MUTED),
            (4, "!", palette::ACCENT, palette::ACCENT),
            (5, "\u{f00c}", palette::ADDED, palette::ADDED),
        ] {
            assert_eq!(buffer[(6, y)].symbol(), "[");
            assert_eq!(buffer[(7, y)].symbol(), symbol);
            assert_eq!(buffer[(8, y)].symbol(), "]");
            assert_eq!(buffer[(6, y)].fg, marker);
            assert_eq!(buffer[(10, y)].fg, text);
        }
        assert!(buffer[(10, 2)].modifier.contains(Modifier::BOLD));
        assert!(!buffer[(10, 5)].modifier.contains(Modifier::CROSSED_OUT));
        assert!(!buffer[(6, 5)].modifier.contains(Modifier::CROSSED_OUT));
        panel.update(state(&[("DONE", TodoStatus::Completed)]));
        terminal
            .draw(|frame| panel.draw(frame, Rect::new(0, 0, 80, 2)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].symbol(), "\u{f00c}");
        assert_eq!(buffer[(7, 1)].fg, palette::ADDED);
        assert_eq!(buffer[(0, 0)].fg, palette::ADDED);
        terminal
            .draw(|frame| panel.draw(frame, Rect::new(0, 0, 80, 4)))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(3, 1)].fg, palette::ADDED);
        assert_eq!(buffer[(6, 2)].fg, palette::ADDED);
        assert_eq!(buffer[(10, 2)].fg, palette::ADDED);
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
        assert!(text.contains("\u{f00c} TODO"));
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
            .position(|row| row.to_string().starts_with("│  ├─ [ ] abc"))
            .unwrap();
        assert!(panel.rows[start + 1].to_string().starts_with("│  │      g"));
        assert!(
            panel
                .rows
                .iter()
                .any(|row| row.to_string() == "│  │      EXPLIC")
        );
        assert!(
            panel
                .rows
                .iter()
                .any(|row| row.to_string() == "│  └─ [ ] 尾项")
        );
    }

    #[test]
    fn tui_todo_preview_never_claims_execution() {
        let mut panel = Panel::default();
        let mut snapshot = state(&[("计划任务", TodoStatus::Pending)]);
        snapshot.plan = Some(pp_core::events::TodoPlanState {
            id: "plan-1".into(),
            revision: 1,
            status: PlanStatus::Draft,
        });
        panel.update(snapshot);
        let text = render(&mut panel, 80, 3);
        assert!(text.contains("Preview"));
        assert!(text.contains("[ ] 计划任务"));
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
        let height = panel.rows.len() as u16 + 1;
        let text = render(&mut panel, 12, height);
        let joined = text.replace(['\n', ' ', '│'], "");
        assert!(joined.contains("验证阶段"));
        assert!(joined.contains("很长的中文任务名称必须完整换行展示"));
        assert!(joined.contains("外部等待"));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\u{7f}'));
        assert!(
            !panel
                .rows
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.style.add_modifier.contains(Modifier::CROSSED_OUT))
        );
    }

    #[test]
    fn tui_todo_expanded_shows_all_parallel_items_in_progress() {
        let mut panel = Panel::default();
        panel.update(state(&[
            ("A", TodoStatus::InProgress),
            ("B", TodoStatus::InProgress),
            ("C", TodoStatus::InProgress),
            ("D", TodoStatus::InProgress),
            ("E", TodoStatus::Pending),
        ]));
        panel.toggle();
        let text = render(&mut panel, 100, 24);
        assert_eq!(text.matches("(in progress)").count(), 4, "{text}");
        for task in ["A", "B", "C", "D"] {
            assert!(
                text.contains(&format!("[ ] {task} (in progress)")),
                "{text}"
            );
        }
        assert!(!text.contains("E (in progress)"), "{text}");
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
        assert!(text.contains("[\u{f00c}] 已完成"));
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
