//! 文件变更 diff 的排版；折叠额度与行号栏宽度与命令区共用。

use super::*;

fn diff_line_text(text: &str) -> (&str, &'static str) {
    if let Some(text) = text.strip_suffix("\r\n") {
        (text, "CRLF")
    } else if let Some(text) = text.strip_suffix('\n') {
        (text, "LF")
    } else {
        (text, "No newline")
    }
}

pub(in crate::tui) fn diff_layout(
    diff: &FileDiff,
    width: u16,
    expanded: bool,
) -> Vec<Line<'static>> {
    diff_layout_styled(diff, width, expanded, None)
}

pub(in crate::tui) fn diff_layout_styled(
    diff: &FileDiff,
    width: u16,
    expanded: bool,
    edit_path: Option<&str>,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let FileDiff::Text {
        created,
        deleted,
        added,
        removed,
        hunks,
    } = diff
    else {
        if let FileDiff::Unavailable { reason } = diff {
            return wrap_block(
                block_line(reason.clone(), Style::default().fg(palette::MUTED)),
                width,
                "",
            );
        }
        unreachable!();
    };
    if edit_path.is_none() || *deleted {
        rows.extend(wrap_block(
            Line::from(vec![
                Span::raw(if *deleted { "Deleted · " } else { "" }),
                Span::styled(format!("+{added}"), Style::default().fg(palette::ADDED)),
                Span::raw(" "),
                Span::styled(format!("-{removed}"), Style::default().fg(palette::REMOVED)),
            ]),
            width,
            "",
        ));
    }
    let summary_rows = rows.len();
    let total: usize = hunks.iter().map(|hunk| hunk.lines.len()).sum();
    if total == 0 {
        rows.extend(wrap_block(
            block_line(
                if *deleted {
                    "Deleted empty file."
                } else if *created {
                    "Created empty file."
                } else {
                    "No content changes."
                },
                Style::default().fg(palette::MUTED),
            ),
            width,
            "",
        ));
        return rows;
    }
    let max_number = hunks
        .iter()
        .flat_map(|hunk| &hunk.lines)
        .flat_map(|line| [line.old_line, line.new_line])
        .flatten()
        .max()
        .unwrap_or(0);
    let gutter = max_number.to_string().len().max(3);
    let continuation = " ".repeat(gutter + if edit_path.is_some() { 2 } else { 3 });
    let mut shown = 0;
    for (hunk_index, hunk) in hunks.iter().enumerate() {
        if !expanded
            && (shown >= tool_display::DIFF_LINES || hunk_index >= tool_display::DIFF_HUNKS)
        {
            break;
        }
        if hunk_index > 0 {
            rows.push(Line::styled("…", Style::default().fg(palette::DIM)));
        }
        let highlights = edit_path.map(|_| crate::tui::edit_diff::highlights(&hunk.lines));
        // 连续替换段中的行尾变化可能仅靠正文无法区分。
        let mut eol_hints = std::collections::HashSet::new();
        let mut start = 0;
        while start < hunk.lines.len() {
            if hunk.lines[start].kind == DiffLineKind::Context {
                start += 1;
                continue;
            }
            let end = (start..hunk.lines.len())
                .find(|&i| hunk.lines[i].kind == DiffLineKind::Context)
                .unwrap_or(hunk.lines.len());
            let mut endings: std::collections::HashMap<&str, (u8, u8)> =
                std::collections::HashMap::new();
            let ending_bit = |eol| match eol {
                "LF" => 1,
                "CRLF" => 2,
                _ => 4,
            };
            for line in &hunk.lines[start..end] {
                let (text, eol) = diff_line_text(&line.text);
                let entry = endings.entry(text).or_default();
                match line.kind {
                    DiffLineKind::Removed => entry.0 |= ending_bit(eol),
                    DiffLineKind::Added => entry.1 |= ending_bit(eol),
                    DiffLineKind::Context => {}
                }
            }
            for (offset, line) in hunk.lines[start..end].iter().enumerate() {
                let (text, eol) = diff_line_text(&line.text);
                let (old, new) = endings[text];
                let opposite = if line.kind == DiffLineKind::Added {
                    old
                } else {
                    new
                };
                if opposite & !ending_bit(eol) != 0 {
                    eol_hints.insert(start + offset);
                }
            }
            start = end;
        }
        for (index, line) in hunk.lines.iter().enumerate() {
            if !expanded && shown >= tool_display::DIFF_LINES {
                break;
            }
            shown += 1;
            let (text, eol) = diff_line_text(&line.text);
            let (mark, color, number) = match line.kind {
                DiffLineKind::Added => (
                    "+",
                    if edit_path.is_some() {
                        palette::EDIT_ADDED
                    } else {
                        palette::ADDED
                    },
                    line.new_line,
                ),
                DiffLineKind::Removed => (
                    "-",
                    if edit_path.is_some() {
                        palette::EDIT_REMOVED
                    } else {
                        palette::REMOVED
                    },
                    line.old_line,
                ),
                DiffLineKind::Context => (" ", palette::MUTED, line.new_line),
            };
            let number = number.map(|number| number.to_string()).unwrap_or_default();
            let mut spans = if let Some(path) = edit_path {
                let mut spans = vec![Span::styled(
                    format!("{mark}{number:>gutter$}│"),
                    Style::default()
                        .fg(if line.kind == DiffLineKind::Context {
                            palette::EDIT_META
                        } else {
                            color
                        })
                        .bg(palette::EDIT_BG),
                )];
                spans.extend(crate::tui::edit_diff::spans(
                    text,
                    path,
                    line.kind,
                    highlights.as_ref().and_then(|ranges| ranges.get(&index)),
                ));
                spans
            } else {
                vec![
                    Span::styled(
                        format!("{number:>gutter$} {mark} "),
                        Style::default().fg(palette::DIM),
                    ),
                    Span::styled(text.to_owned(), Style::default().fg(color)),
                ]
            };
            if text.starts_with('\u{feff}') {
                spans.push(Span::styled("  BOM", Style::default().fg(palette::DIM)));
            }
            if eol_hints.contains(&index) {
                spans.push(Span::styled(
                    format!("  {eol}"),
                    Style::default().fg(palette::DIM),
                ));
            }
            rows.extend(wrap_block(Line::from(spans), width, &continuation));
            if eol == "No newline" && line.kind != DiffLineKind::Context {
                rows.extend(wrap_block(
                    block_line(
                        "No newline at end of file",
                        Style::default().fg(palette::DIM),
                    ),
                    width,
                    &continuation,
                ));
            }
        }
    }
    // 第二道限制按实际换行后的屏幕行计算，统计行不占 diff 正文额度。
    let mut visual_hidden = 0;
    let visual_limit = summary_rows + tool_display::DIFF_LINES;
    if !expanded && rows.len() > visual_limit {
        visual_hidden = rows.len() - visual_limit;
        rows.truncate(visual_limit);
    }
    if shown < total || visual_hidden > 0 {
        rows.push(Line::styled(
            tool_display::clip(
                &format!(
                    "… {} more lines (Ctrl + O to expand)",
                    total - shown + visual_hidden
                ),
                usize::from(width),
                true,
            ),
            Style::default().fg(palette::DIM),
        ));
    }
    rows
}
