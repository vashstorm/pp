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
            palette::added()
        } else if self.preview() {
            palette::code_block()
        } else {
            palette::accent()
        };
        let done_prefix = if crate::tui::themes::is_ascii() {
            "+ TODO"
        } else {
            "\u{f00c} TODO"
        };
        Line::from(vec![
            Span::styled(
                if self.completed() {
                    done_prefix
                } else {
                    "TODO"
                },
                Style::default()
                    .fg(if self.completed() {
                        color
                    } else {
                        palette::accent()
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(palette::dim())),
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
                        Line::styled(TOGGLE_HINT, Style::default().fg(palette::muted()))
                    } else {
                        summary
                    };
                }
                TOGGLE_HINT
            }
        };
        summary
            .spans
            .push(Span::styled(" · ", Style::default().fg(palette::dim())));
        summary.spans.push(Span::styled(
            hint.to_owned(),
            Style::default().fg(palette::muted()),
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
                    Style::default().fg(palette::dim()),
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
                Style::default().fg(palette::dim()),
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
    fn tui_todo_compact_centers_active_phases_and_window() {
        let mut panel = Panel::default();
        panel.update(state(&[
            ("已完成", TodoStatus::Completed),
            ("当前任务", TodoStatus::InProgress),
            ("下一任务", TodoStatus::Pending),
            ("隐藏任务", TodoStatus::Pending),
        ]));
        assert_eq!(panel.height(80, 24, 20), 5);
        let text = render(&mut panel, 80, 5);
        assert!(
            text.contains("1/4")
                && text.contains("[ ] 当前任务")
                && text.contains("[ ] 下一任务")
                && !text.contains("隐藏任务")
        );
        assert!(
            text.lines().nth(1).unwrap().starts_with("├─ ")
                && text.lines().nth(3).unwrap().starts_with("│  ├─ ")
        );
        panel.update(state(&[("受阻任务", TodoStatus::Blocked)]));
        assert!(render(&mut panel, 80, 3).contains("受阻任务 (blocked: 外部等待)"));

        let mut multi = Panel::default();
        let mut snap_m = state(&[("FIRST", TodoStatus::Completed)]);
        snap_m.phases.push(TodoPhase {
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
        multi.update(snap_m);
        assert_eq!(multi.height(80, 24, 20), 6);
        let m_text = render(&mut multi, 80, 6);
        let m_lines: Vec<_> = m_text.lines().map(str::trim_end).collect();
        assert_eq!(
            &m_lines[1..6],
            &[
                "├─ I. 验证阶段 (1/1)",
                "│  └─ [\u{f00c}] FIRST",
                "├─ II. Next phase (0/2)",
                "│  ├─ [ ] SECOND",
                "│  └─ [ ] THIRD"
            ]
        );

        for active in 0..6 {
            let mut wp = Panel::default();
            let mut snapshot = state(&[]);
            snapshot.phases[0].tasks = (0..6)
                .map(|i| TodoItem {
                    content: format!("TASK_{i}"),
                    status: if i < active {
                        TodoStatus::Completed
                    } else if i == active {
                        TodoStatus::InProgress
                    } else {
                        TodoStatus::Pending
                    },
                    blocker: None,
                })
                .collect();
            wp.update(snapshot.clone());
            assert_eq!(wp.height(80, 30, 20), 5);
            let wtext = render(&mut wp, 80, 5);
            assert_eq!(wtext.matches("TASK_").count(), 3);
            let active_row = if active == 0 {
                2
            } else if active == 5 {
                4
            } else {
                3
            };
            assert!(
                wtext
                    .lines()
                    .nth(active_row)
                    .unwrap()
                    .contains(&format!("TASK_{active}"))
            );
            for task in &mut snapshot.phases[0].tasks {
                task.status = TodoStatus::Completed;
            }
            wp.update(snapshot.clone());
            let done_text = render(&mut wp, 80, 5);
            assert_eq!(done_text.matches("[\u{f00c}]").count(), 3);
            for i in 3..6 {
                assert!(done_text.contains(&format!("TASK_{i}")));
            }
            wp.toggle();
            snapshot.phases[0].name = "完成阶段".into();
            wp.update(snapshot);
            assert!(wp.expanded);
        }
    }

    #[test]
    fn tui_todo_expanded_centers_tree_and_scroll_clamp() {
        let mut p1 = Panel::default();
        let mut s1 = state(&[]);
        s1.phases[0].tasks = (0..12)
            .map(|i| TodoItem {
                content: format!("TASK_{i:02}"),
                status: if i == 6 {
                    TodoStatus::InProgress
                } else {
                    TodoStatus::Pending
                },
                blocker: None,
            })
            .collect();
        p1.update(s1);
        p1.toggle();
        let h1 = p1.height(80, 30, 20);
        let t1 = render(&mut p1, 80, h1);
        assert!(t1.lines().nth(5).unwrap().contains("TASK_06"));
        p1.scroll(true, usize::MAX);
        assert!(render(&mut p1, 80, h1).contains("I. 验证阶段") && p1.scroll == 0);

        for expanded in [false, true] {
            for width in [4, 12, 80] {
                let mut pt = Panel::default();
                pt.update(state(&[
                    ("当前任务包含很长的中文内容", TodoStatus::InProgress),
                    ("下一任务", TodoStatus::Pending),
                ]));
                pt.expanded = expanded;
                let h = pt.height(width, 24, 20);
                let txt = render(&mut pt, width, h);
                assert!(!txt.lines().any(|l| l.trim_end() == "└────") && !txt.contains('╭'));
                assert!(pt.rows.iter().all(|row| row.width() <= usize::from(width)));
            }
        }
        let mut p_phases = Panel::default();
        let mut sp = state(&[
            ("FIRST", TodoStatus::Completed),
            ("SECOND", TodoStatus::Pending),
        ]);
        sp.phases.push(TodoPhase {
            name: "Next phase".into(),
            tasks: vec![TodoItem {
                content: "THIRD".into(),
                status: TodoStatus::Pending,
                blocker: None,
            }],
        });
        p_phases.update(sp);
        p_phases.toggle();
        let tp = render(&mut p_phases, 80, 8);
        let lp: Vec<_> = tp.lines().map(str::trim_end).collect();
        assert_eq!(
            &lp[1..6],
            &[
                "├─ I. 验证阶段 (1/2)",
                "│  ├─ [\u{f00c}] FIRST",
                "│  └─ [ ] SECOND",
                "├─ II. Next phase (0/1)",
                "│  └─ [ ] THIRD"
            ]
        );

        let mut p_clamp = Panel::default();
        p_clamp.update(state(
            &(0..30)
                .map(|_| ("待办", TodoStatus::Pending))
                .collect::<Vec<_>>(),
        ));
        p_clamp.toggle();
        assert_eq!(p_clamp.height(80, 24, 20), 8);
        render(&mut p_clamp, 80, 8);
        p_clamp.scroll(false, usize::MAX);
        assert!(p_clamp.scroll > 0);
        render(&mut p_clamp, 20, 8);
        assert!(p_clamp.scroll <= p_clamp.rows.len() - 7);
        p_clamp.update(state(&[("最后任务", TodoStatus::Pending)]));
        assert!(render(&mut p_clamp, 80, 8).contains("最后任务") && p_clamp.scroll == 0);
        p_clamp.reset_view();
        assert!(!p_clamp.expanded);
    }

    #[test]
    fn tui_todo_states_tiny_heights_and_toggle_hint() {
        let mut panel = Panel::default();
        assert_eq!(panel.height(80, 24, 20), 0);
        panel.update(state(&[("完成", TodoStatus::InProgress)]));
        panel.toggle();
        panel.update(state(&[("完成", TodoStatus::Completed)]));
        assert!(panel.expanded && panel.height(80, 24, 20) == 3);
        panel.toggle();
        assert!(render(&mut panel, 80, 2).contains("[\u{f00c}] 完成"));
        assert!(render(&mut panel, 80, 1).contains("\u{f00c} TODO · 1/1 · Complete"));
        panel.update(state(&[("放弃", TodoStatus::Abandoned)]));
        let abandoned = render(&mut panel, 80, 2);
        assert!(abandoned.contains("Closed") && !abandoned.contains("Complete"));
        panel.update(state(&[("待办", TodoStatus::Pending)]));
        assert!(
            panel.height(80, 24, 2) == 2
                && panel.height(80, 24, 0) == 0
                && panel.height(0, 24, 20) == 0
        );
        panel.toggle();
        assert_eq!(panel.height(80, 6, 5), 2);
        panel.update(TodoState::default());
        assert!(!panel.expanded && panel.height(80, 24, 20) == 0);

        let mut pt = Panel::default();
        pt.update(state(&[("任务", TodoStatus::Pending)]));
        for width in [16, 24, 80] {
            for expanded in [false, true] {
                pt.expanded = expanded;
                let txt = render(&mut pt, width, 4);
                let title = txt.lines().next().unwrap();
                assert!(title.starts_with("TODO") && title.contains(TOGGLE_HINT));
                if width == 80 {
                    let action = if expanded { "collapse" } else { "expand" };
                    assert_eq!(
                        title.trim_end(),
                        format!("TODO · 0/1 · {TOGGLE_HINT} {action}")
                    );
                }
            }
        }
    }

    #[test]
    fn tui_todo_colors_wrapping_preview_and_sanitization() {
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
        let b1 = terminal.backend().buffer();
        assert_eq!(
            (b1[(3, 1)].symbol(), b1[(7, 2)].fg, b1[(6, 3)].fg),
            ("I", palette::todo_active(), palette::muted())
        );
        assert!(b1[(10, 2)].modifier.contains(Modifier::BOLD));
        panel.toggle();
        terminal
            .draw(|frame| panel.draw(frame, frame.area()))
            .unwrap();
        let b2 = terminal.backend().buffer();
        for (y, symbol, marker, text) in [
            (2, " ", palette::todo_active(), palette::todo_active()),
            (3, " ", palette::muted(), palette::muted()),
            (4, "!", palette::accent(), palette::accent()),
            (5, "\u{f00c}", palette::added(), palette::added()),
        ] {
            assert_eq!(
                (b2[(7, y)].symbol(), b2[(6, y)].fg, b2[(10, y)].fg),
                (symbol, marker, text)
            );
        }
        assert!(!b2[(10, 5)].modifier.contains(Modifier::CROSSED_OUT));

        let mut p_wrap = Panel::default();
        p_wrap.update(state(&[
            ("abcdefghijklmnopqrstuv\nEXPLICIT", TodoStatus::Pending),
            ("尾项", TodoStatus::Pending),
        ]));
        p_wrap.toggle();
        render(&mut p_wrap, 16, 8);
        assert!(
            p_wrap
                .rows
                .iter()
                .any(|r| r.to_string() == "│  │      EXPLIC")
        );
        assert!(
            p_wrap
                .rows
                .iter()
                .any(|r| r.to_string() == "│  └─ [ ] 尾项")
        );

        let mut p_prev = Panel::default();
        let mut sp = state(&[("计划任务", TodoStatus::Pending)]);
        sp.plan = Some(pp_core::events::TodoPlanState {
            id: "plan-1".into(),
            revision: 1,
            status: PlanStatus::Draft,
        });
        p_prev.update(sp);
        let t_prev = render(&mut p_prev, 80, 3);
        assert!(t_prev.contains("Preview") && !t_prev.contains('→'));

        let mut p_safe = Panel::default();
        p_safe.update(state(&[
            ("读取\u{1b}[31m状态\u{7f}\t列", TodoStatus::Completed),
            ("很长的中文任务名称必须完整换行展示", TodoStatus::Blocked),
        ]));
        p_safe.toggle();
        p_safe.height(12, 90, 30);
        let h_safe = p_safe.rows.len() as u16 + 1;
        let t_safe = render(&mut p_safe, 12, h_safe);
        let joined = t_safe.replace(['\n', ' ', '│'], "");
        assert!(
            joined.contains("很长的中文任务名称必须完整换行展示")
                && !t_safe.contains('\u{1b}')
                && !t_safe.contains('\u{7f}')
        );
    }

    #[test]
    fn tui_todo_parallel_in_progress_and_duplicate_checklist() {
        let mut p1 = Panel::default();
        p1.update(state(&[
            ("A", TodoStatus::InProgress),
            ("B", TodoStatus::InProgress),
            ("C", TodoStatus::InProgress),
            ("D", TodoStatus::InProgress),
            ("E", TodoStatus::Pending),
        ]));
        p1.toggle();
        let t1 = render(&mut p1, 100, 24);
        assert_eq!(t1.matches("(in progress)").count(), 4);
        assert!(!t1.contains("E (in progress)"));

        let mut p2 = Panel::default();
        p2.update(state(&[
            ("重复任务", TodoStatus::InProgress),
            ("重复任务", TodoStatus::Pending),
            ("已放弃", TodoStatus::Abandoned),
            ("已完成", TodoStatus::Completed),
        ]));
        p2.toggle();
        let t2 = render(&mut p2, 100, 10);
        assert!(t2.contains("phase \"验证阶段\" occurrence 1"));
        assert!(t2.contains("phase \"验证阶段\" occurrence 2"));
        assert!(t2.contains("已放弃 (dropped)") && t2.contains("[\u{f00c}] 已完成"));
    }
}
