use super::*;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

impl Panel {
    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        self.sidebar_area = Rect::default();
        self.list_area = Rect::default();
        if area.is_empty() {
            return;
        }
        let (selected_theme, selected_icons) = self.preview_appearance();
        let p = palette_for(selected_theme);
        let ascii = selected_icons == IconMode::Ascii;

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
            .title(super::super::settings::render::tabs_title(
                super::super::settings::Tab::Themes,
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

        let (body, footer_area) = if inner.height >= 8 {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(2),
                    Constraint::Min(1),
                    Constraint::Length(1),
                ])
                .split(inner);
            let header = format!(
                "Themes — {} · saved: {}",
                self.category_label(),
                match self.category {
                    Category::IconMode => self.saved_icons.label(),
                    Category::Theme => self.saved_theme.label(),
                }
            );
            frame.render_widget(
                Line::styled(header, Style::default().fg(p.muted)),
                Rect::new(sections[0].x, sections[0].y, sections[0].width, 1),
            );
            frame.render_widget(
                Line::styled(
                    "─".repeat(usize::from(sections[0].width)),
                    Style::default().fg(p.panel_border),
                ),
                Rect::new(sections[0].x, sections[0].y + 1, sections[0].width, 1),
            );
            (sections[1], sections[2])
        } else {
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(1), Constraint::Length(1)])
                .split(inner);
            (sections[0], sections[1])
        };

        let sidebar_width = super::super::settings::render::sidebar_width(body.width);
        if sidebar_width == 0 {
            self.sidebar_focus = false;
        }
        let right_area = if sidebar_width > 0 {
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(sidebar_width),
                    Constraint::Length(1),
                    Constraint::Min(10),
                ])
                .split(body);
            self.sidebar_area = cols[0];
            self.draw_sidebar(frame, cols[0], p, selected_icons, ascii);
            frame.render_widget(
                Paragraph::new(vec![Line::from("│"); usize::from(cols[1].height)])
                    .style(Style::default().fg(p.panel_border)),
                cols[1],
            );
            cols[2]
        } else {
            let narrow = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(body);
            frame.render_widget(
                Line::styled(
                    format!(" Category: {} (Tab switch)", self.category_label()),
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                narrow[0],
            );
            narrow[1]
        };

        self.draw_right(frame, right_area, p, selected_theme, selected_icons, ascii);
        self.draw_footer(frame, footer_area, p);
    }

    fn draw_sidebar(
        &self,
        frame: &mut Frame,
        area: Rect,
        p: &Palette,
        selected_icons: IconMode,
        ascii: bool,
    ) {
        if area.is_empty() {
            return;
        }
        let items = [
            (
                self.category == Category::IconMode,
                if ascii { "I" } else { "\u{f0279}" },
                "Icon Style",
                selected_icons.as_str().to_owned(),
            ),
            (
                self.category == Category::Theme,
                if ascii { "T" } else { "\u{f09a9}" },
                "Color Theme",
                Theme::ALL.len().to_string(),
            ),
        ];
        let mut lines = Vec::with_capacity(items.len());
        for (active, icon, name, count) in items {
            let label_style = if active {
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.text)
            };
            let used = Line::from(format!("  {icon} {name}{count}")).width();
            let gap = usize::from(area.width.saturating_sub(1))
                .saturating_sub(used)
                .max(1);
            lines.push(Line::from(vec![
                Span::styled(
                    if active && self.sidebar_focus {
                        "❯ "
                    } else {
                        "  "
                    },
                    label_style,
                ),
                Span::styled(format!("{icon} "), label_style),
                Span::styled(name, label_style),
                Span::styled(
                    format!("{}{count}", " ".repeat(gap)),
                    Style::default().fg(p.muted),
                ),
            ]));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn draw_right(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        p: &Palette,
        selected_theme: Theme,
        selected_icons: IconMode,
        ascii: bool,
    ) {
        if area.is_empty() {
            return;
        }
        // 按两个分类的最大列表行数固定上方列表槽位，避免在 Icon Style 与 Color Theme 间切换时预览区上下跳动。
        let max_list_rows = IconMode::ALL.len().max(Theme::ALL.len()) as u16;
        if area.height >= 12 {
            let list_height = max_list_rows.min(area.height.saturating_sub(6).max(2));
            let spacer = if area.height >= list_height + 19 {
                3
            } else if area.height >= list_height + 11 {
                2
            } else if area.height > list_height + 8 {
                1
            } else {
                0
            };
            let list_area = Rect::new(area.x, area.y, area.width, list_height);
            let preview_y = area.y + list_height + spacer;
            let preview_area = Rect::new(
                area.x,
                preview_y,
                area.width,
                area.bottom().saturating_sub(preview_y),
            );
            self.draw_list(frame, list_area, p, ascii);
            super::preview::draw(
                frame,
                preview_area,
                p,
                selected_theme,
                selected_icons,
                ascii,
            );
        } else {
            self.draw_list(frame, area, p, ascii);
        }
    }

    fn draw_list(&mut self, frame: &mut Frame, area: Rect, p: &Palette, ascii: bool) {
        self.list_area = area;
        if area.is_empty() {
            return;
        }
        let visible = usize::from(area.height).max(1);
        let selected_idx = self.selected_index();
        if selected_idx < self.list_scroll {
            self.list_scroll = selected_idx;
        } else if selected_idx >= self.list_scroll + visible {
            self.list_scroll = selected_idx + 1 - visible;
        }
        let saved_dot = if ascii { "*" } else { "\u{f0765}" };
        let other_dot = if ascii { "o" } else { "\u{f0766}" };

        match self.category {
            Category::IconMode => {
                for (row, (idx, mode)) in IconMode::ALL
                    .iter()
                    .copied()
                    .enumerate()
                    .skip(self.list_scroll)
                    .take(visible)
                    .enumerate()
                {
                    let selected = idx == self.icon_index && !self.sidebar_focus;
                    let is_saved = mode == self.saved_icons;
                    let mode_ascii = mode == IconMode::Ascii;
                    let row_bg = if selected { p.panel_alt_bg } else { p.panel_bg };
                    let marker = if selected { "❯ " } else { "  " };
                    let dot = if is_saved { saved_dot } else { other_dot };
                    let dot_color = if is_saved { p.added } else { p.muted };
                    let sample = if mode_ascii {
                        "> @ / * +"
                    } else {
                        "\u{f1d8} \u{ec19} \u{f07c} \u{f00c}"
                    };
                    let row_rect = Rect::new(area.x, area.y + row as u16, area.width, 1);
                    frame.render_widget(
                        Block::default().style(Style::default().bg(row_bg)),
                        row_rect,
                    );
                    let line = Line::from(vec![
                        Span::styled(
                            marker,
                            Style::default()
                                .fg(p.accent)
                                .bg(row_bg)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("{dot} "), Style::default().fg(dot_color).bg(row_bg)),
                        Span::styled(
                            format!("{:<10}", mode.label()),
                            Style::default()
                                .fg(if selected { p.accent } else { p.text })
                                .bg(row_bg)
                                .add_modifier(if selected {
                                    Modifier::BOLD
                                } else {
                                    Modifier::empty()
                                }),
                        ),
                        Span::styled(
                            format!(" ({})", mode.as_str()),
                            Style::default().fg(p.muted).bg(row_bg),
                        ),
                        Span::styled(
                            format!("  {sample}"),
                            Style::default().fg(p.status_model).bg(row_bg),
                        ),
                        Span::styled(
                            format!("  {}", mode.description()),
                            Style::default().fg(p.muted).bg(row_bg),
                        ),
                    ]);
                    frame.render_widget(line, row_rect);
                }
            }
            Category::Theme => {
                for (row, (idx, theme)) in Theme::ALL
                    .iter()
                    .copied()
                    .enumerate()
                    .skip(self.list_scroll)
                    .take(visible)
                    .enumerate()
                {
                    let selected = idx == self.theme_index && !self.sidebar_focus;
                    let is_saved = theme == self.saved_theme;
                    let row_bg = if selected { p.panel_alt_bg } else { p.panel_bg };
                    let marker = if selected { "❯ " } else { "  " };
                    let dot = if is_saved { saved_dot } else { other_dot };
                    let dot_color = if is_saved { p.added } else { p.muted };
                    let row_rect = Rect::new(area.x, area.y + row as u16, area.width, 1);
                    frame.render_widget(
                        Block::default().style(Style::default().bg(row_bg)),
                        row_rect,
                    );
                    let mut spans = vec![
                        Span::styled(
                            marker,
                            Style::default()
                                .fg(p.accent)
                                .bg(row_bg)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("{dot} "), Style::default().fg(dot_color).bg(row_bg)),
                        Span::styled(
                            format!("{:<16}", theme.label()),
                            Style::default()
                                .fg(if selected { p.accent } else { p.text })
                                .bg(row_bg)
                                .add_modifier(if selected {
                                    Modifier::BOLD
                                } else {
                                    Modifier::empty()
                                }),
                        ),
                        Span::styled(" ", Style::default().bg(row_bg)),
                    ];
                    for color in palette_for(theme).swatch() {
                        spans.push(Span::styled("██", Style::default().fg(color).bg(row_bg)));
                    }
                    spans.push(Span::styled(
                        format!("  {}", theme.description()),
                        Style::default().fg(p.muted).bg(row_bg),
                    ));
                    frame.render_widget(Line::from(spans), row_rect);
                }
            }
        }
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect, p: &Palette) {
        if area.is_empty() {
            return;
        }
        if let Some(error) = &self.error {
            frame.render_widget(
                Line::styled(error.as_str(), Style::default().fg(p.removed)),
                area,
            );
            return;
        }
        if self.busy {
            frame.render_widget(
                Line::styled(
                    match self.category {
                        Category::IconMode => "Saving icon style...",
                        Category::Theme => "Saving color theme...",
                    },
                    Style::default().fg(p.accent),
                ),
                area,
            );
            return;
        }
        let hint = if self.sidebar_focus {
            "←/→ tabs · ↑↓ category · Tab / Shift+Tab / Enter focus list · Esc cancel"
        } else if self.sidebar_area.width > 0 {
            "←/→ tabs · Tab / Shift+Tab focus · ↑↓ preview · Enter save · Esc cancel"
        } else {
            "←/→ tabs · Tab / Shift+Tab category · ↑↓ preview · Enter save · Esc cancel"
        };
        frame.render_widget(Line::styled(hint, Style::default().fg(p.muted)), area);
    }
}
