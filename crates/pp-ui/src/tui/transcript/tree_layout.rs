use super::{
    Frame, Line, Rect, Span, Style, Transcript, TreeBlock, TreeGroup, TreeLayout, Widget, palette,
};

impl Transcript {
    pub(in crate::tui) fn draw_tree(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        blocks: &[TreeBlock],
        visible: &[usize],
        selected: Option<usize>,
        reveal: bool,
    ) {
        self.viewport_height = usize::from(area.height);
        // 树视图的边框与分组几何独立于普通历史浏览。
        self.browse_anchor = None;
        self.displayed_heights.clear();
        if area.is_empty() {
            return;
        }
        let framed = area.width >= 3;
        let inset = u16::from(framed);
        let width = area.width.saturating_sub(2 * inset);
        let cached = self.tree_layout.as_ref().is_some_and(|layout| {
            layout.width == area.width
                && layout.groups.len() == visible.len()
                && layout.groups.iter().zip(visible).all(|(group, &index)| {
                    group.index == index && group.entries == blocks[index].entries
                })
        });
        if !cached {
            self.layout_entries(width, !framed);
            let mut total = 0usize;
            let groups = visible
                .iter()
                .map(|&index| {
                    let entries = blocks[index].entries.clone();
                    let body = self.entries[entries.clone()]
                        .iter()
                        .map(|entry| {
                            let height = entry.layout.as_ref().expect("layout initialized").height;
                            if height == 0 { 0 } else { height + 1 }
                        })
                        .sum::<usize>()
                        .saturating_sub(1);
                    let rows = total..total + body + 2;
                    total = rows.end + 1;
                    TreeGroup {
                        index,
                        entries,
                        rows,
                    }
                })
                .collect();
            self.total = total.saturating_sub(1);
            self.tree_layout = Some(TreeLayout {
                width: area.width,
                groups,
            });
        }
        let layout = self.tree_layout.as_ref().expect("tree layout initialized");
        let bottom = self.total.saturating_sub(self.viewport_height);
        self.offset = self.offset.min(bottom);
        if reveal
            && let Some(group) = layout
                .groups
                .iter()
                .find(|group| Some(group.index) == selected)
        {
            if group.rows.len() > self.viewport_height || group.rows.start < self.offset {
                self.offset = group.rows.start;
            } else if group.rows.end > self.offset + self.viewport_height {
                self.offset = group.rows.end.saturating_sub(self.viewport_height);
            }
            self.offset = self.offset.min(bottom);
        }
        self.follow_bottom = false;
        if visible.is_empty() {
            frame.render_widget(
                Line::styled(
                    "Entries hidden by the current filter",
                    Style::default().fg(palette::MUTED),
                ),
                Rect::new(area.x, area.y, area.width, 1),
            );
            return;
        }
        let viewport_end = self.offset + self.viewport_height;
        let buffer = frame.buffer_mut();
        for group in &layout.groups {
            let start = group.rows.start.max(self.offset);
            let end = group.rows.end.min(viewport_end);
            if start >= end {
                continue;
            }
            let chosen = framed && Some(group.index) == selected;
            let style = Style::default().fg(palette::ACCENT);
            if chosen {
                for row in start..end {
                    let y = area.y + (row - self.offset) as u16;
                    Span::styled("╎", style).render(Rect::new(area.x, y, 1, 1), buffer);
                    Span::styled("╎", style).render(Rect::new(area.right() - 1, y, 1, 1), buffer);
                }
            }
            for (row, left, right) in [(group.rows.start, "╭", "╮"), (group.rows.end - 1, "╰", "╯")]
            {
                if row < start || row >= end {
                    continue;
                }
                let y = area.y + (row - self.offset) as u16;
                if chosen {
                    for x in area.x + 1..area.right() - 1 {
                        Span::styled("╌", style).render(Rect::new(x, y, 1, 1), buffer);
                    }
                    Span::styled(left, style).render(Rect::new(area.x, y, 1, 1), buffer);
                    Span::styled(right, style).render(Rect::new(area.right() - 1, y, 1, 1), buffer);
                }
                if row == group.rows.start {
                    (&blocks[group.index].label)
                        .render(Rect::new(area.x + inset, y, width, 1), buffer);
                }
            }
            let body_start = (group.rows.start + 1).max(start);
            let body_end = (group.rows.end - 1).min(end);
            if body_start < body_end {
                Self::render_entries(
                    self.entries[group.entries.clone()].iter(),
                    buffer,
                    Rect::new(
                        area.x + inset,
                        area.y + (body_start - self.offset) as u16,
                        width,
                        (body_end - body_start) as u16,
                    ),
                    body_start - group.rows.start - 1,
                    !framed,
                );
            }
        }
    }
}
