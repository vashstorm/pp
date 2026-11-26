use super::super::*;
use super::{anatomy, glyphs};

fn format_tag(label: &str) -> String {
    let chars: Vec<char> = label.chars().collect();
    if chars.len() <= 6 {
        format!("{label:<7}")
    } else {
        let mut s: String = chars[..5].iter().collect();
        s.push('…');
        s.push(' ');
        s
    }
}

pub(super) fn paint_chrome(
    entry: &TranscriptEntry,
    line: &LayoutLine,
    ctx: super::RowCtx,
    row: u16,
    area: Rect,
    buffer: &mut Buffer,
) {
    let icons = crate::tui::themes::current_icons();
    let g = glyphs::for_style(pp_config::config::ToolStyle::Ledger, icons);
    let metrics = super::styled::metrics(pp_config::config::ToolStyle::Ledger, area.width);
    let accent = if entry.status == EntryStatus::Failed {
        palette::removed()
    } else {
        anatomy::tool_accent_color(entry)
    };
    let indent = " ".repeat(usize::from(metrics.left_gutter));
    let wide = area.width >= 16;

    match line.source() {
        Some(Source::Top) => {
            let (status_glyph, status_color, _) = anatomy::status_info(entry, icons);
            let label = anatomy::tool_label(entry);
            let (subject, meta) = anatomy::subject_and_meta(entry, icons, None);
            let mut left = Vec::new();
            if wide {
                left.push(Span::raw(" "));
                left.push(Span::styled(
                    format_tag(&label),
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ));
            } else {
                left.push(Span::styled(
                    format!("{label} "),
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ));
            }
            if !subject.is_empty() {
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
                let (status_glyph, status_color, _) = anatomy::status_info(entry, icons);
                let left = vec![
                    Span::raw(indent),
                    Span::styled(item.path.clone(), Style::default().fg(palette::accent())),
                ];
                let mut right = Vec::new();
                if !item.meta.is_empty() {
                    right.push(Span::styled(
                        format!("{}  ", item.meta),
                        Style::default().fg(palette::activity()),
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
        }
        Some(Source::Divider) => {
            if ctx.index == 1 {
                let lead = if wide { " " } else { "" };
                let avail = usize::from(area.width.saturating_sub(metrics.right_gutter))
                    .saturating_sub(lead.len());
                let spans = vec![
                    Span::raw(lead),
                    Span::styled(
                        g.rule.repeat(avail),
                        Style::default().fg(crate::tui::themes::palette().panel_border),
                    ),
                ];
                Line::from(spans).render(Rect::new(area.x, row, area.width, 1), buffer);
            } else {
                let avail = usize::from(area.width.saturating_sub(metrics.right_gutter))
                    .saturating_sub(usize::from(metrics.left_gutter));
                let mut spans = vec![Span::raw(indent)];
                if avail > 10 {
                    let label = format!("{} output ", g.rule.repeat(2));
                    let rest = avail.saturating_sub(Line::raw(&label).width());
                    spans.push(Span::styled(label, Style::default().fg(palette::muted())));
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
        }
        Some(Source::Bottom) => {
            let avail = usize::from(area.width.saturating_sub(metrics.right_gutter))
                .saturating_sub(usize::from(metrics.left_gutter));
            let mut spans = vec![Span::raw(indent)];
            spans.extend(anatomy::footer_spans(entry, avail, None));
            Line::from(spans).render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        _ => {}
    }
}
