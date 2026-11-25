use super::super::*;

pub(super) fn padding(entry: &TranscriptEntry) -> u16 {
    if entry.is_file_read() || matches!(entry.kind, EntryKind::Error | EntryKind::Notice) {
        0
    } else if matches!(entry.kind, EntryKind::Bash | EntryKind::Eval) || entry.is_search() {
        4
    } else {
        2
    }
}

pub(super) fn surface_bg(surface: super::CardSurface) -> Color {
    match surface {
        super::CardSurface::Bash => bash_background(),
        super::CardSurface::Eval => eval_background(),
        super::CardSurface::Edit => palette::edit_bg(),
        super::CardSurface::Search => palette::rg_bg(),
    }
}

pub(super) fn background(entry: &TranscriptEntry) -> Option<Color> {
    if entry.kind == EntryKind::Bash {
        Some(bash_background())
    } else if entry.kind == EntryKind::Eval {
        Some(eval_background())
    } else if entry.is_edit() {
        Some(palette::edit_bg())
    } else if entry.is_search() {
        Some(palette::rg_bg())
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
) {
    let bash = entry.kind == EntryKind::Bash;
    let eval = entry.kind == EntryKind::Eval;
    let edit = entry.is_edit();
    let file_read = entry.is_file_read();
    let framed = matches!(entry.kind, EntryKind::Error | EntryKind::Notice);
    let search = entry.is_search();
    let think = entry.kind == EntryKind::Think;
    let bg = background(entry);

    if bash || eval || search {
        let (left, right, label) = match line.source() {
            Some(Source::Top) => ("╭", "╮", ""),
            Some(Source::Divider) => ("├", "┤", " Output "),
            Some(Source::Bottom) => ("╰", "╯", ""),
            _ => ("│", "│", ""),
        };
        let card_bg = if bash {
            bash_background()
        } else if eval {
            eval_background()
        } else {
            palette::rg_bg()
        };
        let border_color = if entry.status == EntryStatus::Failed {
            palette::removed()
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
            ratatui::text::Line::from(vec![
                Span::styled(
                    format!(" {title} "),
                    Style::default().fg(palette::bash_title()).bg(card_bg),
                ),
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
        let style = Style::default()
            .fg(palette::edit_border())
            .bg(palette::edit_bg());
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
                Style::default()
                    .fg(palette::read_text())
                    .bg(palette::read_bg()),
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
        let color = if entry.tool.is_some() {
            palette::tool_border()
        } else if matches!(entry.body, EntryBody::Tips | EntryBody::Help(_)) {
            super::super::super::statusline::accent()
        } else {
            entry.kind.color()
        };
        Span::styled("│", Style::default().fg(color)).render(Rect::new(area.x, row, 1, 1), buffer);
    }
}
