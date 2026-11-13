//! 表格布局：尾部行、网格排版与竖排回退。

use super::*;

pub(in crate::tui) fn table_tail_line(text: &str, width: u16, style: Style) -> Line<'static> {
    let text = expand_tabs(&display_text(text), TAB_STOP);
    let width = usize::from(width);
    if Line::raw(&text).width() <= width {
        return Line::styled(text, style);
    }
    let mut start = text.len();
    let mut remaining = width.saturating_sub(1);
    for (offset, grapheme) in text.grapheme_indices(true).rev() {
        let columns = Span::raw(grapheme).width();
        if columns > remaining {
            break;
        }
        start = offset;
        remaining -= columns;
    }
    Line::styled(format!("…{}", &text[start..]), style)
}

pub(super) fn layout_table(tb: &TableBlock, width: usize, out: &mut Vec<Line<'static>>) {
    let n = tb.aligns.len();
    if n == 0 {
        return;
    }
    // 每列左右各留一格，列间与外框各占一格。
    let frame_width = 3 * n + 1;
    let mut widths = vec![CELL_MIN_WIDTH; n];
    for (i, cell) in tb.header.iter().enumerate().take(n) {
        widths[i] = widths[i].max(spans_width(cell).max(CELL_MIN_WIDTH));
    }
    for row in &tb.rows {
        for (i, cell) in row.iter().enumerate().take(n) {
            widths[i] = widths[i].max(spans_width(cell).max(CELL_MIN_WIDTH));
        }
    }
    if CELL_MIN_WIDTH * n + frame_width > width {
        table_vertical_fallback(tb, n, width, out);
        return;
    }
    let mut excess = (widths.iter().sum::<usize>() + frame_width).saturating_sub(width);
    while excess > 0 {
        let Some(max) = widths.iter().copied().max().filter(|m| *m > CELL_MIN_WIDTH) else {
            break;
        };
        let group: Vec<usize> = (0..n).filter(|&i| widths[i] == max).collect();
        let next = widths
            .iter()
            .copied()
            .filter(|w| *w < max)
            .max()
            .unwrap_or(CELL_MIN_WIDTH)
            .max(CELL_MIN_WIDTH);
        let potential = group.len() * (max - next);
        if excess >= potential {
            for &i in &group {
                widths[i] = next;
            }
            excess -= potential;
        } else {
            let per = excess / group.len();
            let rem = excess % group.len();
            for (k, &i) in group.iter().enumerate() {
                widths[i] = max - per - usize::from(k < rem);
            }
            excess = 0;
        }
    }
    out.push(table_border(&widths, '┌', '┬', '┐'));
    render_table_row(&tb.header, &widths, &tb.aligns, out);
    for row in &tb.rows {
        out.push(table_border(&widths, '├', '┼', '┤'));
        render_table_row(row, &widths, &tb.aligns, out);
    }
    out.push(table_border(&widths, '└', '┴', '┘'));
}

fn table_border(widths: &[usize], left: char, middle: char, right: char) -> Line<'static> {
    let mut border = String::from(left);
    for (i, width) in widths.iter().enumerate() {
        if i > 0 {
            border.push(middle);
        }
        border.push_str(&"─".repeat(width + 2));
    }
    border.push(right);
    Line::from(Span::styled(border, Style::new().fg(MD_BORDER)))
}

fn render_table_row(
    cells: &[Cell],
    widths: &[usize],
    aligns: &[Alignment],
    out: &mut Vec<Line<'static>>,
) {
    let n = widths.len();
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = (0..n)
        .map(|i| {
            let empty: Cell = Vec::new();
            let cell = cells.get(i).unwrap_or(&empty);
            wrap(WrapMode::Char, cell, widths[i], widths[i])
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(0).max(1);
    for r in 0..height {
        let mut spans = vec![Span::styled("│", Style::new().fg(MD_BORDER))];
        for (i, w) in widths.iter().enumerate() {
            spans.push(Span::raw(" "));
            let mut line = wrapped[i].get(r).cloned().unwrap_or_default();
            apply_align(
                &mut line,
                *w,
                aligns.get(i).copied().unwrap_or(Alignment::None),
            );
            spans.append(&mut line);
            spans.push(Span::raw(" "));
            spans.push(Span::styled("│", Style::new().fg(MD_BORDER)));
        }
        out.push(Line::from(spans));
    }
}

fn apply_align(spans: &mut Vec<Span<'static>>, width: usize, align: Alignment) {
    let free = width.saturating_sub(spans.iter().map(Span::width).sum());
    if free == 0 {
        return;
    }
    match align {
        Alignment::Right => {
            let mut padded = vec![Span::raw(" ".repeat(free))];
            padded.append(spans);
            *spans = padded;
        }
        Alignment::Center => {
            let left = free / 2;
            let right = free - left;
            let mut padded = vec![Span::raw(" ".repeat(left))];
            padded.append(spans);
            padded.push(Span::raw(" ".repeat(right)));
            *spans = padded;
        }
        _ => spans.push(Span::raw(" ".repeat(free))),
    }
}

/// 连最小列宽、边距与完整外框都放不下时，按 `header: value` 竖排；
/// 空表头使用 `Column N`。
fn table_vertical_fallback(tb: &TableBlock, n: usize, width: usize, out: &mut Vec<Line<'static>>) {
    let labels: Vec<String> = (0..n)
        .map(|i| {
            let t: String = tb
                .header
                .get(i)
                .map(|c| c.iter().map(|s| s.content.as_ref()).collect())
                .unwrap_or_default();
            let t = t.trim();
            if t.is_empty() {
                format!("Column {}", i + 1)
            } else {
                t.to_string()
            }
        })
        .collect();
    for (ri, row) in tb.rows.iter().enumerate() {
        if ri > 0 {
            out.push(Line::from(""));
        }
        for (ci, cell) in row.iter().enumerate().take(n) {
            let mut content = vec![Span::styled(
                format!("{}: ", labels[ci]),
                Style::new().fg(MD_HEADING),
            )];
            let prefix_width = spans_width(&content);
            content.extend(cell.iter().cloned());
            let indent = if prefix_width < width {
                prefix_width
            } else {
                0
            };
            for (i, line) in wrap(WrapMode::Word, &content, width, width - indent)
                .into_iter()
                .enumerate()
            {
                let row_indent = indent.min(width.saturating_sub(spans_width(&line)));
                let mut spans = if i == 0 || row_indent == 0 {
                    Vec::new()
                } else {
                    vec![Span::raw(" ".repeat(row_indent))]
                };
                spans.extend(line);
                out.push(Line::from(spans));
            }
        }
    }
}
