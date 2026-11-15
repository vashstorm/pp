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

        let step_label = self.step_label();
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(p.panel_border))
            .title(super::super::settings::render::tabs_title(
                super::super::settings::Tab::Themes,
                Some(step_label),
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

        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(inner);

        self.draw_steps(frame, sections[0], p, selected_theme, selected_icons, ascii);

        let body = sections[1];
        if body.width >= 68 {
            let left_width = (body.width * 38 / 100).clamp(28, 44);
            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Length(left_width),
                    Constraint::Length(1),
                    Constraint::Min(24),
                ])
                .split(body);
            self.draw_list(frame, cols[0], p, selected_theme, selected_icons, ascii);
            frame.render_widget(
                Paragraph::new(vec![Line::from("│"); usize::from(cols[1].height)])
                    .style(Style::default().fg(p.dim)),
                cols[1],
            );
            self.draw_preview(frame, cols[2], p, selected_theme, selected_icons, ascii);
        } else {
            let list_height = match self.step {
                Step::IconMode => 8,
                Step::Theme => 11,
            }
            .min(body.height.saturating_sub(4).max(3));
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(list_height), Constraint::Min(1)])
                .split(body);
            self.draw_list(frame, rows[0], p, selected_theme, selected_icons, ascii);
            self.draw_preview(frame, rows[1], p, selected_theme, selected_icons, ascii);
        }

        self.draw_footer(frame, sections[2], p);
    }

    fn draw_steps(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        p: &Palette,
        selected_theme: Theme,
        selected_icons: IconMode,
        ascii: bool,
    ) {
        let step1_active = self.step == Step::IconMode;
        let step2_active = self.step == Step::Theme;
        let step1_text = format!(" 1. Icons: {} ", selected_icons.label());
        let step2_text = format!(" 2. Theme: {} ", selected_theme.label());
        let step1_len = Line::from(step1_text.as_str()).width() as u16;
        let step2_len = Line::from(step2_text.as_str()).width() as u16;

        self.step_icon_area = Rect::new(area.x, area.y, step1_len.min(area.width), 1);
        let arrow_x = area.x + step1_len;
        let step2_x = arrow_x.saturating_add(3);
        self.step_theme_area = if step2_x < area.right() {
            Rect::new(step2_x, area.y, step2_len.min(area.right() - step2_x), 1)
        } else {
            Rect::default()
        };

        let badge = |active: bool| {
            if active {
                Style::default()
                    .bg(p.panel_selected_bg)
                    .fg(p.panel_selected_fg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().bg(p.panel_alt_bg).fg(p.muted)
            }
        };
        let mode_tag = if ascii { "ASCII" } else { "Nerd Font" };
        let saved_tag = format!(
            "  saved: {} / {}",
            self.saved_icons.label(),
            self.saved_theme.label()
        );
        let top = Line::from(vec![
            Span::styled(step1_text, badge(step1_active)),
            Span::styled(" > ", Style::default().fg(p.dim)),
            Span::styled(step2_text, badge(step2_active)),
            Span::styled(
                format!("  [{mode_tag}]"),
                Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(saved_tag, Style::default().fg(p.muted)),
        ]);
        frame.render_widget(top, Rect::new(area.x, area.y, area.width, 1));
        if area.height >= 2 {
            frame.render_widget(
                Line::styled(
                    "─".repeat(usize::from(area.width)),
                    Style::default().fg(p.dim),
                ),
                Rect::new(area.x, area.y + 1, area.width, 1),
            );
        }
    }

    fn draw_list(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        p: &Palette,
        selected_theme: Theme,
        selected_icons: IconMode,
        ascii: bool,
    ) {
        self.list_area = area;
        if area.is_empty() {
            return;
        }
        match self.step {
            Step::IconMode => {
                self.row_height = 4;
                let mut lines = Vec::new();
                for (idx, mode) in IconMode::ALL.iter().copied().enumerate() {
                    let selected = idx == self.icon_index;
                    let is_saved = mode == self.saved_icons;
                    let mode_ascii = mode == IconMode::Ascii;
                    let row_bg = if selected { p.panel_alt_bg } else { p.panel_bg };
                    let marker = if selected { "❯ " } else { "  " };
                    let saved_badge = if is_saved { "  [saved]" } else { "" };
                    let sample = if mode_ascii {
                        ">  @  /  *  +  R  E  3"
                    } else {
                        "\u{f1d8}  \u{ec19}  \u{f07c}  \u{f126}  \u{f00c}  \u{f0219}  \u{f03eb}  \u{f0aa3}"
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            marker,
                            Style::default()
                                .fg(p.accent)
                                .bg(row_bg)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            mode.label(),
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
                        Span::styled(saved_badge, Style::default().fg(p.added).bg(row_bg)),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("  ", Style::default().bg(row_bg)),
                        Span::styled(
                            mode.description(),
                            Style::default().fg(p.activity).bg(row_bg),
                        ),
                    ]));
                    lines.push(Line::from(vec![
                        Span::styled("  ", Style::default().bg(row_bg)),
                        Span::styled("Glyphs: ", Style::default().fg(p.muted).bg(row_bg)),
                        Span::styled(sample, Style::default().fg(p.status_model).bg(row_bg)),
                    ]));
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(vec![
                    Span::styled("Next: ", Style::default().fg(p.muted)),
                    Span::styled(
                        format!("Select color theme (current: {})", selected_theme.label()),
                        Style::default().fg(p.text),
                    ),
                ]));
                let hint = if ascii {
                    "Tip: Pure ASCII keeps all borders & glyphs terminal-safe."
                } else {
                    "Tip: Nerd Font requires a patched Nerd Font in your terminal."
                };
                lines.push(Line::styled(hint, Style::default().fg(p.dim)));
                frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
            }
            Step::Theme => {
                self.row_height = 2;
                let mut lines = Vec::new();
                for (idx, theme) in Theme::ALL.iter().copied().enumerate() {
                    let selected = idx == self.theme_index;
                    let is_saved = theme == self.saved_theme;
                    let row_bg = if selected { p.panel_alt_bg } else { p.panel_bg };
                    let marker = if selected { "❯ " } else { "  " };
                    let saved_icon = if is_saved {
                        if ascii { "* " } else { "● " }
                    } else {
                        "  "
                    };
                    let mut row1 = vec![
                        Span::styled(
                            marker,
                            Style::default()
                                .fg(p.accent)
                                .bg(row_bg)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(saved_icon, Style::default().fg(p.added).bg(row_bg)),
                        Span::styled(
                            theme.label(),
                            Style::default()
                                .fg(if selected { p.accent } else { p.text })
                                .bg(row_bg)
                                .add_modifier(if selected {
                                    Modifier::BOLD
                                } else {
                                    Modifier::empty()
                                }),
                        ),
                        Span::styled("  ", Style::default().bg(row_bg)),
                    ];
                    for color in palette_for(theme).swatch() {
                        row1.push(Span::styled("██", Style::default().fg(color).bg(row_bg)));
                    }
                    lines.push(Line::from(row1));
                    lines.push(Line::from(vec![
                        Span::styled("    ", Style::default().bg(row_bg)),
                        Span::styled(
                            format!("{} · {}", theme.as_str(), theme.description()),
                            Style::default().fg(p.muted).bg(row_bg),
                        ),
                    ]));
                }
                if selected_icons == IconMode::Ascii {
                    lines.push(Line::default());
                    lines.push(Line::styled(
                        "Icon style: ASCII",
                        Style::default().fg(p.status_directory),
                    ));
                }
                frame.render_widget(Paragraph::new(lines), area);
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
            Line::default(),
            // 2. 用户消息
            Line::from(vec![
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
            ]),
            Line::default(),
            // 3. 助手 Markdown + 语法高亮代码块
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
            Line::default(),
            // 6. 状态栏预览
            Line::from(vec![
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
            ]),
        ];

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
                Line::styled(error.clone(), Style::default().fg(p.removed)),
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
                "↑/↓ select icon style · Enter / → next: choose theme · Esc cancel & restore"
            }
            Step::Theme => {
                "↑/↓ preview theme · Backspace / ← back: icon style · Enter save & apply · Esc cancel & restore"
            }
        };
        frame.render_widget(Line::styled(hint, Style::default().fg(p.muted)), area);
    }
}
