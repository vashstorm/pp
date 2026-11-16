use super::*;
use crate::tui::{statusline, transcript_style};
use pp_core::model::ThinkLevel;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

impl Panel {
    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        self.sidebar_area = Rect::default();
        self.list_area = Rect::default();
        if area.is_empty() {
            return;
        }
        let selected_theme = self.selected_theme();
        let selected_icons = self.selected_icons();
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
                "Themes — {} · saved: {} / {}",
                self.step_label(),
                self.saved_icons.label(),
                self.saved_theme.label()
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
                    format!(" Category: {} (Tab switch)", self.step_label()),
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
                self.step == Step::IconMode,
                if ascii { "I" } else { "\u{f0279}" },
                "Icon Style",
                selected_icons.as_str().to_owned(),
            ),
            (
                self.step == Step::Theme,
                if ascii { "T" } else { "\u{f09a9}" },
                "Color Theme",
                Theme::ALL.len().to_string(),
            ),
        ];
        let mut lines = Vec::with_capacity(items.len());
        for (active, icon, name, count) in items {
            let prefix = if active { "❯ " } else { "  " };
            let label_style = if active {
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(p.text)
            };
            let used = Line::from(format!("{prefix}{icon} {name}{count}")).width();
            let gap = usize::from(area.width.saturating_sub(1))
                .saturating_sub(used)
                .max(1);
            lines.push(Line::from(vec![
                Span::styled(prefix, Style::default().fg(p.accent)),
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
        let total = self.list_len() as u16;
        if area.height >= 12 {
            let list_height = total.min(area.height.saturating_sub(6).max(2));
            let spacer = u16::from(area.height > list_height + 8);
            let list_area = Rect::new(area.x, area.y, area.width, list_height);
            let preview_y = area.y + list_height + spacer;
            let preview_area = Rect::new(
                area.x,
                preview_y,
                area.width,
                area.bottom().saturating_sub(preview_y),
            );
            self.draw_list(frame, list_area, p, ascii);
            self.draw_preview(
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

        match self.step {
            Step::IconMode => {
                for (row, (idx, mode)) in IconMode::ALL
                    .iter()
                    .copied()
                    .enumerate()
                    .skip(self.list_scroll)
                    .take(visible)
                    .enumerate()
                {
                    let selected = idx == self.icon_index;
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
            Step::Theme => {
                for (row, (idx, theme)) in Theme::ALL
                    .iter()
                    .copied()
                    .enumerate()
                    .skip(self.list_scroll)
                    .take(visible)
                    .enumerate()
                {
                    let selected = idx == self.theme_index;
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

    fn draw_preview(
        &self,
        frame: &mut Frame,
        area: Rect,
        p: &Palette,
        selected_theme: Theme,
        selected_icons: IconMode,
        ascii: bool,
    ) {
        if area.width < 8 || area.height < 4 {
            return;
        }
        let title_icon = if ascii { "*" } else { "\u{f09a9}" };
        let preview_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.tool_border))
            .style(Style::default().bg(p.md_bg).fg(p.md_text))
            .title(Line::from(vec![
                Span::styled(
                    format!(" {title_icon} Live Preview: {} ", selected_theme.label()),
                    Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("({}) ", selected_icons.label()),
                    Style::default().fg(p.muted),
                ),
            ]));
        let inner = preview_block.inner(area);
        frame.render_widget(preview_block, area);
        if inner.width < 4 || inner.height < 2 {
            return;
        }

        let edit_icon = transcript_style::tool_icon_for("edit", selected_icons);
        let read_icon = transcript_style::tool_icon_for("read", selected_icons);
        let bash_icon = transcript_style::tool_icon_for("bash", selected_icons);
        let check_icon = transcript_style::done_icon(selected_icons);
        let app_icon = statusline::app_icon(selected_icons);
        let model_icon = statusline::model_icon(selected_icons);
        let dir_icon = statusline::directory_icon(selected_icons);
        let git_icon = statusline::git_icon(selected_icons);
        let think_high = transcript_style::think_icon_for(ThinkLevel::High, selected_icons);

        let spacious = inner.height >= 16;
        let mut lines = vec![
            // 1. 色卡
            Line::from(vec![
                Span::styled("Palette: ", Style::default().fg(p.muted)),
                Span::styled("██ Accent ", Style::default().fg(p.accent)),
                Span::styled("██ Code ", Style::default().fg(p.code_block)),
                Span::styled("██ Added ", Style::default().fg(p.added)),
                Span::styled("██ Removed ", Style::default().fg(p.removed)),
                Span::styled("██ Link", Style::default().fg(p.link)),
            ]),
        ];
        if spacious {
            lines.push(Line::default());
        }
        // 2. 用户消息
        lines.push(Line::from(vec![
            Span::styled(
                " You ",
                Style::default()
                    .fg(p.accent)
                    .bg(p.user_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " Switch theme and render pure ASCII icons cleanly ",
                Style::default().fg(p.text).bg(p.user_bg),
            ),
        ]));
        if spacious {
            lines.push(Line::default());
        }
        // 3. 助手 Markdown + 语法高亮代码块
        lines.extend([
            Line::from(vec![Span::styled(
                "# Assistant Preview",
                Style::default()
                    .fg(p.md_heading)
                    .add_modifier(Modifier::BOLD),
            )]),
            Line::from(vec![
                Span::styled("• ", Style::default().fg(p.md_bullet)),
                Span::styled(
                    "Bold highlight",
                    Style::default()
                        .fg(p.md_strong)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(", inline ", Style::default().fg(p.md_text)),
                Span::styled("`tui.theme`", Style::default().fg(p.md_code)),
                Span::styled(" & ", Style::default().fg(p.md_text)),
                Span::styled(
                    "https://pp.dev",
                    Style::default()
                        .fg(p.link)
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ]),
            Line::from(vec![
                Span::styled("  │ ", Style::default().fg(p.dim)),
                Span::styled("fn ", Style::default().fg(p.syntax_keyword)),
                Span::styled("apply_theme", Style::default().fg(p.syntax_function)),
                Span::styled("(mode: ", Style::default().fg(p.md_text)),
                Span::styled("IconMode", Style::default().fg(p.edit_type)),
                Span::styled(") -> ", Style::default().fg(p.md_text)),
                Span::styled("\"ok\"", Style::default().fg(p.edit_string)),
                Span::styled(" // preview", Style::default().fg(p.syntax_comment)),
            ]),
            Line::default(),
            // 4. 工具与 Diff 预览
            Line::from(vec![
                Span::styled(format!(" {check_icon} Read "), Style::default().fg(p.added)),
                Span::styled("· ", Style::default().fg(p.read_summary_meta)),
                Span::styled(
                    "crates/pp-config/src/config/theme.rs:1-42",
                    Style::default().fg(p.accent),
                ),
            ]),
            Line::from(vec![
                Span::styled(
                    format!(" {edit_icon} Edit: src/tui/themes.rs "),
                    Style::default().fg(p.edit_title).bg(p.tool_bg),
                ),
                Span::styled("+2", Style::default().fg(p.edit_added).bg(p.tool_bg)),
                Span::styled("/", Style::default().fg(p.edit_meta).bg(p.tool_bg)),
                Span::styled("-1 ", Style::default().fg(p.edit_removed).bg(p.tool_bg)),
                Span::styled(
                    format!("  {read_icon} read  {bash_icon} bash"),
                    Style::default().fg(p.muted).bg(p.tool_bg),
                ),
            ]),
            Line::from(vec![
                Span::styled(" - ", Style::default().fg(p.edit_removed).bg(p.tool_bg)),
                Span::styled(
                    "const THEME: &str = \"old\";",
                    Style::default().fg(p.edit_removed).bg(p.tool_bg),
                ),
            ]),
            Line::from(vec![
                Span::styled(" + ", Style::default().fg(p.edit_added).bg(p.tool_bg)),
                Span::styled(
                    format!("let theme = \"{}\";", selected_theme.as_str()),
                    Style::default().fg(p.edit_added).bg(p.tool_bg),
                ),
            ]),
            Line::default(),
            // 5. 思考档位图标
            Line::from(vec![
                Span::styled("Effort: ", Style::default().fg(p.muted)),
                Span::styled(
                    ThinkLevel::ALL
                        .iter()
                        .map(|level| {
                            format!(
                                "{} {}",
                                transcript_style::think_icon_for(*level, selected_icons),
                                level.as_str()
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" · "),
                    Style::default().fg(p.activity),
                ),
            ]),
        ]);
        if spacious || inner.height >= 14 {
            lines.push(Line::default());
        }
        // 6. 状态栏预览
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {app_icon}"),
                Style::default()
                    .fg(p.status_accent)
                    .bg(p.input_status_bg)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(p.dim).bg(p.input_status_bg)),
            Span::styled(
                format!("{model_icon} openai/gpt-5"),
                Style::default().fg(p.status_model).bg(p.input_status_bg),
            ),
            Span::styled(" · ", Style::default().fg(p.dim).bg(p.input_status_bg)),
            Span::styled(
                format!("{think_high} hig"),
                Style::default().fg(p.text).bg(p.input_status_bg),
            ),
            Span::styled(" · ", Style::default().fg(p.dim).bg(p.input_status_bg)),
            Span::styled(
                format!("{dir_icon} ~/pp"),
                Style::default()
                    .fg(p.status_directory)
                    .bg(p.input_status_bg),
            ),
            Span::styled(" · ", Style::default().fg(p.dim).bg(p.input_status_bg)),
            Span::styled(
                format!("{git_icon} main* "),
                Style::default().fg(p.status_git).bg(p.input_status_bg),
            ),
        ]));

        // 填充背景色避免浅色主题下出现终端黑底条纹
        let max_rows = usize::from(inner.height);
        lines.truncate(max_rows);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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
                    "Saving theme and icon preferences...",
                    Style::default().fg(p.accent),
                ),
                area,
            );
            return;
        }
        let hint = match self.step {
            Step::IconMode => {
                "[/] or 1-4 tabs · ↑↓ select · Enter / → / Tab theme · Esc cancel & restore"
            }
            Step::Theme => {
                "[/] or 1-4 tabs · ↑↓ preview · ← / Backspace / Tab icons · Enter save · Esc cancel"
            }
        };
        frame.render_widget(Line::styled(hint, Style::default().fg(p.muted)), area);
    }
}
