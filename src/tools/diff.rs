use crate::events::{DiffHunk, DiffLine, DiffLineKind, FileDiff};
use similar::{ChangeTag, TextDiff};

/// Builds the post-commit diff shown for edit/write from the content actually
/// passed to the tool. `before` is `None` when the target did not exist.
///
/// Line endings, BOM and the missing final newline are preserved verbatim;
/// EOL-only differences stay visible because each changed line keeps its
/// original terminator. Hunks carry three context lines like `git diff`.
pub(super) fn build(before: Option<&str>, after: &str) -> FileDiff {
    let diff = TextDiff::from_lines(before.unwrap_or(""), after);
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
                    old_line: change.old_index().map(|index| index + 1),
                    new_line: change.new_index().map(|index| index + 1),
                    text: change.value().to_owned(),
                });
            }
        }
        if !lines.is_empty() {
            hunks.push(DiffHunk { lines });
        }
    }
    FileDiff::Text {
        created: before.is_none(),
        added,
        removed,
        hunks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(hunk: &DiffHunk, index: usize) -> &DiffLine {
        &hunk.lines[index]
    }

    #[test]
    fn edit_hunk_carries_true_one_based_line_numbers_and_counts() {
        let diff = build(Some("one\ntwo\nthree\n"), "one\ntwo\nTHREE\n");
        let FileDiff::Text {
            created,
            added,
            removed,
            hunks,
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

    #[test]
    fn distant_changes_split_into_hunks_with_three_context_lines() {
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

    #[test]
    fn eol_bom_and_missing_final_newline_differences_stay_visible() {
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

    #[test]
    fn identical_created_and_empty_file_flags_match_commit_semantics() {
        // 完全相同的 write：无 hunk，显示层据此输出 No content changes.
        let diff = build(Some("same\n"), "same\n");
        let FileDiff::Text {
            created,
            added,
            removed,
            hunks,
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
