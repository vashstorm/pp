use super::statusline::accent;
use crate::tips::{Section, Tip, key_width, row_width, sorted};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

fn first() -> Color {
    super::themes::palette().welcome_first
}

fn separator() -> Color {
    super::themes::palette().welcome_separator
}

fn muted() -> Color {
    super::themes::palette().welcome_muted
}

fn tip_spans(tip: &Tip, key_width: usize, aligned: bool) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            if aligned {
                format!("{:<key_width$} ", tip.label)
            } else {
                format!("{} ", tip.label)
            },
            Style::default().fg(accent()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(tip.description, Style::default().fg(Color::Gray)),
    ]
}

fn section_lines(lines: &mut Vec<Line<'static>>, section: &'static Section, width: u16) {
    let key_width = key_width();
    let tips = sorted(section);
    lines.push(Line::default());
    lines.push(Line::styled(
        section.title,
        Style::default().fg(first()).add_modifier(Modifier::BOLD),
    ));
    let rule_style = Style::default().fg(separator());
    // The two-column layout depends on the actual data width: the body must fit two entries rendered with aligned key names plus a four-column gap.
    let two_column = usize::from(width) >= row_width() * 2 + 4;
    if two_column {
        let rows = tips.len().div_ceil(2);
        let column_width = usize::from((width - 4) / 2);
        lines.push(Line::styled(
            format!(
                "{}    {}",
                "─".repeat(column_width),
                "─".repeat(column_width)
            ),
            rule_style,
        ));
        for row in 0..rows {
            let mut spans = tip_spans(tips[row], key_width, true);
            let used = spans.iter().map(Span::width).sum::<usize>();
            if let Some(&tip) = tips.get(row + rows) {
                spans.push(Span::raw(" ".repeat(column_width.saturating_sub(used) + 4)));
                spans.extend(tip_spans(tip, key_width, true));
            }
            lines.push(Line::from(spans));
        }
    } else {
        lines.push(Line::styled("─".repeat(usize::from(width)), rule_style));
        lines.extend(
            tips.into_iter()
                .map(|tip| Line::from(tip_spans(tip, key_width, width >= 34))),
        );
    }
}

pub(super) fn lines(width: u16) -> Vec<Line<'static>> {
    let header = if width >= 54 {
        Line::from(vec![
            Span::styled("Tips:", Style::default().fg(accent())),
            Span::styled(
                "  All commands & shortcuts · /help for full usage",
                Style::default().fg(muted()),
            ),
        ])
    } else {
        Line::styled("Tips:", Style::default().fg(accent()))
    };
    let mut lines = vec![header];
    for section in crate::tips::SECTIONS {
        section_lines(&mut lines, section, width);
    }
    lines
}
