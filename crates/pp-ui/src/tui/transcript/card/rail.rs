use super::super::*;
use super::{anatomy, glyphs};

pub(super) fn paint_chrome(
    entry: &TranscriptEntry,
    line: &LayoutLine,
    ctx: super::RowCtx,
    row: u16,
    area: Rect,
    buffer: &mut Buffer,
) {
    let icons = crate::tui::themes::current_icons();
    let g = glyphs::for_style(pp_config::config::ToolStyle::Rail, icons);
    let metrics = super::styled::metrics(pp_config::config::ToolStyle::Rail, area.width);
    let rail_color = match entry.status {
        EntryStatus::Failed => palette::removed(),
        EntryStatus::Running => palette::accent(),
        EntryStatus::Complete => palette::tool_border(),
    };
    let rail_style = Style::default().fg(rail_color);
    let mut left_pad = " ".repeat(usize::from(area.width >= 10));

    match line.source() {
        Some(Source::Top) => {
            let (status_glyph, status_color, _) = anatomy::status_info(entry, icons);
            let node = if entry.is_file_read() {
                g.read_group
            } else {
                g.dot
            };
            let node_color = if entry.status == EntryStatus::Complete {
                anatomy::tool_accent_color(entry)
            } else {
                status_color
            };
            let label = anatomy::tool_label(entry);
            let (subject, meta) = anatomy::subject_and_meta(entry, icons, None);
            let mut left = Vec::new();
            if !left_pad.is_empty() {
                left.push(Span::raw(std::mem::take(&mut left_pad)));
            }
            left.extend([
                Span::styled(
                    format!("{node} "),
                    Style::default().fg(node_color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label,
                    Style::default()
                        .fg(anatomy::tool_accent_color(entry))
                        .add_modifier(Modifier::BOLD),
                ),
            ]);
            if !subject.is_empty() {
                left.push(Span::raw("  "));
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
                let is_last = idx + 1 == entry.read_group.len() && ctx.index + 1 == ctx.total;
                let marker = if is_last { g.last } else { g.rail };
                let mut left = Vec::new();
                if area.width >= 10 {
                    left.push(Span::raw(" "));
                }
                left.push(Span::styled(format!("{marker:<2}"), rail_style));
                left.push(Span::styled(
                    item.path.clone(),
                    Style::default().fg(palette::accent()),
                ));
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
            let lead = if area.width >= 10 {
                format!(" {} ", g.rail)
            } else {
                format!("{} ", g.rail)
            };
            let lead_w = Line::raw(&lead).width();
            let avail =
                usize::from(area.width.saturating_sub(metrics.right_gutter)).saturating_sub(lead_w);
            let mut spans = vec![Span::styled(lead, rail_style)];
            if avail > 10 {
                let label = format!("{} Output ", g.rule.repeat(2));
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
        Some(Source::Bottom) => {
            let lead = if area.width >= 10 {
                format!(" {} ", g.rail_end)
            } else {
                format!("{} ", g.rail_end)
            };
            let lead_w = Line::raw(&lead).width();
            let avail =
                usize::from(area.width.saturating_sub(metrics.right_gutter)).saturating_sub(lead_w);
            let mut spans = vec![Span::styled(lead, rail_style)];
            spans.extend(anatomy::footer_spans(entry, avail, None));
            Line::from(spans).render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        _ => {
            let is_last = ctx.index + 1 == ctx.total;
            let glyph = if is_last { g.last.trim_end() } else { g.rail };
            let prefix = if area.width >= 10 {
                format!(" {glyph}")
            } else {
                glyph.to_owned()
            };
            Span::styled(prefix, rail_style).render(
                Rect::new(area.x, row, metrics.left_gutter.min(area.width), 1),
                buffer,
            );
        }
    }
}
