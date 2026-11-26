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
    let g = glyphs::for_style(pp_config::config::ToolStyle::Hanging, icons);
    let metrics = super::styled::metrics(pp_config::config::ToolStyle::Hanging, area.width);
    let (_, status_color, status_label) = anatomy::status_info(entry, icons);
    let accent = anatomy::tool_accent_color(entry);

    match line.source() {
        Some(Source::Top) => {
            let label = anatomy::tool_label(entry);
            let inner_w = area.width.saturating_sub(metrics.padding()).max(1);
            let inline_cmd = anatomy::inline_bash_command(entry, inner_w);
            let (subject, mut meta) = anatomy::subject_and_meta(entry, icons, inline_cmd);
            if entry.status == EntryStatus::Failed && !meta.contains(status_label) {
                meta = if meta.is_empty() {
                    status_label.to_owned()
                } else {
                    format!("{meta} · {status_label}")
                };
            }
            let dot = if entry.status == EntryStatus::Running {
                entry.read_clock
            } else {
                g.dot
            };
            let mut left = vec![
                Span::styled(
                    format!("{dot} "),
                    Style::default()
                        .fg(status_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    label,
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ),
            ];
            if !subject.is_empty() {
                left.push(Span::styled("(", Style::default().fg(palette::muted())));
                left.extend(subject);
                left.push(Span::styled(")", Style::default().fg(palette::muted())));
            }
            let right = if meta.is_empty() {
                Vec::new()
            } else {
                vec![Span::styled(
                    meta,
                    Style::default().fg(if entry.status == EntryStatus::Failed {
                        palette::removed()
                    } else {
                        palette::muted()
                    }),
                )]
            };
            let content_w = usize::from(area.width.saturating_sub(metrics.right_gutter));
            let composed = anatomy::compose_header(left, right, content_w, None);
            Line::from(composed).render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        Some(Source::ReadItem(idx)) => {
            if let Some(item) = entry.read_group.get(idx) {
                let prefix = if area.width >= 10 {
                    if ctx.is_first_body {
                        format!("  {} ", g.hang)
                    } else {
                        "    ".to_owned()
                    }
                } else if ctx.is_first_body {
                    format!("{} ", g.hang)
                } else {
                    "  ".to_owned()
                };
                let left = vec![
                    Span::styled(prefix, Style::default().fg(palette::muted())),
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
            let lead = if area.width >= 10 { "  " } else { "" };
            let spans = vec![
                Span::raw(lead),
                Span::styled(
                    format!("{} output", g.rule.repeat(2)),
                    Style::default().fg(palette::dim()),
                ),
            ];
            Line::from(spans).render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        Some(Source::Bottom) => {
            let indent = " ".repeat(usize::from(metrics.left_gutter));
            let avail = usize::from(area.width.saturating_sub(metrics.right_gutter))
                .saturating_sub(usize::from(metrics.left_gutter));
            let mut spans = vec![Span::raw(indent)];
            spans.extend(anatomy::footer_spans(entry, avail, None));
            Line::from(spans).render(Rect::new(area.x, row, area.width, 1), buffer);
        }
        _ if ctx.is_first_body => {
            let prefix = if area.width >= 10 {
                format!("  {}", g.hang)
            } else {
                g.hang.to_owned()
            };
            Span::styled(prefix, Style::default().fg(palette::muted())).render(
                Rect::new(area.x, row, metrics.left_gutter.min(area.width), 1),
                buffer,
            );
        }
        _ => {}
    }
}
