//! 侧栏、角色行与模型／档位列表的绘制。

use super::*;

impl Panel {
    pub(in crate::tui) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        self.area = area;
        frame.render_widget(
            Block::default().style(Style::default().bg(BG).fg(TEXT)),
            area,
        );
        if area.width < 4 || area.height < 4 {
            return;
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(BORDER))
            .title(Line::styled(" Models ", Style::default().fg(GOLD)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let sidebar_width = if inner.width >= 54 {
            (inner.width / 5).clamp(22, 32)
        } else {
            0
        };
        self.sidebar_area = Rect::new(
            inner.x,
            inner.y,
            sidebar_width.saturating_sub(1),
            inner.height.saturating_sub(1),
        );
        if sidebar_width > 0 {
            let mut lines = vec![];
            for index in 0..self.providers.len() + 2 {
                if index == 2 {
                    lines.push(Line::styled(
                        "─".repeat(usize::from(sidebar_width.saturating_sub(2))),
                        Style::default().fg(BORDER),
                    ));
                }
                let active = self.sidebar == index;
                let (icon, icon_color, name, count) = match index {
                    0 => (
                        ROLES_ICON,
                        if active { GOLD } else { TEXT },
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
                        MODELS_ICON,
                        if active { GOLD } else { TEXT },
                        "All models".to_owned(),
                        self.choices.len().to_string(),
                    ),
                    _ => {
                        let provider = &self.providers[index - 2];
                        (
                            PROVIDER_ICON,
                            GREEN,
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
                let gap = usize::from(sidebar_width.saturating_sub(2))
                    .saturating_sub(used)
                    .max(1);
                let mut spans = vec![
                    Span::styled(prefix, Style::default().fg(GOLD)),
                    Span::styled(format!("{icon} "), Style::default().fg(icon_color)),
                    Span::styled(name, Style::default().fg(if active { GOLD } else { TEXT })),
                ];
                spans.push(Span::styled(
                    format!("{}{count}", " ".repeat(gap)),
                    Style::default().fg(MUTED),
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
                Paragraph::new(vec![Line::from("│"); usize::from(inner.height)])
                    .style(Style::default().fg(BORDER)),
                Rect::new(inner.x + sidebar_width - 1, inner.y, 1, inner.height),
            );
        }
        self.body = Rect::new(
            inner.x + sidebar_width + u16::from(sidebar_width > 0),
            inner.y,
            inner
                .width
                .saturating_sub(sidebar_width + u16::from(sidebar_width > 0)),
            inner.height.saturating_sub(2),
        );
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
        frame.render_widget(
            Line::styled(
                pp_core::text::display_text(&header).into_owned(),
                Style::default().fg(MUTED),
            ),
            Rect::new(self.body.x, self.body.y, self.body.width, 1),
        );
        let content_offset = self.content_offset();
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
                // 只读 Override 行固定在角色列表上方；无覆盖时显示 none。
                let override_y = self.body.y + 1;
                let mut spans = vec![
                    Span::styled("  ", Style::default().fg(MUTED)),
                    Span::styled(
                        format!("{MODELS_ICON} {:<8} ", "Override"),
                        Style::default().fg(if self.override_selection.is_some() {
                            GOLD
                        } else {
                            MUTED
                        }),
                    ),
                ];
                match &self.override_selection {
                    Some(selection) => {
                        spans.extend(model_spans(&selection.reference, GOLD));
                        let effort_line = Line::styled(
                            format!(
                                "{} {}",
                                palette::think_icon(selection.effort),
                                selection.effort.as_str()
                            ),
                            Style::default().fg(GOLD),
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
                        spans.push(Span::styled("none", Style::default().fg(MUTED)));
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
                        let fallback = assignment.fallback.as_ref().expect("备用行来自已配置模型");
                        (&fallback.model, fallback.effort)
                    } else {
                        (&assignment.model, assignment.effort)
                    };
                    let selected = !self.sidebar_focus && self.role_index == row + self.scroll;
                    let color = if !enabled {
                        MUTED
                    } else {
                        match role {
                            ModelRole::Default => GREEN,
                            ModelRole::Fast => Color::Rgb(222, 200, 26),
                            ModelRole::Slow => GOLD,
                            ModelRole::Vision => Color::Rgb(255, 64, 85),
                            ModelRole::Plan => Color::Rgb(153, 194, 232),
                            ModelRole::Review => Color::Rgb(187, 154, 247),
                            ModelRole::Explorer => Color::Rgb(126, 207, 197),
                        }
                    };
                    let marker = if is_fallback {
                        FALLBACK_ICON
                    } else if enabled {
                        ENABLED_ICON
                    } else {
                        DISABLED_ICON
                    };
                    let label = if is_fallback {
                        format!("         {marker} ")
                    } else {
                        format!("{marker} {:<8} ", role.name())
                    };
                    let mut spans = vec![
                        Span::styled(
                            if selected { "❯ " } else { "  " },
                            Style::default().fg(if enabled { GOLD } else { MUTED }),
                        ),
                        Span::styled(
                            label,
                            Style::default().fg(if is_fallback { MUTED } else { color }),
                        ),
                    ];
                    if configured.is_none() && !is_fallback {
                        spans.push(Span::styled("auto → ", Style::default().fg(MUTED)));
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
                        format!("{SEARCH_ICON} {}", self.query),
                        Style::default().fg(GOLD),
                    ),
                    Rect::new(self.body.x, self.body.y + 1, self.body.width, 1),
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
                        Style::default().fg(GOLD),
                    )];
                    spans.extend(model_spans(
                        &self.choices[index].reference,
                        if selected { GOLD } else { TEXT },
                    ));
                    if self.favorites.contains(&self.choices[index].reference) {
                        spans.push(Span::styled(
                            " \u{f005}",
                            super::super::menu::model_icon_style("\u{f005}"),
                        ));
                    }
                    frame.render_widget(
                        Line::from(spans),
                        Rect::new(content.x, content.y + row as u16, content.width, 1),
                    );
                }
            }
            View::Effort => {
                frame.render_widget(
                    "Choose an effort for this role (only levels supported by the model)",
                    Rect::new(self.body.x, self.body.y + 1, self.body.width, 1),
                );
                let chips = self
                    .efforts()
                    .iter()
                    .enumerate()
                    .map(|(index, level)| {
                        Span::styled(
                            format!(" {} ", level.as_str()),
                            if index == self.selected {
                                Style::default()
                                    .fg(BG)
                                    .bg(GOLD)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(TEXT)
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                frame.render_widget(
                    Paragraph::new(Line::from(chips)).wrap(ratatui::widgets::Wrap { trim: false }),
                    content,
                );
            }
        }
        let hint = if self.busy {
            "Saving…"
        } else if !self.message.is_empty() {
            &self.message
        } else if self.view == View::Models {
            "↑/↓ rows · Enter pick · Ctrl + F favorite · type to search · Esc back"
        } else {
            "↑/↓ rows · Enter pick · f fallback · x clear · t thinking · Space toggle · Delete remove · Backspace clear override · Esc back"
        };
        frame.render_widget(
            Line::styled(hint, Style::default().fg(MUTED)),
            Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
        );
    }
}

fn model_spans(reference: &str, color: Color) -> Vec<Span<'static>> {
    let safe = pp_core::text::display_text(reference).into_owned();
    match safe.split_once('/') {
        Some((provider, model)) => vec![
            Span::styled(format!("{provider}/"), Style::default().fg(MUTED)),
            Span::styled(model.to_owned(), Style::default().fg(color)),
        ],
        None => vec![Span::styled(safe, Style::default().fg(MUTED))],
    }
}
