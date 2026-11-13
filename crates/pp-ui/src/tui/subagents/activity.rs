//! Activity 页：分类／范围过滤后的行选择与绘制。

use super::inspector::status_icon;
use super::*;

impl Panel {
    pub(super) fn activity_rows(&self) -> Vec<usize> {
        let anchor = self.selected_agent().map(|agent| agent.id.clone());
        self.projection.activity(
            self.activity.category,
            self.activity.scope,
            anchor.as_deref(),
            &self.activity.query,
        )
    }

    /// 当前可见列表中的选中位置；跟随模式下最后一行就是选中项。
    pub(super) fn activity_index(&self, rows: &[usize]) -> Option<usize> {
        if self.activity.selected.is_none() {
            return rows.len().checked_sub(1);
        }
        let selected = self.activity.selected.as_ref()?;
        rows.iter()
            .position(|index| self.projection.rows()[*index].id() == *selected)
            .or_else(|| rows.len().checked_sub(1))
    }

    pub(super) fn activity_step(&mut self, delta: isize) {
        let rows = self.activity_rows();
        if rows.is_empty() {
            return;
        }
        let current = self.activity_index(&rows).unwrap_or(rows.len() - 1) as isize;
        let next = (current + delta).clamp(0, rows.len() as isize - 1) as usize;
        self.activity.selected = Some(self.projection.rows()[rows[next]].id());
        self.activity.follow = false;
        self.activity.offset = next;
    }

    pub(super) fn draw_activity(&mut self, frame: &mut Frame, area: Rect) {
        if area.height == 0 {
            return;
        }
        let header = Rect::new(area.x, area.y, area.width, 1);
        let body = Rect::new(
            area.x,
            area.y + 1,
            area.width,
            area.height.saturating_sub(1),
        );
        let search = if self.activity.editing {
            format!("/{}", self.activity.query)
        } else if self.activity.query.is_empty() {
            "/search".to_owned()
        } else {
            format!("/{}", self.activity.query)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    self.activity.category.label().to_owned(),
                    Style::default().fg(ACCENT),
                ),
                Span::styled("  ·  ", Style::default().fg(DIM)),
                Span::styled(
                    self.activity.scope.label().to_owned(),
                    Style::default().fg(ACCENT),
                ),
                Span::styled("  ·  ", Style::default().fg(DIM)),
                Span::styled(
                    search,
                    Style::default().fg(if self.activity.editing { ACCENT } else { MUTED }),
                ),
                Span::styled("  ·  ", Style::default().fg(DIM)),
                Span::styled(
                    if self.activity.follow {
                        "follow latest"
                    } else {
                        "paused"
                    }
                    .to_owned(),
                    Style::default().fg(if self.activity.follow { ADDED } else { ACCENT }),
                ),
            ])),
            header,
        );
        let rows = self.activity_rows();
        if rows.is_empty() {
            let message = match self.activity.scope {
                Scope::Selected | Scope::Subtree if self.selected.is_none() => {
                    "No agent selected for this scope"
                }
                _ if !self.activity.query.is_empty() => "No matching activity",
                _ => "No activity yet",
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    message,
                    Style::default().fg(MUTED),
                ))),
                body,
            );
            return;
        }
        let heights = rows
            .iter()
            .map(|index| {
                let lines = self.activity_lines(*index);
                lines_height(&lines, body.width)
            })
            .collect::<Vec<u16>>();
        let selected = self.activity_index(&rows);
        let start = if self.activity.follow {
            tail_start(&heights, body.height)
        } else {
            let offset = self.activity.offset.min(rows.len().saturating_sub(1));
            visible_start(&heights, Some(offset), body.height)
        };
        let mut y = body.y;
        for (slot, index) in rows.iter().enumerate().skip(start) {
            let height = heights[slot];
            if y >= body.bottom() {
                break;
            }
            let height = height.min(body.bottom() - y);
            let rect = Rect::new(body.x, y, body.width, height);
            let is_selected = selected == Some(slot);
            let lines = style_row(self.activity_lines(*index), is_selected);
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), rect);
            self.activity_hits.push((rect, *index));
            y += height;
        }
    }

    fn activity_lines(&self, index: usize) -> Vec<Line<'static>> {
        let row: &ActivityRow = &self.projection.rows()[index];
        let name = self
            .projection
            .agent(&row.agent)
            .map(|agent| agent.name.clone())
            .unwrap_or_else(|| row.agent.clone());
        let category = match row.kind {
            Kind::Response => "response",
            Kind::Tool => "tool",
            Kind::Lifecycle => "lifecycle",
        };
        vec![Line::from(vec![
            Span::styled(
                format!("{} ", detail::format_clock(row.timestamp)),
                Style::default().fg(DIM),
            ),
            Span::styled(
                format!("{} ", activity_icon(row.status, self.clock)),
                Style::default().fg(row_color(row.status)),
            ),
            Span::styled(name, Style::default().fg(ACCENT)),
            Span::styled(
                format!(" {category} · {}", row.title),
                Style::default().fg(ACTIVITY),
            ),
            Span::styled(
                format!("  {}", summary_line(&row.summary)),
                Style::default().fg(MUTED),
            ),
        ])]
    }
}

pub(super) fn activity_icon(status: RowStatus, clock: u64) -> &'static str {
    match status {
        RowStatus::Pending => status_icon(AgentStatus::Running, clock),
        RowStatus::Success => "\u{f00c}",
        RowStatus::Error => "\u{f00d}",
        RowStatus::Aborted => "■",
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use pp_core::events::AgentEvent;

    #[test]
    fn activity_paused_follow_keeps_viewport_and_escape_clears_search() {
        let mut drafts = Drafts::default();
        let mut panel = panel(vec![record("a1", "Main", "idle")], vec![]);
        for index in 0..6u64 {
            panel.apply(Reply::Activity {
                epoch: 1,
                agent: "a1".into(),
                page: pp_runtime::subagent::EventPage {
                    entries: vec![entry(index * 100, "r1", AgentEvent::AssistantStarted)],
                    cursor: None,
                    reset: false,
                    has_more: false,
                    warnings: vec![],
                },
            });
        }
        panel.handle(key(KeyCode::Char('2')), &mut drafts);
        // 默认跟随最新：最后一行的选中 id 就是末行。
        let rows = panel.activity_rows();
        let last = panel.projection.rows()[*rows.last().unwrap()].id();
        assert!(panel.activity.follow);
        // 手动上移暂停跟随并固定 id。
        panel.handle(key(KeyCode::Char('k')), &mut drafts);
        assert!(!panel.activity.follow);
        let pinned = panel.activity.selected.clone().unwrap();
        assert_ne!(pinned, last);
        for index in 6..12u64 {
            panel.apply(Reply::Activity {
                epoch: 1,
                agent: "a1".into(),
                page: pp_runtime::subagent::EventPage {
                    entries: vec![entry(index * 100, "r1", AgentEvent::AssistantStarted)],
                    cursor: None,
                    reset: false,
                    has_more: false,
                    warnings: vec![],
                },
            });
        }
        assert_eq!(panel.activity.selected.as_deref(), Some(pinned.as_str()));
        // Space 恢复跟随并跳最新。
        panel.handle(key(KeyCode::Char(' ')), &mut drafts);
        assert!(panel.activity.follow);
        // 搜索编辑优先于普通字符快捷键，Esc 先清空查询。
        panel.handle(key(KeyCode::Char('/')), &mut drafts);
        for character in "retry".chars() {
            panel.handle(key(KeyCode::Char(character)), &mut drafts);
        }
        assert_eq!(panel.activity.query, "retry");
        panel.handle(key(KeyCode::Esc), &mut drafts);
        assert!(panel.activity.query.is_empty());
        assert!(!panel.activity.editing);
        assert_eq!(panel.page, Page::Activity);
        // 再按 Esc 回到 Agents 页。
        panel.handle(key(KeyCode::Esc), &mut drafts);
        assert_eq!(panel.page, Page::Agents);
    }
}
