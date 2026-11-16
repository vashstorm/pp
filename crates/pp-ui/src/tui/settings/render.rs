//! 共享顶部标签栏绘制／命中测试与 `General` 页双栏渲染。
use super::*;
use crate::tui::themes::palette;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

struct TabSlot {
    tab: Tab,
    text: String,
    start_col: u16,
    end_col: u16,
}

pub(in crate::tui) fn sidebar_width(width: u16) -> u16 {
    if width >= 54 {
        (width / 4).clamp(20, 26)
    } else {
        0
    }
}

fn tab_layout(active: Tab, suffix: Option<&str>, area: Rect) -> (Vec<TabSlot>, &'static str, bool) {
    let avail = usize::from(area.width.saturating_sub(2));
    let full_slots: Vec<_> = Tab::ALL
        .into_iter()
        .map(|tab| {
            if tab == active {
                format!("[{}]", tab.label())
            } else {
                format!(" {} ", tab.label())
            }
        })
        .collect();
    // " " + slots joined by " · " + optional " · <suffix>" + " "
    let suffix_len = suffix.map_or(0, |s| 3 + s.chars().count());
    let full_width =
        2 + full_slots.iter().map(|s| s.chars().count()).sum::<usize>() + (Tab::ALL.len() - 1) * 3;
    if avail >= full_width + suffix_len {
        return build_slots(full_slots, " · ", area.x, active);
    }
    let compact_slots: Vec<_> = Tab::ALL
        .into_iter()
        .map(|tab| {
            if tab == active {
                format!("[{}]", tab.label())
            } else {
                format!(" {} ", tab.label())
            }
        })
        .collect();
    let compact_width = 2
        + compact_slots
            .iter()
            .map(|s| s.chars().count())
            .sum::<usize>()
        + (Tab::ALL.len() - 1);
    if avail >= compact_width + suffix_len || avail >= compact_width {
        return build_slots(compact_slots, " ", area.x, active);
    }
    let tight_slots: Vec<_> = Tab::ALL
        .into_iter()
        .map(|tab| tab.label().to_owned())
        .collect();
    let tight_width =
        2 + tight_slots.iter().map(|s| s.chars().count()).sum::<usize>() + (Tab::ALL.len() - 1);
    if avail >= tight_width {
        return build_slots(tight_slots, " ", area.x, active);
    }
    // 窄终端仅渲染当前激活标签名，确保在极端小屏下仍不溢出。
    let single = vec![active.label().to_owned()];
    (
        vec![TabSlot {
            tab: active,
            start_col: area.x + 2,
            end_col: area.x + 2 + active.label().chars().count() as u16,
            text: single.into_iter().next().unwrap_or_default(),
        }],
        "",
        false,
    )
}

fn build_slots(
    texts: Vec<String>,
    separator: &'static str,
    area_x: u16,
    _active: Tab,
) -> (Vec<TabSlot>, &'static str, bool) {
    let sep_len = separator.chars().count() as u16;
    let mut cursor = area_x + 2;
    let mut slots = Vec::with_capacity(texts.len());
    for (idx, (tab, text)) in Tab::ALL.into_iter().zip(texts).enumerate() {
        if idx > 0 {
            cursor += sep_len;
        }
        let w = text.chars().count() as u16;
        slots.push(TabSlot {
            tab,
            text,
            start_col: cursor,
            end_col: cursor + w,
        });
        cursor += w;
    }
    (slots, separator, true)
}

pub(in crate::tui) fn tabs_title(
    active: Tab,
    suffix: Option<&str>,
    area: Rect,
    accent: Color,
    text: Color,
    muted: Color,
) -> Line<'static> {
    let (_slots, separator, _show_suffix_check) = tab_layout(active, suffix, area);
    let slots = _slots;
    let mut spans = vec![Span::raw(" ")];
    for (idx, slot) in slots.iter().enumerate() {
        if idx > 0 {
            spans.push(Span::styled(separator, Style::default().fg(muted)));
        }
        let style = if slot.tab == active {
            Style::default().fg(accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(text)
        };
        spans.push(Span::styled(slot.text.clone(), style));
    }
    if let Some(suffix) = suffix {
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let avail = usize::from(area.width.saturating_sub(2));
        let extra = 3 + suffix.chars().count() + 1;
        if avail >= used + extra {
            spans.push(Span::styled(" · ", Style::default().fg(muted)));
            spans.push(Span::styled(suffix.to_owned(), Style::default().fg(text)));
        }
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

pub(in crate::tui) fn tab_at_column(
    active: Tab,
    suffix: Option<&str>,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<Tab> {
    if row != area.y || area.width < 4 || area.height < 3 {
        return None;
    }
    let (slots, _, multi) = tab_layout(active, suffix, area);
    if !multi {
        return None;
    }
    slots
        .into_iter()
        .find(|slot| column >= slot.start_col && column < slot.end_col)
        .map(|slot| slot.tab)
}

impl Panel {
    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        if area.is_empty() {
            return;
        }
        let p = palette();
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::default().style(Style::default().bg(p.panel_bg).fg(p.text)),
            area,
        );
        if area.width < 10 || area.height < 6 {
            return;
        }
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.panel_border))
            .title(tabs_title(
                Tab::General,
                None,
                area,
                p.accent,
                p.text,
                p.muted,
            ));
        let inner = outer.inner(area);
        frame.render_widget(outer, area);
        if inner.width < 6 || inner.height < 4 {
            return;
        }

        let (body, detail_area, footer_area) = if inner.height >= 8 {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2),
                    Constraint::Min(3),
                    Constraint::Length(1),
                    Constraint::Length(2),
                    Constraint::Length(1),
                ])
                .split(inner);
            frame.render_widget(
                Line::styled(
                    "General — -/+ or Space adjust · Enter edit numeric values",
                    Style::default().fg(p.muted),
                ),
                Rect::new(sections[0].x, sections[0].y, sections[0].width, 1),
            );
            frame.render_widget(
                Line::styled(
                    "─".repeat(usize::from(sections[0].width)),
                    Style::default().fg(p.panel_border),
                ),
                Rect::new(sections[0].x, sections[0].y + 1, sections[0].width, 1),
            );
            frame.render_widget(
                Line::styled(
                    "─".repeat(usize::from(sections[2].width)),
                    Style::default().fg(p.panel_border),
                ),
                sections[2],
            );
            (sections[1], sections[3], sections[4])
        } else {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),
                    Constraint::Length(2),
                    Constraint::Length(1),
                ])
                .split(inner);
            (sections[0], sections[1], sections[2])
        };

        let sidebar_width = sidebar_width(body.width);
        if sidebar_width > 0 {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(sidebar_width),
                    Constraint::Length(1),
                    Constraint::Min(10),
                ])
                .split(body);
            self.sidebar_area = cols[0];
            self.list_area = cols[2];
            self.draw_sidebar(frame, cols[0], p);
            frame.render_widget(
                Paragraph::new(vec![Line::raw("│"); usize::from(cols[1].height)])
                    .style(Style::default().fg(p.panel_border)),
                cols[1],
            );
            self.draw_items(frame, cols[2], p);
        } else {
            self.sidebar_area = Rect::default();
            let narrow = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(body);
            frame.render_widget(
                Line::styled(
                    format!(" Category: {} (Tab next)", self.selected_category().label()),
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                narrow[0],
            );
            self.list_area = narrow[1];
            self.draw_items(frame, narrow[1], p);
        }

        self.draw_detail(frame, detail_area, p);
        self.draw_footer(frame, footer_area, p);
    }

    fn draw_sidebar(&self, frame: &mut Frame, area: Rect, p: &crate::tui::themes::Palette) {
        let mut lines = Vec::with_capacity(Category::ALL.len());
        for (idx, category) in Category::ALL.iter().enumerate() {
            let selected = idx == self.category_index;
            let prefix = if selected { "❯ " } else { "  " };
            let style = if selected && self.sidebar_focus {
                Style::default()
                    .bg(p.panel_selected_bg)
                    .fg(p.panel_selected_fg)
                    .add_modifier(Modifier::BOLD)
            } else if selected {
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.text)
            };
            lines.push(Line::styled(format!("{prefix}{}", category.label()), style));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_items(&mut self, frame: &mut Frame, area: Rect, p: &crate::tui::themes::Palette) {
        let Some(settings) = &self.settings else {
            let msg = if self.busy {
                "Loading settings.yml..."
            } else {
                "Failed to load settings.yml"
            };
            frame.render_widget(
                Paragraph::new(Line::styled(msg, Style::default().fg(p.muted))),
                area,
            );
            return;
        };
        let items = self.category_items();
        if self.item_index >= items.len() {
            self.item_index = items.len().saturating_sub(1);
        }
        let visible = usize::from(area.height).max(1);
        if self.item_index < self.item_scroll {
            self.item_scroll = self.item_index;
        } else if self.item_index >= self.item_scroll + visible {
            self.item_scroll = self.item_index + 1 - visible;
        }

        let label_width = items
            .iter()
            .map(|item| item.label.chars().count())
            .max()
            .unwrap_or(18)
            .min(usize::from(area.width).saturating_sub(12));

        let mut lines = Vec::new();
        for (idx, item) in items
            .iter()
            .enumerate()
            .skip(self.item_scroll)
            .take(visible)
        {
            let selected = idx == self.item_index && !self.sidebar_focus;
            let value_text = if selected && let Some(edit) = &self.editing {
                format!("[{}|]", edit.buffer)
            } else {
                item.display_value(settings)
            };
            let prefix = if selected { "❯ " } else { "  " };
            let row_style = if selected {
                Style::default()
                    .bg(p.panel_selected_bg)
                    .fg(p.panel_selected_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.text)
            };
            let val_style = if selected {
                row_style
            } else {
                Style::default().fg(p.accent)
            };
            let padded_label = format!("{:<width$}", item.label, width = label_width);
            lines.push(Line::from(vec![
                Span::styled(prefix, row_style),
                Span::styled(padded_label, row_style),
                Span::styled("  ", row_style),
                Span::styled(value_text, val_style),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_detail(&self, frame: &mut Frame, area: Rect, p: &crate::tui::themes::Palette) {
        let mut lines = Vec::new();
        if let Some(item) = self.selected_item() {
            lines.push(Line::from(vec![
                Span::styled(
                    item.key,
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" — ", Style::default().fg(p.muted)),
                Span::styled(item.description, Style::default().fg(p.text)),
            ]));
        }
        lines.push(Line::styled(
            format!("File: {}", self.settings_display_path),
            Style::default().fg(p.muted),
        ));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, p: &crate::tui::themes::Palette) {
        let line = if let Some(error) = &self.error {
            Line::styled(error.as_str(), Style::default().fg(p.removed))
        } else if let Some(status) = &self.status {
            Line::styled(status.as_str(), Style::default().fg(p.added))
        } else if self.editing.is_some() {
            Line::styled(
                "Type value · Enter save · Esc cancel edit",
                Style::default().fg(p.muted),
            )
        } else {
            Line::styled(
                "←/→ tabs · Tab focus · ↑↓ move · -/+/Space adjust · Enter edit · r reload · Esc close",
                Style::default().fg(p.muted),
            )
        };
        frame.render_widget(line, area);
    }
}
