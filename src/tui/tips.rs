use super::statusline::ACCENT;
use crate::tips::{Section, Tip, key_width, row_width, sorted};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

fn tip_spans(tip: &Tip, key_width: usize, aligned: bool) -> Vec<Span<'static>> {
    vec![
        Span::styled(
            if aligned {
                format!("{:<key_width$} ", tip.label)
            } else {
                format!("{} ", tip.label)
            },
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
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
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));
    // 双列由实际数据宽度决定：正文宽度容得下两个按对齐键名渲染的条目再加四列间隔。
    let two_column = usize::from(width) >= row_width() * 2 + 4;
    if two_column {
        let rows = tips.len().div_ceil(2);
        let column_width = usize::from((width - 4) / 2);
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
        lines.extend(
            tips.into_iter()
                .map(|tip| Line::from(tip_spans(tip, key_width, width >= 34))),
        );
    }
}

pub(super) fn lines(width: u16) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled("Tips:", Style::default().fg(ACCENT))];
    for section in crate::tips::SECTIONS {
        section_lines(&mut lines, section, width);
    }
    lines
}
