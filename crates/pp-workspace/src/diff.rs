use pp_core::events::DiffHunk;
use pp_core::events::DiffLine;
use pp_core::events::DiffLineKind;
use pp_core::events::FileDiff;
use similar::{ChangeTag, TextDiff};

/// Builds the post-commit diff shown for edit/write from the content actually
/// passed to the tool. `before` is `None` when the target did not exist.
///
/// Line endings, BOM and the missing final newline are preserved verbatim;
/// EOL-only differences stay visible because each changed line keeps its
/// original terminator. Hunks carry three context lines like `git diff`.
fn trim_common_lines<'a>(old: &'a str, after: &'a str) -> (&'a str, &'a str, usize) {
    let ob = old.as_bytes();
    let ab = after.as_bytes();
    let prefix = ob
        .iter()
        .zip(ab.iter())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| ob.len().min(ab.len()));
    let prefix_line_end = ob[..prefix]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |i| i + 1);
    let mut win_start = prefix_line_end;
    for _ in 0..4 {
        if win_start == 0 {
            break;
        }
        win_start = ob[..win_start - 1]
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |i| i + 1);
    }
    let skipped_lines = ob[..win_start].iter().filter(|&&b| b == b'\n').count();

    let old_rem = &ob[win_start..];
    let after_rem = &ab[win_start..];
    let suffix = old_rem
        .iter()
        .rev()
        .zip(after_rem.iter().rev())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| old_rem.len().min(after_rem.len()));
    let suffix_from_end = if suffix == 0 {
        0
    } else {
        let old_common_start = old_rem.len() - suffix;
        // 对齐到公共后缀内的第一个完整行首，再向后放宽 4 行上下文缓冲。
        let first_nl = old_rem[old_common_start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(old_rem.len(), |i| old_common_start + i + 1);
        let mut keep_start = first_nl;
        for _ in 0..4 {
            if keep_start >= old_rem.len() {
                break;
            }
            keep_start = match old_rem[keep_start..].iter().position(|&b| b == b'\n') {
                Some(i) => keep_start + i + 1,
                None => old_rem.len(),
            };
        }
        old_rem.len() - keep_start
    };
    let old_end = old.len() - suffix_from_end;
    let after_end = after.len() - suffix_from_end;
    (
        &old[win_start..old_end],
        &after[win_start..after_end],
        skipped_lines,
    )
}

pub fn build(before: Option<&str>, after: &str) -> FileDiff {
    if before == Some(after) {
        return FileDiff::Text {
            deleted: false,
            created: false,
            added: 0,
            removed: 0,
            hunks: Vec::new(),
        };
    }
    let (old_slice, new_slice, line_offset) = match before {
        Some(old) => trim_common_lines(old, after),
        None => ("", after, 0),
    };
    let diff = TextDiff::from_lines(old_slice, new_slice);
    let mut added = 0;
    let mut removed = 0;
    let mut hunks = Vec::new();
    for group in diff.grouped_ops(3) {
        let mut lines = Vec::new();
        for op in &group {
            for change in diff.iter_changes(op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => DiffLineKind::Context,
                    ChangeTag::Delete => {
                        removed += 1;
                        DiffLineKind::Removed
                    }
                    ChangeTag::Insert => {
                        added += 1;
                        DiffLineKind::Added
                    }
                };
                lines.push(DiffLine {
                    kind,
                    // `Change` indices are zero-based; the wire format is 1-based.
                    old_line: change.old_index().map(|index| line_offset + index + 1),
                    new_line: change.new_index().map(|index| line_offset + index + 1),
                    text: change.value().to_owned(),
                });
            }
        }
        if !lines.is_empty() {
            hunks.push(DiffHunk { lines });
        }
    }
    FileDiff::Text {
        deleted: false,
        created: before.is_none(),
        added,
        removed,
        hunks,
    }
}

#[derive(Debug)]
pub enum Original {
    Missing,
    Bytes(Vec<u8>),
    Unavailable,
}

/// 从真实提交前后的存在性与字节构造 diff；删除空文件也有独立的删除标记。
pub fn committed(before: &Original, after: Option<&[u8]>) -> FileDiff {
    let before = match before {
        Original::Missing => None,
        Original::Bytes(bytes) => match std::str::from_utf8(bytes) {
            Ok(text) => Some(text),
            Err(_) => {
                return FileDiff::Unavailable {
                    reason: "Previous content is not UTF-8; text diff unavailable.".into(),
                };
            }
        },
        Original::Unavailable => {
            return FileDiff::Unavailable {
                reason: "Previous content could not be read; text diff unavailable.".into(),
            };
        }
    };
    let after_text = match after.map(std::str::from_utf8).transpose() {
        Ok(text) => text.unwrap_or(""),
        Err(_) => {
            return FileDiff::Unavailable {
                reason: "Latest content is not UTF-8; text diff unavailable.".into(),
            };
        }
    };
    let mut result = build(before, after_text);
    if let FileDiff::Text { deleted, .. } = &mut result {
        *deleted = after.is_none();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(hunk: &DiffHunk, index: usize) -> &DiffLine {
        &hunk.lines[index]
    }

    #[test]
    fn hunk_line_numbers_and_splitting_preserve_context() {
        {
            let diff = build(Some("one\ntwo\nthree\n"), "one\ntwo\nTHREE\n");
            let FileDiff::Text {
                created,
                added,
                removed,
                hunks,
                ..
            } = diff
            else {
                panic!("text diff expected");
            };
            assert!(!created);
            assert_eq!((added, removed), (1, 1));
            assert_eq!(hunks.len(), 1);
            let hunk = &hunks[0];
            assert_eq!(line(hunk, 0).kind, DiffLineKind::Context);
            assert_eq!(line(hunk, 0).old_line, Some(1));
            assert_eq!(line(hunk, 0).new_line, Some(1));
            assert_eq!(line(hunk, 0).text, "one\n");
            assert_eq!(line(hunk, 2).kind, DiffLineKind::Removed);
            assert_eq!(line(hunk, 2).old_line, Some(3));
            assert_eq!(line(hunk, 2).new_line, None);
            assert_eq!(line(hunk, 2).text, "three\n");
            assert_eq!(line(hunk, 3).kind, DiffLineKind::Added);
            assert_eq!(line(hunk, 3).old_line, None);
            assert_eq!(line(hunk, 3).new_line, Some(3));
            assert_eq!(line(hunk, 3).text, "THREE\n");
            assert_eq!(hunk.lines.len(), 4);
        }
        {
            let before = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\n";
            let after = "L1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nL10\n";
            let diff = build(Some(before), after);
            let FileDiff::Text {
                added,
                removed,
                hunks,
                ..
            } = diff
            else {
                panic!("text diff expected");
            };
            assert_eq!((added, removed), (2, 2));
            assert_eq!(hunks.len(), 2);
            // 前导 context 截到最后三行，变化行带真实行号。
            let first = &hunks[0];
            assert_eq!(first.lines.len(), 5);
            assert_eq!(first.lines[0].kind, DiffLineKind::Removed);
            assert_eq!(first.lines[0].old_line, Some(1));
            assert_eq!(first.lines[1].text, "L1\n");
            assert_eq!(first.lines[2].new_line, Some(2));
            assert_eq!(first.lines[4].new_line, Some(4));
            // 内部 6 行以内的相同段保留在一个 hunk 中，超过则拆分。
            let second = &hunks[1];
            assert_eq!(second.lines.len(), 5);
            assert_eq!(second.lines[0].new_line, Some(7));
            assert_eq!(second.lines[3].kind, DiffLineKind::Removed);
            assert_eq!(second.lines[3].old_line, Some(10));
            assert_eq!(second.lines[4].text, "L10\n");
        }
    }

    #[test]
    fn eol_bom_missing_newline_and_commit_flags() {
        {
            // CRLF -> LF 是真实差异，不抹掉。
            let diff = build(Some("a\r\nb\r\n"), "a\nb\r\n");
            let FileDiff::Text { hunks, .. } = diff else {
                panic!("text diff expected");
            };
            assert_eq!(hunks.len(), 1);
            assert_eq!(hunks[0].lines.len(), 3);
            assert_eq!(hunks[0].lines[0].text, "a\r\n");
            assert_eq!(hunks[0].lines[1].text, "a\n");
            assert_eq!(hunks[0].lines[2].text, "b\r\n");

            // BOM 属于首行内容，不规范化。
            let diff = build(Some("\u{feff}x\n"), "x\n");
            let FileDiff::Text { hunks, .. } = diff else {
                panic!("text diff expected");
            };
            assert_eq!(hunks[0].lines[0].text, "\u{feff}x\n");
            assert_eq!(hunks[0].lines[1].text, "x\n");

            // 末行缺少换行符时保留原始形态，显示层据此提示。
            let diff = build(Some("a\nb"), "a\nc");
            let FileDiff::Text { hunks, .. } = diff else {
                panic!("text diff expected");
            };
            assert_eq!(hunks[0].lines.len(), 3);
            assert_eq!(hunks[0].lines[1].kind, DiffLineKind::Removed);
            assert_eq!(hunks[0].lines[1].text, "b");
            assert_eq!(hunks[0].lines[2].kind, DiffLineKind::Added);
            assert_eq!(hunks[0].lines[2].text, "c");
        }
        {
            // 完全相同的 write：无 hunk，显示层据此输出 No content changes.
            let diff = build(Some("same\n"), "same\n");
            let FileDiff::Text {
                created,
                added,
                removed,
                hunks,
                ..
            } = diff
            else {
                panic!("text diff expected");
            };
            assert!(!created);
            assert_eq!((added, removed, hunks.len()), (0, 0, 0));

            // 新建空文件：created=true 且无 hunk，显示层据此输出 Created empty file.
            let diff = build(None, "");
            let FileDiff::Text {
                created,
                added,
                removed,
                hunks,
                ..
            } = diff
            else {
                panic!("text diff expected");
            };
            assert!(created);
            assert_eq!((added, removed, hunks.len()), (0, 0, 0));

            // 已存在的空文件不算新建。
            let diff = build(Some(""), "x");
            let FileDiff::Text {
                created,
                added,
                removed,
                hunks,
                ..
            } = diff
            else {
                panic!("text diff expected");
            };
            assert!(!created);
            assert_eq!((added, removed), (1, 0));
            assert_eq!(hunks[0].lines[0].text, "x");
            assert_eq!(hunks[0].lines[0].new_line, Some(1));
        }
    }
}
