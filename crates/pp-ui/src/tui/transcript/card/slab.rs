use super::super::*;
use super::{anatomy, glyphs, tone};

pub(super) fn paint_chrome(
    entry: &TranscriptEntry,
    line: &LayoutLine,
    _ctx: super::RowCtx,
    row: u16,
    area: Rect,
    buffer: &mut Buffer,
) {
    let p = crate::tui::themes::palette();
    let icons = crate::tui::themes::current_icons();
    let g = glyphs::for_style(pp_config::config::ToolStyle::Slab, icons);
    let metrics = super::styled::metrics(pp_config::config::ToolStyle::Slab, area.width);
    let failed = entry.status == EntryStatus::Failed;
    let accent = if failed {
        palette::removed()
    } else {
        anatomy::tool_accent_color(entry)
    };
    let body_bg = tone::slab_body_bg(p);
    let header_bg = tone::slab_header_bg(p, failed);
    let bar_pad = if area.width >= 10 { "  " } else { " " };

    match line.source() {
        Some(Source::Top) => {
            buffer.set_style(
                Rect::new(area.x, row, area.width, 1),
                Style::default().bg(header_bg),
            );
            let (status_glyph, status_color, _) = anatomy::status_info(entry, icons);
            let icon = anatomy::tool_glyph(entry, icons);
            let label = anatomy::tool_label(entry);
            let (subject, meta) = anatomy::subject_and_meta(entry, icons, None);
            let mut left = vec![
                Span::styled(
                    format!("{}{bar_pad}", g.bar),
                    Style::default().fg(accent).bg(header_bg),
                ),
                Span::styled(
                    format!("{icon} "),
                    Style::default()
                        .fg(accent)
                        .bg(header_bg)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label,
                    Style::default()
                        .fg(accent)
                        .bg(header_bg)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            if !subject.is_empty() {
                left.push(Span::styled("  ", Style::default().bg(header_bg)));
                for span in subject {
                    left.push(Span::styled(span.content, span.style.bg(header_bg)));
                }
            }
            let mut right = Vec::new();
            if !meta.is_empty() {
                right.push(Span::styled(
                    format!("{meta}  "),
                    Style::default().fg(palette::muted()).bg(header_bg),
                ));
            }
            right.push(Span::styled(
                status_glyph,
                Style::default()
                    .fg(status_color)
                    .bg(header_bg)
                    .add_modifier(Modifier::BOLD),
            ));
            let content_w = usize::from(area.width.saturating_sub(metrics.right_gutter));
            let composed = anatomy::compose_header(left, right, content_w, Some(header_bg));
            Line::from(composed).render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        Some(Source::ReadItem(idx)) => {
            if let Some(item) = entry.read_group.get(idx) {
                let left = vec![
                    Span::styled(
                        format!("{}{bar_pad}", g.bar),
                        Style::default().fg(accent).bg(body_bg),
                    ),
                    Span::styled(
                        item.path.clone(),
                        Style::default().fg(palette::accent()).bg(body_bg),
                    ),
                ];
                let right = if item.meta.is_empty() {
                    Vec::new()
                } else {
                    vec![Span::styled(
                        item.meta.clone(),
                        Style::default().fg(palette::activity()).bg(body_bg),
                    )]
                };
                let content_w = usize::from(area.width.saturating_sub(metrics.right_gutter));
                let composed = anatomy::compose_header(left, right, content_w, Some(body_bg));
                Line::from(composed).render(Rect::new(area.x, row, area.width, 1), buffer);
            }
        }
        Some(Source::Divider) => {
            let lead = format!("{}{bar_pad}", g.bar);
            let avail = usize::from(area.width.saturating_sub(metrics.right_gutter))
                .saturating_sub(Line::raw(&lead).width());
            let mut spans = vec![Span::styled(lead, Style::default().fg(accent).bg(body_bg))];
            if avail > 10 {
                let label = format!("{} Output ", g.rule.repeat(2));
                let rest = avail.saturating_sub(Line::raw(&label).width());
                spans.push(Span::styled(
                    label,
                    Style::default().fg(palette::muted()).bg(body_bg),
                ));
                spans.push(Span::styled(
                    g.rule.repeat(rest),
                    Style::default().fg(palette::dim()).bg(body_bg),
                ));
            } else {
                spans.push(Span::styled(
                    g.rule.repeat(avail),
                    Style::default().fg(palette::dim()).bg(body_bg),
                ));
            }
            Line::from(spans).render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        Some(Source::Bottom) => {
            let lead = format!("{}{bar_pad}", g.bar);
            let avail = usize::from(area.width.saturating_sub(metrics.right_gutter))
                .saturating_sub(Line::raw(&lead).width());
            let mut spans = vec![Span::styled(lead, Style::default().fg(accent).bg(body_bg))];
            spans.extend(anatomy::footer_spans(entry, avail, Some(body_bg)));
            Line::from(spans).render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        _ => {
            Span::styled(g.bar, Style::default().fg(accent).bg(body_bg))
                .render(Rect::new(area.x, row, 1, 1), buffer);
        }
    }
}
