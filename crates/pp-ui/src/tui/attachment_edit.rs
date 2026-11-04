//! 附件标记沿用文本布局，但所有编辑边界必须落在标记之外。
use super::resources::Bindings;
use crossterm::event::KeyEvent;
use ratatui_textarea::{CursorMove, DataCursor, TextArea};

pub(super) const ICON: &str = "\u{f03e}";
pub(super) const COLOR: ratatui::style::Color = ratatui::style::Color::Rgb(255, 175, 95);

/// 返回旧文本的替换区间及新文本替换区间的终点，全部使用 UTF-8 字节位置。
pub(super) fn changed_range(before: &str, after: &str) -> (usize, usize, usize) {
    let start = before
        .chars()
        .zip(after.chars())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    let suffix = before[start..]
        .chars()
        .rev()
        .zip(after[start..].chars().rev())
        .take_while(|(a, b)| a == b)
        .map(|(c, _)| c.len_utf8())
        .sum::<usize>();
    (start, before.len() - suffix, after.len() - suffix)
}

/// 相邻文字相同时最短 diff 可能把插入位置移到另一侧，优先使用真实光标和选区。
pub(super) fn edited_range(
    before: &str,
    after: &str,
    old_cursor: usize,
    new_cursor: usize,
    selection: Option<std::ops::Range<usize>>,
) -> (usize, usize, usize) {
    let valid = |start: usize, end: usize| {
        let inserted_end = end.checked_add_signed(after.len() as isize - before.len() as isize)?;
        (start <= inserted_end
            && before.get(..start) == after.get(..start)
            && before.get(end..) == after.get(inserted_end..))
        .then_some((start, end, inserted_end))
    };
    if let Some(range) = selection
        && let Some(range) = valid(range.start, range.end)
    {
        return range;
    }
    if before.len() <= after.len() && new_cursor >= old_cursor {
        if let Some(range) = valid(old_cursor, old_cursor) {
            return range;
        }
    } else if before.len() > after.len()
        && let Some(range) = valid(new_cursor, new_cursor + before.len() - after.len())
    {
        return range;
    }
    changed_range(before, after)
}

pub(super) fn offset(input: &TextArea<'_>, cursor: DataCursor) -> usize {
    input
        .lines()
        .iter()
        .take(cursor.0)
        .map(|line| line.len() + 1)
        .sum::<usize>()
        + input.lines()[cursor.0]
            .chars()
            .take(cursor.1)
            .map(char::len_utf8)
            .sum::<usize>()
}

pub(super) fn jump(input: &mut TextArea<'_>, byte: usize) {
    let mut remaining = byte;
    for (row, line) in input.lines().iter().enumerate() {
        if remaining <= line.len() {
            let col = line[..remaining].chars().count();
            input.move_cursor(CursorMove::Jump(
                row.min(u16::MAX as usize) as u16,
                col.min(u16::MAX as usize) as u16,
            ));
            return;
        }
        remaining -= line.len() + 1;
    }
    input.move_cursor(CursorMove::Bottom);
    input.move_cursor(CursorMove::End);
}

pub(super) fn normalize(input: &mut TextArea<'_>, bindings: &Bindings, previous: usize) {
    let cursor = offset(input, input.cursor());
    if let Some(((a, b), (c, d))) = input.selection_range() {
        let start = offset(input, DataCursor(a, b));
        let end = offset(input, DataCursor(c, d));
        if start != end {
            let mut expanded = start..end;
            for range in bindings.values() {
                if range.start < end && range.end > start {
                    expanded.start = expanded.start.min(range.start);
                    expanded.end = expanded.end.max(range.end);
                }
            }
            if expanded != (start..end) {
                input.cancel_selection();
                let backwards = cursor == start;
                jump(
                    input,
                    if backwards {
                        expanded.end
                    } else {
                        expanded.start
                    },
                );
                input.start_selection();
                jump(
                    input,
                    if backwards {
                        expanded.start
                    } else {
                        expanded.end
                    },
                );
            }
            return;
        }
    }
    if let Some(range) = bindings
        .values()
        .find(|range| range.start < cursor && cursor < range.end)
    {
        jump(
            input,
            if cursor > previous {
                range.end
            } else {
                range.start
            },
        );
    }
}

pub(super) fn key(input: &mut TextArea<'static>, key: KeyEvent, bindings: &Bindings) -> bool {
    if bindings.is_empty() {
        return input.input(key);
    }
    let before = input.lines().join("\n");
    let previous = offset(input, input.cursor());
    normalize(input, bindings, previous);
    let selection = input
        .selection_range()
        .map(|((a, b), (c, d))| offset(input, DataCursor(a, b))..offset(input, DataCursor(c, d)));
    // 在副本上得到编辑器原生的词删除、选区替换等范围，再扩展到完整附件。
    let mut probe = input.clone();
    let changed = probe.input(key);
    let after = probe.lines().join("\n");
    if before == after {
        input.input(key);
        normalize(input, bindings, previous);
        return changed;
    }
    let (start, end, inserted_end) = edited_range(
        &before,
        &after,
        previous,
        offset(&probe, probe.cursor()),
        selection,
    );
    let mut expanded = start..end;
    for range in bindings.values() {
        if range.start < end && range.end > start {
            expanded.start = expanded.start.min(range.start);
            expanded.end = expanded.end.max(range.end);
        }
    }
    if expanded == (start..end) {
        return input.input(key);
    }
    input.cancel_selection();
    jump(input, expanded.start);
    input.delete_str(before[expanded].chars().count());
    if start < inserted_end {
        input.insert_str(&after[start..inserted_end]);
    }
    true
}
