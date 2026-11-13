//! `write` 的正文尾部窗口布局。

use super::*;

/// 三个写入阶段折叠时都保留末尾的屏幕正文行，省略提示另占一行。
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
    // 多取一行用于判断是否需要省略提示。
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
            style.fg(palette::MUTED),
        );
    }
    rows
}

fn write_line_tail(line: &str, width: u16, style: Style, limit: usize) -> Vec<Line<'static>> {
    let columns = usize::from(width.max(1));
    // 可打印 ASCII 每字节占一列，可以直接跳过窗口外的整行。
    if line.len() > limit * columns && line.bytes().all(|byte| matches!(byte, b' '..=b'~')) {
        let start = (line.len().div_ceil(columns) - limit) * columns;
        return wrap_block(block_line(line[start..].to_owned(), style), width, "");
    }

    // 宽字符和制表符仍按统一的换行规则计算，只保留窗口内的行。
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
