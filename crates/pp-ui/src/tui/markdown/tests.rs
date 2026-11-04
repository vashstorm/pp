use super::*;
use crate::tui::transcript_style::{EDIT_NUMBER, EDIT_STRING, SYNTAX_KEYWORD};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::{Paragraph, Widget};

fn layout(src: &str, width: u16) -> Vec<Line<'static>> {
    parse(src).layout(width)
}

fn text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

fn texts(lines: &[Line<'static>]) -> Vec<String> {
    lines.iter().map(text).collect()
}

fn fg(line: &Line<'_>, idx: usize) -> Option<Color> {
    line.spans.get(idx).and_then(|s| s.style.fg)
}

fn has_modifier(line: &Line<'_>, idx: usize, m: Modifier) -> bool {
    line.spans
        .get(idx)
        .is_some_and(|s| s.style.add_modifier.contains(m))
}

/// 首个包含 `needle` 的 span 的前景色，用于语义高亮断言。
fn span_fg(line: &Line<'_>, needle: &str) -> Option<Color> {
    line.spans
        .iter()
        .find(|span| span.content.contains(needle))
        .and_then(|span| span.style.fg)
}

#[test]
fn heading_strips_marker_and_uses_accent_bold() {
    let ls = layout("# Title", 40);
    assert_eq!(texts(&ls), vec!["Title"]);
    assert_eq!(fg(&ls[0], 0), Some(ACCENT));
    assert!(has_modifier(&ls[0], 0, Modifier::BOLD));
    let setext = layout("Title\n=====", 40);
    assert_eq!(texts(&setext), vec!["Title"]);
    assert_eq!(fg(&setext[0], 0), Some(ACCENT));
}

#[test]
fn heading_anchors_follow_wrapped_blocks_and_ignore_fenced_examples() {
    let source = "# **Overview**\n\nA long paragraph that wraps over several terminal lines.\n\n```md\n# Not a heading\n```\n\n## 重复章节 `code`\n\n| Column | Value |\n| --- | --- |\n| long content | more content |\n\n重复章节\n--------\n\n### Final\n";
    let document = parse(source);
    for width in [8, 25, 80] {
        let (lines, headings) = document.layout_with_headings(width);
        assert_eq!(lines, document.layout(width));
        assert_eq!(headings.len(), 4);
        assert_eq!(
            headings
                .iter()
                .map(|heading| heading.level)
                .collect::<Vec<_>>(),
            [1, 2, 2, 3]
        );
        assert_eq!(headings[0].title, "Overview");
        assert_eq!(headings[1].title, "重复章节 code");
        assert_eq!(headings[2].title, "重复章节");
        for heading in &headings {
            assert!(heading.title.starts_with(&text(&lines[heading.row])));
        }
        assert!(headings.windows(2).all(|pair| pair[0].row < pair[1].row));
    }
    let (lines, headings) = document.layout_with_headings(0);
    assert!(lines.is_empty() && headings.is_empty());
}

#[test]
fn inline_styles_use_modifiers_and_palette() {
    let ls = layout("**bold** and *em* and ~~gone~~ and `code`", 80);
    assert_eq!(
        texts(&ls),
        vec!["bold and em and gone and code".to_string()]
    );
    let spans = &ls[0].spans;
    let contents: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(
        contents,
        vec!["bold", " and ", "em", " and ", "gone", " and ", "code"]
    );
    assert!(has_modifier(&ls[0], 0, Modifier::BOLD));
    assert!(has_modifier(&ls[0], 2, Modifier::ITALIC));
    assert!(has_modifier(&ls[0], 4, Modifier::CROSSED_OUT));
    assert_eq!(fg(&ls[0], 6), Some(CODE));
    assert_eq!(fg(&ls[0], 1), None);
}

#[test]
fn link_shows_label_and_dim_url_only_when_different() {
    let ls = layout("see [docs](http://e.com/x) now", 80);
    let contents: Vec<String> = ls[0].spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(contents, vec!["see ", "docs", " (http://e.com/x)", " now"]);
    assert_eq!(fg(&ls[0], 1), Some(LINK));
    assert_eq!(fg(&ls[0], 2), Some(DIM));
    let same = layout("[http://e.com](http://e.com)", 80);
    assert_eq!(texts(&same), vec!["http://e.com"]);
    assert_eq!(same[0].spans.len(), 1);
    assert_eq!(fg(&same[0], 0), Some(LINK));
}

#[test]
fn image_renders_as_alt_and_url() {
    let ls = layout("![alt text](http://i.io/p.png)", 80);
    let contents: Vec<String> = ls[0].spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(contents, vec!["Image: alt text", " (http://i.io/p.png)"]);
    assert_eq!(fg(&ls[0], 1), Some(DIM));
}

#[test]
fn closed_code_block_renders_frame_with_language() {
    let ls = layout("```rust ignore_me\nlet x = 1;\n```\n", 40);
    assert_eq!(ls.len(), 3);
    assert!(text(&ls[0]).starts_with("── rust "));
    assert!(text(&ls[0]).ends_with('─'));
    assert_eq!(Span::raw(text(&ls[0])).width(), 40);
    assert_eq!(text(&ls[1]), "let x = 1;");
    assert_eq!(span_fg(&ls[1], "let"), Some(SYNTAX_KEYWORD));
    assert_eq!(span_fg(&ls[1], "="), Some(SYNTAX_KEYWORD));
    assert_eq!(span_fg(&ls[1], "1"), Some(EDIT_NUMBER));
    assert_eq!(span_fg(&ls[1], " x "), Some(CODE_BLOCK));
    assert_eq!(text(&ls[2]), "─".repeat(40));
    assert_eq!(fg(&ls[2], 0), Some(DIM));
}

#[test]
fn code_block_without_or_unknown_language_falls_back_to_plain_color() {
    let plain = layout("```\nlet x = 1;\n```\n", 40);
    assert_eq!(text(&plain[1]), "let x = 1;");
    assert_eq!(span_fg(&plain[1], "let"), Some(CODE_BLOCK));
    let unknown = layout("```notalang\nlet x = 1;\n```\n", 40);
    assert_eq!(text(&unknown[1]), "let x = 1;");
    assert_eq!(span_fg(&unknown[1], "let"), Some(CODE_BLOCK));
}

#[test]
fn highlighted_code_wraps_and_keeps_span_colors() {
    let ls = layout("```rust\nlet xy = 1;\n```\n", 9);
    assert_eq!(
        texts(&ls),
        vec!["── rust ─", "let xy = ", "1;", "─────────"]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    );
    assert_eq!(span_fg(&ls[1], "let"), Some(SYNTAX_KEYWORD));
    assert_eq!(span_fg(&ls[1], " xy "), Some(CODE_BLOCK));
    assert_eq!(span_fg(&ls[2], "1"), Some(EDIT_NUMBER));
    assert_eq!(span_fg(&ls[2], ";"), Some(CODE_BLOCK));
    assert!(ls.iter().all(|line| line.width() <= 9));
}

#[test]
fn highlighted_code_with_tabs_cjk_and_emoji_survives() {
    let ls = layout("```rust\n\tlet s = \"中文🙂\";\n```\n", 40);
    assert_eq!(text(&ls[1]), "    let s = \"中文🙂\";");
    assert_eq!(span_fg(&ls[1], "let"), Some(SYNTAX_KEYWORD));
    assert_eq!(span_fg(&ls[1], "中文🙂"), Some(EDIT_STRING));
}

#[test]
fn highlighted_code_preserves_combining_graphemes_in_buffer() {
    let code = "function e\u{301}() {}";
    for closed in [false, true] {
        let source = format!(
            "```javascript\n{code}\n{}",
            if closed { "```\n" } else { "" }
        );
        for width in [10, 11, 40] {
            let lines = layout(&source, width);
            let body_end = lines.len() - usize::from(closed);
            let area = Rect::new(0, 0, width, lines.len() as u16);
            let mut buffer = Buffer::empty(area);
            Paragraph::new(lines).render(area, &mut buffer);
            let mut rendered = String::new();
            for y in 1..body_end as u16 {
                let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
                // 仅去除最后一行的填充，保留换行前代码自身的空格。
                if y + 1 == body_end as u16 {
                    rendered.push_str(row.trim_end());
                } else {
                    rendered.push_str(&row);
                }
            }
            assert_eq!(rendered, code, "width={width}, closed={closed}");
            let accent_x = 9 % width;
            let accent_y = 1 + 9 / width;
            assert_eq!(buffer[(accent_x, accent_y)].symbol(), "e\u{301}");
        }
    }
}

#[test]
fn oversized_code_block_falls_back_to_plain_color() {
    let src = format!("```rust\n{}\n```\n", "let x = 1;\n".repeat(500));
    let ls = layout(&src, 40);
    // 500 行内容 + 1 空行 + 顶/底框，全部回退纯色。
    assert_eq!(ls.len(), 503);
    for line in &ls[1..501] {
        assert_eq!(span_fg(line, "let"), Some(CODE_BLOCK));
    }
}

#[test]
fn unclosed_code_block_stays_open_for_streaming() {
    let ls = layout("```rust\nlet x = 1;\n", 40);
    assert_eq!(ls.len(), 2);
    assert!(text(&ls[0]).starts_with("── rust "));
    assert_eq!(text(&ls[1]), "let x = 1;");
    // 流式未闭合块已到达内容即时着色，且没有闭合底线。
    assert_eq!(span_fg(&ls[1], "let"), Some(SYNTAX_KEYWORD));
    assert!(!text(&ls[1]).chars().all(|c| c == '─'));
    let empty_closed = layout("```\n```\n", 20);
    assert_eq!(empty_closed.len(), 2);
}

#[test]
fn streaming_partial_emphasis_shows_literal_then_resolves() {
    let partial = parse("# Title\n\n**bo").layout(40);
    assert_eq!(texts(&partial), vec!["Title", "", "**bo"]);
    let full = parse("# Title\n\n**bold**").layout(40);
    assert_eq!(texts(&full), vec!["Title", "", "bold"]);
    assert!(has_modifier(&full[2], 0, Modifier::BOLD));
}

#[test]
fn soft_break_joins_with_space_hard_break_splits() {
    assert_eq!(texts(&layout("a\nb", 40)), vec!["a b"]);
    assert_eq!(texts(&layout("a  \nb", 40)), vec!["a", "b"]);
}

#[test]
fn unordered_list_marker_with_hanging_indent() {
    let ls = layout("- alpha beta gamma\n- next", 12);
    assert_eq!(texts(&ls), vec!["• alpha beta", "  gamma", "• next"]);
    assert_eq!(fg(&ls[0], 0), Some(MUTED));
    assert_eq!(ls[0].spans[0].content, "• ");
    assert_eq!(ls[1].spans[0].content, "  ");
}

#[test]
fn ordered_list_uses_source_numbers_and_hangs() {
    let ls = layout("3. three\n4. four", 20);
    assert_eq!(texts(&ls), vec!["3. three", "4. four"]);
    let wrapped = layout("10. alpha beta", 12);
    assert_eq!(texts(&wrapped), vec!["10. alpha", "    beta"]);
}

#[test]
fn task_list_markers() {
    let ls = layout("- [x] done\n- [ ] todo", 20);
    assert_eq!(texts(&ls), vec!["☑ done", "☐ todo"]);
    assert_eq!(fg(&ls[0], 0), Some(MUTED));
    assert_eq!(fg(&ls[1], 0), Some(MUTED));
}

#[test]
fn nested_list_indents_two_spaces_per_level() {
    assert_eq!(texts(&layout("- a\n  - b", 20)), vec!["• a", "  • b"]);
    assert_eq!(
        texts(&layout("- a\n  - b\n    - c", 20)),
        vec!["• a", "  • b", "    • c"]
    );
}

#[test]
fn quote_bar_repeated_on_wrapped_continuations() {
    let ls = layout("> alpha beta gamma", 11);
    assert_eq!(texts(&ls), vec!["│ alpha", "│ beta", "│ gamma"]);
    for l in &ls {
        assert_eq!(fg(l, 0), Some(MUTED));
        assert_eq!(l.spans[0].content, "│ ");
    }
}

#[test]
fn word_wrap_breaks_at_spaces_and_caps_width() {
    assert_eq!(
        texts(&layout("alpha beta gamma delta", 11)),
        vec!["alpha beta", "gamma delta"]
    );
    let hard = layout(&"a".repeat(13), 5);
    assert_eq!(texts(&hard), vec!["aaaaa", "aaaaa", "aaa"]);
}

#[test]
fn cjk_wraps_by_display_width_without_splitting() {
    assert_eq!(texts(&layout("中文中文", 4)), vec!["中文", "中文"]);
    assert_eq!(texts(&layout("中文 ab", 4)), vec!["中文", "ab"]);
}

#[test]
fn overwide_grapheme_keeps_its_own_line() {
    assert_eq!(texts(&layout("中文ab", 1)), vec!["中", "文", "a", "b"]);
    let emoji = layout("ab👨‍👩‍👧cd", 5);
    assert_eq!(texts(&emoji).join(""), "ab👨‍👩‍👧cd");
    assert!(texts(&emoji).iter().any(|line| line.contains("👨‍👩‍👧")));
    assert!(emoji.iter().all(|line| line.width() <= 5));
}

#[test]
fn width_zero_renders_nothing_without_panic() {
    assert!(layout("hello *world*", 0).is_empty());
    assert!(layout("| a | b |\n|---|---|\n| 1 | 2 |", 0).is_empty());
}

#[test]
fn very_long_line_splits_at_grapheme_boundaries() {
    let ls = layout(&"a".repeat(9000), 80);
    assert_eq!(ls.len(), 113);
    assert_eq!(text(&ls[0]).chars().count(), 80);
    let total: usize = ls.iter().map(|l| text(l).chars().count()).sum();
    assert_eq!(total, 9000);
}

#[test]
fn tables_shrink_before_vertical_fallback_and_keep_all_cells() {
    let lines = layout("| aaaa | bbbb |\n|---|---|\n| cccc | dddd |", 10);
    assert!(lines.iter().all(|line| line.width() <= 10));
    let rows = texts(&lines);
    assert_eq!(rows[0].trim_end(), "aaa │ bbbb");
    assert!(rows[1].starts_with('a'));
    assert!(rows.iter().any(|row| row.contains('┼')));
    assert!(rows.iter().any(|row| row.starts_with("ccc │ dddd")));
    assert!(rows.last().unwrap().starts_with('c'));
}

#[test]
fn vertical_tables_keep_long_headers_empty_cells_and_unicode_at_narrow_widths() {
    for width in 1..9 {
        let lines = layout("| | 中文 |\n|---|---|\n| | é🙂 |", width);
        assert!(
            lines
                .iter()
                .all(|line| line.width() <= usize::from(width).max(2))
        );
        let joined: String = texts(&lines)
            .join("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert_eq!(joined, "Column1:中文:é🙂", "width {width}");
    }
}

#[test]
fn tables_align_cells_without_changing_their_content() {
    let lines = layout("| a | b |\n|:---:|---:|\n| x | y |", 40);
    let rows = texts(&lines);
    assert!(rows[0].starts_with(" a ") && rows[0].ends_with("  b"));
    assert!(rows[2].starts_with(" x ") && rows[2].ends_with("  y"));
    assert!(lines.iter().all(|line| line.width() == 9));
}

#[test]
fn quote_and_list_table_prefixes_survive_narrow_layout() {
    for width in [6, 16, 24] {
        let lines = layout("> | a | b |\n> |---|---|\n> | x | y |", width);
        assert!(texts(&lines).iter().all(|line| line.starts_with("│ ")));
        assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
        let visible = texts(&lines).join("");
        assert!(visible.contains('x') && visible.contains('y'));
    }
    let lines = texts(&layout("- | a | b |\n  |---|---|\n  | x | y |", 24));
    assert!(lines[0].starts_with("• "));
    assert!(lines[1..].iter().all(|line| line.starts_with("  ")));
    assert!(
        lines
            .iter()
            .any(|line| line.contains('x') && line.contains('y'))
    );
}

#[test]
fn table_keeps_wide_cells_intact() {
    let src = "| 中文 | x |\n|---|---|\n| 中文中文中文 | y |";
    let ls = layout(src, 40);
    let joined: String = texts(&ls).join("\n");
    assert!(joined.contains("中文中文中文"));
    assert!(joined.contains('│'));
}

#[test]
fn quoted_code_repeats_prefixes_and_preserves_blank_lines_when_wrapped() {
    let lines = layout("> ```rust\n> abcdefghijkl\n>\n> ```", 8);
    let rows = texts(&lines);
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|row| row.starts_with("│ ")));
    assert!(lines.iter().all(|line| line.width() <= 8));
    assert_eq!(rows[1], "│ abcdef");
    assert_eq!(rows[2], "│ ghijkl");
    assert_eq!(rows[3], "│ ");
    let indented = texts(&layout("    first\n\n    last\n", 30));
    assert!(indented.last().unwrap().chars().all(|c| c == '─'));
    assert!(
        indented
            .windows(3)
            .any(|rows| rows == ["first", "", "last"])
    );
}

#[test]
fn display_only_labels_cannot_emit_controls_and_soft_breaks_keep_style() {
    let source = "![a\u{1b}b](https://example.test)\n\n```ru\u{1b}st\ncode\n```\n\n**one\ntwo**";
    let lines = layout(source, 60);
    assert!(texts(&lines).iter().all(|line| !line.contains('\u{1b}')));
    assert!(texts(&lines).iter().any(|line| line.contains("Image: a�b")));
    let bold = lines.iter().find(|line| text(line) == "one two").unwrap();
    assert!(
        bold.spans
            .iter()
            .all(|span| span.style.add_modifier.contains(Modifier::BOLD))
    );
    for width in 1..5 {
        assert!(
            layout("> 12. abcdef", width)
                .iter()
                .all(|line| line.width() <= usize::from(width))
        );
    }
}

#[test]
fn control_characters_are_sanitized_for_display() {
    let ls = layout("a\u{1b}[0mb\u{7f}c\u{0}d", 20);
    assert_eq!(texts(&ls), vec!["a\u{fffd}[0mb\u{fffd}c\u{fffd}d"]);
    let cjk = layout("中文\u{1b}文", 20);
    assert_eq!(texts(&cjk), vec!["中文\u{fffd}文"]);
}

#[test]
fn tabs_expand_to_four_column_tab_stops() {
    let ls = layout("```\n\tgo\n```", 20);
    assert_eq!(text(&ls[1]), "    go");
    assert_eq!(texts(&layout("a\tb", 20)), vec!["a   b"]);
    let mid = layout("```\nab\tc\n```", 20);
    assert_eq!(text(&mid[1]), "ab  c");
}

#[test]
fn code_block_preserves_spaces_and_wraps_exactly() {
    let ls = layout("```\n  keep  spacing\n```", 20);
    assert_eq!(text(&ls[1]), "  keep  spacing");
    let long = layout("```\nabcdefghijklmnopqrstuvwxyz\n```", 10);
    assert_eq!(texts(&long)[1], "abcdefghij");
    assert_eq!(texts(&long)[2], "klmnopqrst");
}

#[test]
fn html_is_shown_literally() {
    assert_eq!(texts(&layout("<div>x</div>", 20)), vec!["<div>x</div>"]);
    assert_eq!(texts(&layout("a <b>x</b> c", 20)), vec!["a <b>x</b> c"]);
}

#[test]
fn empty_messages_produce_no_lines() {
    assert!(layout("", 20).is_empty());
    assert!(layout("\n\n", 20).is_empty());
    assert!(layout("   \n  ", 20).is_empty());
}

#[test]
fn blocks_are_separated_by_one_blank_line() {
    let ls = layout("para\n\n```\nc\n```\n\nafter", 20);
    let dash = "─".repeat(20);
    let expected = vec!["para", "", dash.as_str(), "c", dash.as_str(), "", "after"];
    assert_eq!(texts(&ls), expected);
}

#[test]
fn list_items_keep_paragraph_prefixes() {
    let ls = layout("- first\n\n  second paragraph", 20);
    assert_eq!(texts(&ls), vec!["• first", "", "  second paragraph"]);
}
