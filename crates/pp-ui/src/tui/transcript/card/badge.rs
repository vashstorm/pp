use super::super::*;
use super::{anatomy, glyphs, tone};
use pp_config::config::IconMode;

pub(super) fn paint_chrome(
    entry: &TranscriptEntry,
    line: &LayoutLine,
    _ctx: super::RowCtx,
    row: u16,
    area: Rect,
    buffer: &mut Buffer,
) {
    let icons = crate::tui::themes::current_icons();
    let g = glyphs::for_style(pp_config::config::ToolStyle::Badge, icons);
    let metrics = super::styled::metrics(pp_config::config::ToolStyle::Badge, area.width);
    let failed = entry.status == EntryStatus::Failed;
    let accent = if failed {
        palette::removed()
    } else {
        anatomy::tool_accent_color(entry)
    };
    let badge_fg = tone::badge_fg(accent);
    let badge_style = Style::default()
        .fg(badge_fg)
        .bg(accent)
        .add_modifier(Modifier::BOLD);
    let indent = " ".repeat(usize::from(metrics.left_gutter));

    match line.source() {
        Some(Source::Top) => {
            let (status_glyph, status_color, _) = anatomy::status_info(entry, icons);
            let label = anatomy::tool_label(entry);
            let pill = if icons == IconMode::Ascii {
                format!("[{label}]")
            } else {
                format!(" {label} ")
            };
            let (subject, meta) = anatomy::subject_and_meta(entry, icons, None);
            let mut left = Vec::new();
            if area.width >= 10 {
                left.push(Span::raw(" "));
            }
            left.push(Span::styled(pill, badge_style));
            if !subject.is_empty() {
                left.push(Span::raw(" "));
                left.extend(subject);
            }
            let mut right = Vec::new();
            if !meta.is_empty() {
                right.push(Span::styled(
                    format!("{meta}  "),
                    Style::default().fg(palette::muted()),
                ));
            }
            right.push(Span::styled(
                status_glyph,
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ));
            let content_w = usize::from(area.width.saturating_sub(metrics.right_gutter));
            let composed = anatomy::compose_header(left, right, content_w, None);
            Line::from(composed).render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        Some(Source::ReadItem(idx)) => {
            if let Some(item) = entry.read_group.get(idx) {
                let left = vec![
                    Span::raw(indent),
                    Span::styled(item.path.clone(), Style::default().fg(palette::accent())),
                ];
                let right = if item.meta.is_empty() {
                    Vec::new()
                } else {
                    vec![Span::styled(
                        item.meta.clone(),
                        Style::default().fg(palette::activity()),
                    )]
                };
                let content_w = usize::from(area.width.saturating_sub(metrics.right_gutter));
                let composed = anatomy::compose_header(left, right, content_w, None);
                Line::from(composed).render(Rect::new(area.x, row, area.width, 1), buffer);
            }
        }
        Some(Source::Divider) => {
            let avail = usize::from(area.width.saturating_sub(metrics.right_gutter))
                .saturating_sub(usize::from(metrics.left_gutter));
            let mut spans = vec![Span::raw(indent)];
            if avail > 10 {
                let label = format!("{} Output ", g.rule.repeat(2));
                let rest = avail.saturating_sub(Line::raw(&label).width());
                spans.push(Span::styled(label, Style::default().fg(accent)));
                spans.push(Span::styled(
                    g.rule.repeat(rest),
                    Style::default().fg(palette::dim()),
                ));
            } else {
                spans.push(Span::styled(
                    g.rule.repeat(avail),
                    Style::default().fg(palette::dim()),
                ));
            }
            Line::from(spans).render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        Some(Source::Bottom) => {
            let avail = usize::from(area.width.saturating_sub(metrics.right_gutter))
                .saturating_sub(usize::from(metrics.left_gutter));
            let mut spans = vec![Span::raw(indent)];
            let prefix = g.rule.repeat(2);
            let prefix_w = Line::raw(&prefix).width();
            spans.push(Span::styled(
                format!("{prefix} "),
                Style::default().fg(palette::dim()),
            ));
            let footer_spans =
                anatomy::footer_spans(entry, avail.saturating_sub(prefix_w + 4), None);
            let footer_w: usize = footer_spans.iter().map(Span::width).sum();
            spans.extend(footer_spans);
            let used = prefix_w + 1 + footer_w;
            if avail > used + 2 {
                spans.push(Span::styled(
                    format!(" {}", g.rule.repeat(avail - used - 1)),
                    Style::default().fg(palette::dim()),
                ));
            }
            Line::from(spans).render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        _ => {}
    }
}
