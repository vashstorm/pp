//! Position conversion: UTF-16 column → byte offset, plus the range used for whole-document replacement.

use serde_json::{Value, json};

use crate::types::Position;

/// Range used for whole-document replacement: covers the last line and column of the old content.
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

/// Position conversion: split from the line start by UTF-16 column.
///
/// `None` means the line does not exist, the column exceeds that line, or the column falls inside a surrogate pair: the caller must
/// reject this position rather than guess a neighbouring one. A CRLF's `\r` is not line content.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_client_position_offsets_are_strict_utf16() {
        let text = "a\u{1f600}b\r\nxy\n";
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 0,
                    character: 0
                }
            ),
            Some(0)
        );
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 0,
                    character: 1
                }
            ),
            Some(1)
        );
        // The column falls inside a surrogate pair: reject, do not snap to a neighbouring character.
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 0,
                    character: 2
                }
            ),
            None
        );
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 0,
                    character: 3
                }
            ),
            Some(5)
        );
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 0,
                    character: 4
                }
            ),
            Some(6)
        );
        // A CRLF's \r is not line content.
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 0,
                    character: 5
                }
            ),
            None
        );
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 1,
                    character: 2
                }
            ),
            Some(10)
        );
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 2,
                    character: 0
                }
            ),
            Some(11)
        );
        assert_eq!(
            position_offset(
                text,
                Position {
                    line: 3,
                    character: 0
                }
            ),
            None
        );
    }
}
