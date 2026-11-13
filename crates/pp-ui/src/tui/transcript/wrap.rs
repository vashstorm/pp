//! 文本行换行：普通块的样式化与按屏幕行访问。

use super::*;

pub(in crate::tui) fn block_line(
    text: impl Into<std::borrow::Cow<'static, str>>,
    style: Style,
) -> Line<'static> {
    Line::from(Span::styled(text, style))
}

/// Wrap styled graphemes once; each output is one clipped terminal row.
pub(in crate::tui) fn wrap_block(
    line: Line<'static>,
    width: u16,
    continuation: &str,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    visit_wrapped_rows(line, width, continuation, |row| {
        rows.push(row);
        true
    });
    rows
}

pub(in crate::tui) fn visit_wrapped_rows(
    line: Line<'static>,
    width: u16,
    continuation: &str,
    mut visit: impl FnMut(Line<'static>) -> bool,
) {
    let width = usize::from(width.max(1));
    let mut emitted = false;
    let mut spans = Vec::new();
    let mut used = 0;
    let mut has_content = false;
    let mut ended_newline = false;
    for span in line.spans {
        let safe = pp_core::text::display_text(&span.content);
        let style = line.style.patch(span.style);
        for part in safe.split_inclusive(['\n', '\t']) {
            let (plain, separator) = if let Some(text) = part.strip_suffix('\n') {
                (text, Some("\n"))
            } else if let Some(text) = part.strip_suffix('\t') {
                (text, Some("\t"))
            } else {
                (part, None)
            };
            let text = Span::raw(plain);
            for (symbol, style) in text
                .styled_graphemes(style)
                .map(|grapheme| (grapheme.symbol, grapheme.style))
                .chain(separator.map(|symbol| (symbol, style)))
            {
                if symbol == "\n" {
                    if !visit(Line::from(std::mem::take(&mut spans))) {
                        return;
                    }
                    emitted = true;
                    used = 0;
                    has_content = false;
                    ended_newline = true;
                    continue;
                }
                ended_newline = false;
                let repeats = if symbol == "\t" { 4 - used % 4 } else { 1 };
                let value = if symbol == "\t" { " " } else { symbol };
                let columns = Line::raw(value).width();
                for _ in 0..repeats {
                    if has_content && used + columns > width {
                        if !visit(Line::from(std::mem::take(&mut spans))) {
                            return;
                        }
                        emitted = true;
                        let prefix = if Line::raw(continuation).width() + columns <= width {
                            continuation
                        } else {
                            ""
                        };
                        if !prefix.is_empty() {
                            spans.push(Span::styled(
                                prefix.to_owned(),
                                Style::default().fg(palette::DIM),
                            ));
                        }
                        used = Line::raw(prefix).width();
                    }
                    used += columns;
                    has_content = true;
                    if let Some(previous) =
                        spans.last_mut().filter(|previous| previous.style == style)
                    {
                        previous.content.to_mut().push_str(value);
                    } else {
                        spans.push(Span::styled(value.to_owned(), style));
                    }
                }
            }
        }
    }
    if !spans.is_empty() || !emitted || ended_newline {
        visit(Line::from(spans));
    }
}
