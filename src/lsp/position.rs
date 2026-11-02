//! 位置换算：UTF-16 列 → 字节偏移，以及全量替换使用的范围。

use serde_json::{Value, json};

use crate::lsp::types::Position;

/// 全量替换用的范围：覆盖旧内容的最后一行末列。
pub(super) fn whole_range(previous: &str) -> Value {
    let mut line = 0u32;
    let mut character = 0u32;
    for ch in previous.chars() {
        if ch == '\n' {
            line += 1;
            character = 0;
        } else {
            character += ch.len_utf16() as u32;
        }
    }
    json!({
        "start": {"line": 0, "character": 0},
        "end": {"line": line, "character": character},
    })
}

/// 位置换算：按 UTF-16 列从行首切分。
///
/// 返回 `None` 表示行不存在、列超出该行，或列落在代理对中间：调用方必须
/// 拒绝这个位置，不能猜测相邻位置。CRLF 的 `\r` 不属于行内容。
pub(crate) fn position_offset(text: &str, position: Position) -> Option<usize> {
    let mut line_start = 0;
    for _ in 0..position.line {
        let index = text[line_start..].find('\n')? + line_start;
        line_start = index + 1;
    }
    let mut line_end = text[line_start..]
        .find('\n')
        .map(|offset| line_start + offset)
        .unwrap_or(text.len());
    if line_end > line_start && text.as_bytes()[line_end - 1] == b'\r' {
        line_end -= 1;
    }
    let mut units = 0u32;
    for (offset, ch) in text[line_start..line_end].char_indices() {
        if units == position.character {
            return Some(line_start + offset);
        }
        units = units.checked_add(ch.len_utf16() as u32)?;
        if units > position.character {
            return None;
        }
    }
    (units == position.character).then_some(line_end)
}
