//! Tail-window body layout of `write`.

use super::*;

/// All three write phases keep the trailing screen body lines when folded, with the omission hint on a line of its own.
pub(in crate::tui) fn write_body(
    content: &str,
    width: u16,
    expanded: bool,
    style: Style,
) -> Vec<Line<'static>> {
    if expanded {
        return wrap(
            logical(content).into_iter().map(str::to_owned),
            width,
            style,
        );
    }
    if content.is_empty() {
        return Vec::new();
    }
    let content = content.strip_suffix('\n').unwrap_or(content);
    let mut source = content.split('\n');
    let mut rows = Vec::new();
    // Read one extra line to decide whether the omission hint is needed.
    while rows.len() <= WRITE_LINES {
        let Some(line) = source.next_back() else {
            break;
        };
        let mut wrapped = write_line_tail(line, width, style, WRITE_LINES + 1 - rows.len());
        wrapped.append(&mut rows);
        rows = wrapped;
    }
    if rows.len() > WRITE_LINES {
        rows[0] = Line::styled(
            clip(
                "… more lines (Ctrl + O to expand)",
                usize::from(width),
                true,
            ),
            style.fg(palette::muted()),
        );
    }
    rows
}

fn write_line_tail(line: &str, width: u16, style: Style, limit: usize) -> Vec<Line<'static>> {
    let columns = usize::from(width.max(1));
    // Every printable ASCII byte takes one column, so whole lines outside the window can be skipped.
    if line.len() > limit * columns && line.bytes().all(|byte| matches!(byte, b' '..=b'~')) {
        let start = (line.len().div_ceil(columns) - limit) * columns;
        return wrap_block(block_line(line[start..].to_owned(), style), width, "");
    }

    // Wide characters and tabs still go through the shared wrapping rules and only keep the lines inside the window.
    let mut rows = VecDeque::new();
    visit_wrapped_rows(block_line(line.to_owned(), style), width, "", |row| {
        if rows.len() == limit {
            rows.pop_front();
        }
        rows.push_back(row);
        true
    });
    rows.into()
}
