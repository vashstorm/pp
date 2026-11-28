//! Drawing of the sidebar, role rows and the model/level list.

use super::*;

impl Panel {
    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        if area.is_empty() {
            return;
        }
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::default().style(Style::default().bg(bg()).fg(text_fg())),
            area,
        );
        if area.width < 4 || area.height < 4 {
            return;
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border()))
            .title(super::super::settings::render::tabs_title(
                super::super::settings::Tab::Roles,
                None,
                area,
                gold(),
                text_fg(),
                muted(),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let header = match self.view {
            View::Roles => {
                "Model roles — Space or click the switch icon to toggle; only enabled roles are offered to the model".to_owned()
            }
            View::Models => format!(
                "Select a model for {}{} · type to search · Enter to choose effort",
                self.role().name(), if self.editing_fallback { " · fallback" } else { "" }
            ),
            View::Effort => format!(
                "{} · {}",
                self.role().name(),
                self.pending
                    .as_ref()
                    .map_or("", |assignment| if self.editing_fallback { assignment.fallback.as_ref().map_or("", |f| f.model.as_str()) } else { assignment.model.as_str() })
            ),
        };
        let header_text = pp_core::text::display_text(&header).into_owned();
        self.has_top_bar = inner.height >= 8;
        let main_area = if self.has_top_bar {
            frame.render_widget(
                Line::styled(header_text.clone(), Style::default().fg(muted())),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
            frame.render_widget(
                Line::styled(
                    "─".repeat(usize::from(inner.width)),
                    Style::default().fg(border()),
                ),
                Rect::new(inner.x, inner.y + 1, inner.width, 1),
            );
            Rect::new(
                inner.x,
                inner.y + 2,
                inner.width,
                inner.height.saturating_sub(3),
            )
        } else {
            Rect::new(
                inner.x,
                inner.y,
                inner.width,
                inner.height.saturating_sub(1),
            )
        };
        let sidebar_width = super::super::settings::render::sidebar_width(main_area.width);
        self.sidebar_area = if sidebar_width > 0 {
            Rect::new(main_area.x, main_area.y, sidebar_width, main_area.height)
        } else {
            Rect::default()
        };
        if sidebar_width > 0 {
            let mut lines = vec![];
            for index in 0..self.providers.len() + 2 {
                if index == 2 {
                    lines.push(Line::styled(
                        "─".repeat(usize::from(sidebar_width.saturating_sub(1))),
                        Style::default().fg(border()),
                    ));
                }
                let active = self.sidebar == index;
                let (icon, icon_color, name, count) = match index {
                    0 => (
                        roles_icon(),
                        if active { gold() } else { text_fg() },
                        "Roles".to_owned(),
                        format!(
                            "{}/{}",
                            self.roles
                                .values()
                                .filter(|assignment| assignment.enabled)
                                .count(),
                            ModelRole::ALL.len()
                        ),
                    ),
                    1 => (
                        models_icon(),
                        if active { gold() } else { text_fg() },
                        "All models".to_owned(),
                        self.choices.len().to_string(),
                    ),
                    _ => {
                        let provider = &self.providers[index - 2];
                        (
                            provider_icon(),
                            green(),
                            pp_core::text::display_text(provider).into_owned(),
                            self.choices
                                .iter()
                                .filter(|choice| {
                                    choice
                                        .reference
                                        .split_once('/')
                                        .is_some_and(|(id, _)| id == provider)
                                })
                                .count()
                                .to_string(),
                        )
                    }
                };
                let prefix = if active && self.sidebar_focus {
                    "❯ "
                } else {
                    "  "
                };
                let used = Line::from(format!("{prefix}{icon} {name}{count}")).width();
                let gap = usize::from(sidebar_width.saturating_sub(1))
                    .saturating_sub(used)
                    .max(1);
                let mut spans = vec![
                    Span::styled(prefix, Style::default().fg(gold())),
                    Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
                    Span::styled(
                        name,
                        Style::default().fg(if active { gold() } else { text_fg() }),
                    ),
                ];
                spans.push(Span::styled(
                    format!("{}{count}", " ".repeat(gap)),
                    Style::default().fg(muted()),
                ));
                lines.push(Line::from(spans));
            }
            let selected_row = self.sidebar + usize::from(self.sidebar >= 2);
            let offset = selected_row
                .saturating_sub(usize::from(self.sidebar_area.height.saturating_sub(1)));
            self.sidebar_scroll = offset;
            frame.render_widget(
                Paragraph::new(lines).scroll((offset.min(u16::MAX as usize) as u16, 0)),
                self.sidebar_area,
            );
            frame.render_widget(
                Paragraph::new(vec![Line::from("│"); usize::from(main_area.height)])
                    .style(Style::default().fg(border())),
                Rect::new(
                    main_area.x + sidebar_width,
                    main_area.y,
                    1,
                    main_area.height,
                ),
            );
        }
        let body_x = main_area.x + sidebar_width + u16::from(sidebar_width > 0);
        self.body = Rect::new(
            body_x,
            main_area.y,
            main_area
                .width
                .saturating_sub(sidebar_width + u16::from(sidebar_width > 0)),
            main_area.height,
        );
        if !self.has_top_bar && self.body.height > 0 {
            frame.render_widget(
                Line::styled(header_text, Style::default().fg(muted())),
                Rect::new(self.body.x, self.body.y, self.body.width, 1),
            );
        }
        let content_offset = self.content_offset();
        let subheader_y = self.body.y + u16::from(!self.has_top_bar);
        if self.body.height <= content_offset {
            return;
        }
        let content = Rect::new(
            self.body.x,
            self.body.y + content_offset,
            self.body.width,
            self.body.height - content_offset,
        );
        match self.view {
            View::Roles => {
                // The read-only Override row is pinned above the role list; it shows none without an override.
                let override_y = subheader_y;
                let mut spans = vec![
                    Span::styled("  ", Style::default().fg(muted())),
                    Span::styled(
                        format!("{} {:<8} ", models_icon(), "Override"),
                        Style::default().fg(if self.override_selection.is_some() {
                            gold()
                        } else {
                            muted()
                        }),
                    ),
                ];
                match &self.override_selection {
                    Some(selection) => {
                        spans.extend(model_spans(&selection.reference, gold()));
                        let effort_line = Line::styled(
                            format!(
                                "{} {}",
                                palette::think_icon(selection.effort),
                                selection.effort.as_str()
                            ),
                            Style::default().fg(gold()),
                        );
                        let effort_width =
                            effort_line.width().min(usize::from(self.body.width)) as u16;
                        let model_width = self.body.width.saturating_sub(effort_width + 1);
                        let model_line = Line::from(spans);
                        let model_end = model_line.width().min(usize::from(model_width)) as u16;
                        let effort_x =
                            self.body.x + model_end + u16::from(self.body.width > effort_width);
                        frame.render_widget(
                            model_line,
                            Rect::new(self.body.x, override_y, model_width, 1),
                        );
                        frame.render_widget(
                            effort_line,
                            Rect::new(effort_x, override_y, effort_width, 1),
                        );
                    }
                    None => {
                        spans.push(Span::styled("none", Style::default().fg(muted())));
                        frame.render_widget(
                            Line::from(spans),
                            Rect::new(self.body.x, override_y, self.body.width, 1),
                        );
                    }
                }
                let roles = self.rows();
                self.scroll = self
                    .role_index
                    .saturating_sub(usize::from(content.height.saturating_sub(1)));
                for (row, (role, is_fallback)) in roles
                    .into_iter()
                    .skip(self.scroll)
                    .take(usize::from(content.height))
                    .enumerate()
                {
                    let configured = self.roles.get(&role);
                    let enabled = configured.is_some_and(|assignment| assignment.enabled);
                    let assignment = configured.cloned().unwrap_or_else(|| RoleAssignment {
                        fallback: None,
                        enabled: false,
                        model: self.default_model.clone(),
                        effort: self.default_effort,
                    });
                    let (model, effort) = if is_fallback {
                        let fallback = assignment
                            .fallback
                            .as_ref()
                            .expect("the fallback row comes from a configured model");
                        (&fallback.model, fallback.effort)
                    } else {
                        (&assignment.model, assignment.effort)
                    };
                    let selected = !self.sidebar_focus && self.role_index == row + self.scroll;
                    let color = if !enabled {
                        muted()
                    } else {
                        match role {
                            ModelRole::Default => green(),
                            ModelRole::Fast => Color::Rgb(222, 200, 26),
                            ModelRole::Slow => gold(),
                            ModelRole::Vision => Color::Rgb(255, 64, 85),
                            ModelRole::Plan => Color::Rgb(153, 194, 232),
                            ModelRole::Review => Color::Rgb(187, 154, 247),
                            ModelRole::Explorer => Color::Rgb(126, 207, 197),
                        }
                    };
                    let marker = if is_fallback {
                        fallback_icon()
                    } else if enabled {
                        enabled_icon()
                    } else {
                        disabled_icon()
                    };
                    let label = if is_fallback {
                        format!("         {marker} ")
                    } else {
                        format!("{marker} {:<8} ", role.name())
                    };
                    let mut spans = vec![
                        Span::styled(
                            if selected { "❯ " } else { "  " },
                            Style::default().fg(if enabled { gold() } else { muted() }),
                        ),
                        Span::styled(
                            label,
                            Style::default().fg(if is_fallback { muted() } else { color }),
                        ),
                    ];
                    if configured.is_none() && !is_fallback {
                        spans.push(Span::styled("auto → ", Style::default().fg(muted())));
                    }
                    spans.extend(model_spans(
                        if model.is_empty() { "(not set)" } else { model },
                        color,
                    ));
                    let y = content.y + row as u16;
                    let effort_line = Line::styled(
                        format!("{} {}", palette::think_icon(effort), effort.as_str()),
                        Style::default().fg(color),
                    );
                    let effort_width = effort_line.width().min(usize::from(content.width)) as u16;
                    let model_width = content.width.saturating_sub(effort_width + 1);
                    let model_line = Line::from(spans);
                    let model_end = model_line.width().min(usize::from(model_width)) as u16;
                    let effort_x = content.x + model_end + u16::from(content.width > effort_width);
                    frame.render_widget(model_line, Rect::new(content.x, y, model_width, 1));
                    frame.render_widget(effort_line, Rect::new(effort_x, y, effort_width, 1));
                }
            }
            View::Models => {
                frame.render_widget(
                    Line::styled(
                        format!("{} {}", search_icon(), self.query),
                        Style::default().fg(gold()),
                    ),
                    Rect::new(self.body.x, subheader_y, self.body.width, 1),
                );
                let indices = self.model_indices();
                self.selected = self.selected.min(indices.len().saturating_sub(1));
                self.scroll = self
                    .selected
                    .saturating_sub(usize::from(content.height.saturating_sub(1)));
                if indices.is_empty() {
                    frame.render_widget("No matching models", content);
                }
                for (row, index) in indices
                    .into_iter()
                    .skip(self.scroll)
                    .take(usize::from(content.height))
                    .enumerate()
                {
                    let selected = !self.sidebar_focus && row + self.scroll == self.selected;
                    let mut spans = vec![Span::styled(
                        if selected { "❯ " } else { "  " },
                        Style::default().fg(gold()),
                    )];
                    spans.extend(model_spans(
                        &self.choices[index].reference,
                        if selected { gold() } else { text_fg() },
                    ));
                    if self.favorites.contains(&self.choices[index].reference) {
                        let (fav_label, fav_icon) = if super::super::themes::is_ascii() {
                            (" *", "*")
                        } else {
                            (" \u{f005}", "\u{f005}")
                        };
                        spans.push(Span::styled(
                            fav_label,
                            super::super::menu::model_icon_style(fav_icon),
                        ));
                    }
                    frame.render_widget(
                        Line::from(spans),
                        Rect::new(content.x, content.y + row as u16, content.width, 1),
                    );
                }
            }
            View::Effort => {
                let inset = if self.body.width >= 16 { 2 } else { 0 };
                frame.render_widget(
                    Line::styled(
                        "Choose an effort for this role",
                        Style::default().fg(text_fg()).add_modifier(Modifier::BOLD),
                    ),
                    Rect::new(self.body.x + inset, subheader_y, self.body.width - inset, 1),
                );
                draw_efforts(
                    frame,
                    Rect::new(
                        content.x + inset,
                        content.y,
                        content.width - 2 * inset,
                        content.height,
                    ),
                    &self.efforts(),
                    self.selected,
                );
            }
        }
        let hint = if self.busy {
            "Saving…"
        } else if !self.message.is_empty() {
            &self.message
        } else if self.view == View::Models {
            "↑/↓ rows · Enter pick · Ctrl + F favorite · type to search · Esc back"
        } else if self.view == View::Effort {
            if inner.width >= 51 {
                "←/→ or Tab to switch effort · Enter pick · Esc back"
            } else {
                "←/→ Move · Enter Save · Esc Back"
            }
        } else {
            "↑/↓ rows · Enter pick · f fallback · x clear · t thinking · Space toggle · Delete remove · Backspace clear override · Esc back"
        };
        frame.render_widget(
            Line::styled(hint, Style::default().fg(muted())),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
}

fn draw_efforts(frame: &mut Frame, area: Rect, levels: &[ThinkLevel], selected: usize) {
    if area.is_empty() {
        return;
    }
    if levels.is_empty() {
        frame.render_widget(
            Line::styled("No effort levels available", Style::default().fg(muted())),
            area,
        );
        return;
    }

    let theme = super::super::themes::palette();
    let mut options = Vec::with_capacity(levels.len());
    let (mut column, mut row) = (0, 0);
    for (index, &level) in levels.iter().enumerate() {
        let focused = index == selected;
        let style = Style::default().fg(theme.text).bg(if focused {
            theme.panel_alt_bg
        } else {
            theme.panel_bg
        });
        let accent = style.fg(effort_accent(level, theme.panel_bg));
        let (opening, closing) = if focused { ("[ ", " ]") } else { ("  ", "  ") };
        let line = Line::from(vec![
            Span::styled(opening, accent),
            Span::styled(palette::think_icon(level), accent),
            Span::styled(" ", style),
            Span::styled(level.as_str(), style),
            Span::styled(closing, accent),
        ]);
        let line = if line.width() > usize::from(area.width) {
            // Very narrow widths keep the full level name and the selected state still uses the theme background.
            Line::styled(level.as_str(), style)
        } else {
            line
        };
        let width = (line.width() as u16).min(area.width);
        if column > area.width - width {
            column = 0;
            row += 1;
        }
        options.push((column, row, width, line));
        column += width + 1;
    }

    let top_gap = u16::from(area.height > row + 1);
    let visible_rows = area.height - top_gap;
    let focused_row = options.get(selected).map_or(0, |option| option.1);
    let first_row = focused_row.saturating_sub(visible_rows - 1);
    for (column, row, width, line) in options {
        if !(first_row..first_row + visible_rows).contains(&row) {
            continue;
        }
        frame.render_widget(
            line,
            Rect::new(
                area.x + column,
                area.y + top_gap + (row - first_row),
                width,
                1,
            ),
        );
    }
}

fn effort_accent(level: ThinkLevel, background: Color) -> Color {
    let (on_dark, on_light) = match level {
        ThinkLevel::None => ((174, 184, 204), (71, 85, 105)),
        ThinkLevel::Low => ((139, 211, 172), (37, 104, 73)),
        ThinkLevel::Medium => ((135, 192, 250), (39, 90, 148)),
        ThinkLevel::High => ((246, 191, 102), (128, 82, 11)),
        ThinkLevel::Xhigh => ((201, 166, 247), (105, 63, 145)),
        ThinkLevel::Max => ((246, 146, 153), (158, 55, 74)),
    };
    let light = matches!(background, Color::Rgb(r, g, b)
        if 299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b) >= 128_000);
    let (red, green, blue) = if light { on_light } else { on_dark };
    Color::Rgb(red, green, blue)
}

fn model_spans(reference: &str, color: Color) -> Vec<Span<'static>> {
    let safe = pp_core::text::display_text(reference).into_owned();
    match safe.split_once('/') {
        Some((provider, model)) => vec![
            Span::styled(format!("{provider}/"), Style::default().fg(muted())),
            Span::styled(model.to_owned(), Style::default().fg(color)),
        ],
        None => vec![Span::styled(safe, Style::default().fg(muted()))],
    }
}
