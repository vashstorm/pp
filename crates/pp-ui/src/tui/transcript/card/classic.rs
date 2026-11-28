use super::super::*;
use pp_config::config::ToolStyle;

pub(super) fn padding(entry: &TranscriptEntry) -> u16 {
    if entry.is_file_read() || matches!(entry.kind, EntryKind::Error | EntryKind::Notice) {
        0
    } else if matches!(entry.kind, EntryKind::Bash | EntryKind::Eval) || entry.is_search() {
        4
    } else {
        2
    }
}

fn entry_tool_name(entry: &TranscriptEntry) -> &str {
    if let Some(tool) = entry.tool.as_deref() {
        tool
    } else if entry.kind == EntryKind::Lsp {
        "lsp"
    } else if entry.kind == EntryKind::Eval {
        "eval"
    } else if entry.kind == EntryKind::Bash {
        "bash"
    } else if entry.kind == EntryKind::Rg {
        "rg"
    } else {
        ""
    }
}

pub(super) fn surface_bg(surface: super::CardSurface, style: ToolStyle) -> Color {
    if style == ToolStyle::Classic2 {
        return palette::classic2_tool_theme(surface.tool_name()).bg;
    }
    match surface {
        super::CardSurface::Bash | super::CardSurface::BashTask => bash_background(),
        super::CardSurface::Eval => eval_background(),
        super::CardSurface::Edit | super::CardSurface::Write => palette::edit_bg(),
        super::CardSurface::Rg | super::CardSurface::Glob | super::CardSurface::WebSearch => {
            palette::rg_bg()
        }
    }
}

pub(super) fn background(entry: &TranscriptEntry, style: ToolStyle) -> Option<Color> {
    if entry.kind == EntryKind::Bash {
        Some(if style == ToolStyle::Classic2 {
            palette::classic2_tool_theme(entry_tool_name(entry)).bg
        } else {
            bash_background()
        })
    } else if entry.kind == EntryKind::Eval {
        Some(if style == ToolStyle::Classic2 {
            palette::classic2_tool_theme("eval").bg
        } else {
            eval_background()
        })
    } else if entry.is_edit() {
        Some(if style == ToolStyle::Classic2 {
            palette::classic2_tool_theme(entry_tool_name(entry)).bg
        } else {
            palette::edit_bg()
        })
    } else if entry.is_search() {
        Some(if style == ToolStyle::Classic2 {
            palette::classic2_tool_theme(entry_tool_name(entry)).bg
        } else {
            palette::rg_bg()
        })
    } else if entry.is_file_read() {
        None
    } else if matches!(entry.kind, EntryKind::Diff | EntryKind::Ask) {
        Some(match entry.status {
            EntryStatus::Running => palette::tool_pending_bg(),
            EntryStatus::Complete => palette::tool_success_bg(),
            EntryStatus::Failed => palette::tool_error_bg(),
        })
    } else {
        entry.kind.conversation_background()
    }
}

pub(super) fn text_target(
    entry: &TranscriptEntry,
    line: &LayoutLine,
    inset: u16,
    area_width: u16,
    before_divider: bool,
    style: ToolStyle,
) -> Option<(u16, u16)> {
    let bash = entry.kind == EntryKind::Bash;
    let eval = entry.kind == EntryKind::Eval;
    let edit = entry.is_edit();
    let file_read = entry.is_file_read();
    let framed = matches!(entry.kind, EntryKind::Error | EntryKind::Notice);
    let search = entry.is_search();

    let padding = if file_read || framed {
        0
    } else if bash || eval || search || (edit && matches!(line.source(), Some(Source::Title))) {
        4
    } else {
        inset
    };
    if area_width <= padding
        || matches!(
            line.source(),
            Some(Source::Top | Source::Divider | Source::Bottom)
        )
    {
        return None;
    }
    let x_offset = if file_read || framed {
        0
    } else if bash && before_divider && style == ToolStyle::Classic2 {
        // Start the command gutter next to the left border.
        inset.saturating_sub(1)
    } else if edit && !matches!(line.source(), Some(Source::Title)) {
        1
    } else {
        inset
    };
    Some((x_offset, area_width - padding))
}

pub(super) fn paint_chrome(
    entry: &TranscriptEntry,
    line: &LayoutLine,
    row: u16,
    area: Rect,
    buffer: &mut Buffer,
    tool_style: ToolStyle,
) {
    let bash = entry.kind == EntryKind::Bash;
    let eval = entry.kind == EntryKind::Eval;
    let edit = entry.is_edit();
    let file_read = entry.is_file_read();
    let framed = matches!(entry.kind, EntryKind::Error | EntryKind::Notice);
    let search = entry.is_search();
    let think = entry.kind == EntryKind::Think;
    let bg = background(entry, tool_style);
    let classic2 = tool_style == ToolStyle::Classic2;
    let tool_name = entry_tool_name(entry);

    if bash || eval || search {
        let (left, right, label) = match line.source() {
            Some(Source::Top) => ("╭", "╮", ""),
            Some(Source::Divider) => ("├", "┤", " Output "),
            Some(Source::Bottom) => ("╰", "╯", ""),
            _ => ("│", "│", ""),
        };
        let card_bg = if classic2 {
            palette::classic2_tool_theme(tool_name).bg
        } else if bash {
            bash_background()
        } else if eval {
            eval_background()
        } else {
            palette::rg_bg()
        };
        let border_color = if entry.status == EntryStatus::Failed {
            palette::removed()
        } else if classic2 {
            palette::classic2_tool_theme(tool_name).border
        } else if bash {
            bash_border()
        } else if eval {
            eval_border()
        } else {
            palette::rg_border()
        };
        let style = Style::default().fg(border_color).bg(card_bg);
        let horizontal = matches!(
            line.source(),
            Some(Source::Top | Source::Divider | Source::Bottom)
        );
        if horizontal {
            Span::styled("─".repeat(usize::from(area.width)), style)
                .render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        Span::styled(left, style).render(Rect::new(area.x, row, 1, 1), buffer);
        if area.width > 1 {
            Span::styled(right, style).render(Rect::new(area.right() - 1, row, 1, 1), buffer);
        }
        if !label.is_empty() && area.width > 4 {
            let label_color = if bash || eval {
                palette::text()
            } else {
                palette::muted()
            };
            Span::styled(label, Style::default().fg(label_color).bg(card_bg))
                .render(Rect::new(area.x + 2, row, area.width - 4, 1), buffer);
        }
        if (bash || eval) && matches!(line.source(), Some(Source::Bottom)) && area.width > 4 {
            let available = usize::from(area.width.saturating_sub(6));
            let warning_needed = !entry.footer_warning.is_empty();
            let warning_width = ratatui::text::Line::raw(entry.footer_warning.as_str()).width();
            let footer_budget = if warning_needed {
                available
                    .saturating_sub(warning_width + if entry.footer.is_empty() { 0 } else { 3 })
            } else {
                available
            };
            let footer = tool_display::clip(&entry.footer, footer_budget, warning_needed);
            let footer_width = ratatui::text::Line::raw(footer.as_str()).width();
            let separator = if footer.is_empty() { "" } else { " · " };
            let remaining = available
                .saturating_sub(footer_width + ratatui::text::Line::raw(separator).width());
            let warning = if warning_needed {
                tool_display::clip(&entry.footer_warning, remaining, false)
            } else {
                String::new()
            };
            let mut spans = vec![Span::styled(
                format!(" {footer}"),
                Style::default().fg(palette::muted()).bg(card_bg),
            )];
            if !warning.is_empty() {
                if !separator.is_empty() {
                    spans.push(Span::styled(
                        separator.to_owned(),
                        Style::default().fg(palette::muted()).bg(card_bg),
                    ));
                }
                spans.push(Span::styled(
                    format!("{warning} "),
                    Style::default().fg(palette::accent()).bg(card_bg),
                ));
            } else if !footer.is_empty() {
                spans.push(Span::styled(
                    " ",
                    Style::default().fg(palette::muted()).bg(card_bg),
                ));
            }
            ratatui::text::Line::from(spans)
                .render(Rect::new(area.x + 2, row, area.width - 4, 1), buffer);
        }
        if bash && matches!(line.source(), Some(Source::Top)) && area.width > 4 {
            let (status, color) = match entry.status {
                EntryStatus::Running => ("running", palette::accent()),
                EntryStatus::Complete => ("done", palette::added()),
                EntryStatus::Failed => ("failed", palette::removed()),
            };
            let title = if entry.tool.as_deref() == Some("bash_task") {
                "Bash task"
            } else {
                "Bash"
            };
            let (title_text, title_color) = if classic2 {
                (
                    format!(" {} {title} ", palette::tool_icon(tool_name)),
                    palette::classic2_tool_theme(tool_name).title,
                )
            } else {
                (format!(" {title} "), palette::bash_title())
            };
            ratatui::text::Line::from(vec![
                Span::styled(title_text, Style::default().fg(title_color).bg(card_bg)),
                Span::styled(
                    format!("· {status} "),
                    Style::default().fg(color).bg(card_bg),
                ),
            ])
            .render(Rect::new(area.x + 2, row, area.width - 4, 1), buffer);
        } else if eval && matches!(line.source(), Some(Source::Top)) && area.width > 4 {
            let lang = tool_display::EvalLang::from_entry(&entry.arguments, entry.result.as_ref());
            let icons = super::super::super::themes::current_icons();
            let (status, color) = match entry.status {
                EntryStatus::Running => ("running", palette::accent()),
                EntryStatus::Complete => ("done", palette::added()),
                EntryStatus::Failed => ("failed", palette::removed()),
            };
            ratatui::text::Line::from(vec![
                Span::styled(
                    format!(" {}", lang.icon(icons)),
                    Style::default()
                        .fg(lang.icon_color())
                        .bg(card_bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {} ", lang.label()),
                    Style::default()
                        .fg(lang.accent())
                        .bg(card_bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("· {status} "),
                    Style::default().fg(color).bg(card_bg),
                ),
            ])
            .render(Rect::new(area.x + 2, row, area.width - 4, 1), buffer);
        }
    } else if edit {
        let (left, right, horizontal, label) = match line.source() {
            Some(Source::Title) => ("╭", "╮", true, ""),
            Some(Source::Divider) => ("├", "┤", true, " Output "),
            Some(Source::Bottom) => ("╰", "╯", true, ""),
            _ => ("│", "│", false, ""),
        };
        let (border_color, card_bg) = if classic2 {
            let theme = palette::classic2_tool_theme(tool_name);
            (theme.border, theme.bg)
        } else {
            (palette::edit_border(), palette::edit_bg())
        };
        let style = Style::default().fg(border_color).bg(card_bg);
        if horizontal {
            Span::styled("─".repeat(usize::from(area.width)), style)
                .render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        Span::styled(left, style).render(Rect::new(area.x, row, 1, 1), buffer);
        if area.width > 1 {
            Span::styled(right, style).render(Rect::new(area.right() - 1, row, 1, 1), buffer);
        }
        if !label.is_empty() && area.width > 4 {
            Span::styled(
                label,
                Style::default().fg(palette::read_text()).bg(if classic2 {
                    card_bg
                } else {
                    palette::read_bg()
                }),
            )
            .render(Rect::new(area.x + 2, row, area.width - 4, 1), buffer);
        }
    } else if !think
        && !file_read
        && !framed
        && entry.kind != EntryKind::Activity
        && bg.is_none()
        && !entry.is_wait_card()
    {
        let (rail, color) = if entry.tool.as_deref() == Some("task") {
            let glyphs = super::glyphs::for_style(
                pp_config::config::ToolStyle::Classic,
                crate::tui::themes::current_icons(),
            );
            (glyphs.rail, entry.tool_accent())
        } else if entry.tool.is_some() || entry.kind == EntryKind::Lsp {
            (
                "│",
                if classic2 {
                    palette::classic2_tool_theme(tool_name).border
                } else {
                    palette::tool_border()
                },
            )
        } else if matches!(entry.body, EntryBody::Tips | EntryBody::Help(_)) {
            ("│", super::super::super::statusline::accent())
        } else {
            ("│", entry.kind.color())
        };
        Span::styled(rail, Style::default().fg(color)).render(Rect::new(area.x, row, 1, 1), buffer);
    }
}
