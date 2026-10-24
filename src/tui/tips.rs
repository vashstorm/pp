use super::statusline::ACCENT;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

fn tip_spans(tip: &'static str, aligned: bool) -> Vec<Span<'static>> {
    let (key, description) = tip.split_once(' ').unwrap_or((tip, ""));
    vec![
        Span::styled(
            if aligned {
                format!("{key:<12}")
            } else {
                format!("{key} ")
            },
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(description, Style::default().fg(Color::Gray)),
    ]
}

pub(super) fn lines(width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled("Tips:", Style::default().fg(ACCENT))];
    let tips = crate::tips::TIPS;
    if width >= 76 {
        let rows = tips.len().div_ceil(2);
        let column_width = usize::from((width - 4) / 2);
        for row in 0..rows {
            let mut spans = tip_spans(tips[row], true);
            let used = spans.iter().map(Span::width).sum::<usize>();
            if let Some(&tip) = tips.get(row + rows) {
                spans.push(Span::raw(" ".repeat(column_width.saturating_sub(used) + 4)));
                spans.extend(tip_spans(tip, true));
            }
            lines.push(Line::from(spans));
        }
    } else {
        lines.extend(
            tips.iter()
                .map(|&tip| Line::from(tip_spans(tip, width >= 34))),
        );
    }
    lines
}
