use super::*;
use crate::tui::transcript_style::{edit_number, edit_string, syntax_keyword};
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

/// Foreground color of the first span containing `needle`, used for semantic highlight assertions.
fn span_fg(line: &Line<'_>, needle: &str) -> Option<Color> {
    line.spans
        .iter()
        .find(|span| span.content.contains(needle))
        .and_then(|span| span.style.fg)
}

#[test]
fn headings_inline_styles_links_and_unicode_wrapping() {
    let ls = layout("# Title", 40);
    assert_eq!(texts(&ls), vec!["Title"]);
    assert_eq!(fg(&ls[0], 0), Some(md_heading()));
    assert!(has_modifier(&ls[0], 0, Modifier::BOLD));
    let setext = layout("Title\n=====", 40);
    assert_eq!(texts(&setext), vec!["Title"]);
    assert_eq!(fg(&setext[0], 0), Some(md_heading()));

    let source = "# **Overview**\n\nA long paragraph that wraps over several terminal lines.\n\n```md\n# Not a heading\n```\n\n## Repeated section `code`\n\n| Column | Value |\n| --- | --- |\n| long content | more content |\n\nRepeated section\n--------\n\n### Final\n";
    let document = parse(source);
    for width in [8, 25, 80] {
        let (lines, headings) = document.layout_with_headings(width);
        assert_eq!(lines, document.layout(width));
        assert_eq!(headings.len(), 4);
        assert_eq!(
            headings.iter().map(|h| h.level).collect::<Vec<_>>(),
            [1, 2, 2, 3]
        );
        assert_eq!(headings[0].title, "Overview");
        assert_eq!(headings[1].title, "Repeated section code");
        assert_eq!(headings[2].title, "Repeated section");
        for heading in &headings {
            assert!(heading.title.starts_with(&text(&lines[heading.row])));
        }
        assert!(headings.windows(2).all(|pair| pair[0].row < pair[1].row));
    }
    let (lines, headings) = document.layout_with_headings(0);
    assert!(lines.is_empty() && headings.is_empty());

    let styled = layout("**bold** and *em* and ~~gone~~ and `code`", 80);
    assert_eq!(
        texts(&styled),
        vec!["bold and em and gone and code".to_string()]
    );
    let contents: Vec<&str> = styled[0].spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(
        contents,
        vec!["bold", " and ", "em", " and ", "gone", " and ", "code"]
    );
    assert!(has_modifier(&styled[0], 0, Modifier::BOLD));
    assert_eq!(fg(&styled[0], 0), Some(md_strong()));
    assert!(has_modifier(&styled[0], 2, Modifier::ITALIC));
    assert!(has_modifier(&styled[0], 4, Modifier::CROSSED_OUT));
    assert_eq!(fg(&styled[0], 6), Some(md_code()));
    assert_eq!(fg(&styled[0], 1), None);

    let linked = layout("see [docs](http://e.com/x) now", 80);
    let link_contents: Vec<String> = linked[0]
        .spans
        .iter()
        .map(|s| s.content.to_string())
        .collect();
    assert_eq!(
        link_contents,
        vec!["see ", "docs", " (http://e.com/x)", " now"]
    );
    assert_eq!(fg(&linked[0], 1), Some(link()));
    assert_eq!(fg(&linked[0], 2), Some(dim()));
    let same = layout("[http://e.com](http://e.com)", 80);
    assert_eq!(texts(&same), vec!["http://e.com"]);
    assert_eq!(fg(&same[0], 0), Some(link()));

    let img = layout("![alt text](http://i.io/p.png)", 80);
    let img_contents: Vec<String> = img[0].spans.iter().map(|s| s.content.to_string()).collect();
    assert_eq!(
        img_contents,
        vec!["Image: alt text", " (http://i.io/p.png)"]
    );
    assert_eq!(fg(&img[0], 1), Some(dim()));

    assert_eq!(
        texts(&layout("alpha beta gamma delta", 11)),
        vec!["alpha beta", "gamma delta"]
    );
    assert_eq!(
        texts(&layout(&"a".repeat(13), 5)),
        vec!["aaaaa", "aaaaa", "aaa"]
    );
    assert_eq!(texts(&layout("中文中文", 4)), vec!["中文", "中文"]);
    assert_eq!(texts(&layout("中文 ab", 4)), vec!["中文", "ab"]);
    assert_eq!(texts(&layout("中文ab", 1)), vec!["中", "文", "a", "b"]);
    let emoji = layout("ab👨\u{200d}👩\u{200d}👧cd", 5);
    assert_eq!(texts(&emoji).join(""), "ab👨\u{200d}👩\u{200d}👧cd");
    assert!(emoji.iter().all(|line| line.width() <= 5));
    assert!(layout("hello *world*", 0).is_empty());
    assert!(layout("| a | b |\n|---|---|\n| 1 | 2 |", 0).is_empty());
    let ls = layout(&"a".repeat(9000), 80);
    assert_eq!(ls.len(), 113);
    assert_eq!(
        ls.iter().map(|l| text(l).chars().count()).sum::<usize>(),
        9000
    );
}

#[test]
fn code_block_rendering_wrapping_and_combining_graphemes() {
    let ls = layout("```rust ignore_me\nlet x = 1;\n```\n", 40);
    assert_eq!(ls.len(), 3);
    assert!(text(&ls[0]).starts_with("── rust "));
    assert!(text(&ls[0]).ends_with('─'));
    assert_eq!(Span::raw(text(&ls[0])).width(), 40);
    assert_eq!(text(&ls[1]), "let x = 1;");
    assert_eq!(span_fg(&ls[1], "let"), Some(syntax_keyword()));
    assert_eq!(span_fg(&ls[1], "1"), Some(edit_number()));
    assert_eq!(span_fg(&ls[1], " x "), Some(code_block()));
    assert_eq!(text(&ls[2]), "─".repeat(40));

    for src in ["```\nlet x = 1;\n```\n", "```notalang\nlet x = 1;\n```\n"] {
        let plain = layout(src, 40);
        assert_eq!(text(&plain[1]), "let x = 1;");
        assert_eq!(span_fg(&plain[1], "let"), Some(code_block()));
    }

    let wrapped = layout("```rust\nlet xy = 1;\n```\n", 9);
    assert_eq!(
        texts(&wrapped),
        vec!["── rust ─", "let xy = ", "1;", "─────────"]
    );
    assert_eq!(span_fg(&wrapped[1], "let"), Some(syntax_keyword()));
    assert_eq!(span_fg(&wrapped[2], "1"), Some(edit_number()));
    assert!(wrapped.iter().all(|line| line.width() <= 9));

    let unicode = layout("```rust\n\tlet s = \"hello🙂\";\n```\n", 40);
    assert_eq!(text(&unicode[1]), "    let s = \"hello🙂\";");
    assert_eq!(span_fg(&unicode[1], "hello🙂"), Some(edit_string()));

    let huge = format!("```rust\n{}\n```\n", "let x = 1;\n".repeat(500));
    let huge_ls = layout(&huge, 40);
    assert_eq!(huge_ls.len(), 503);
    for line in &huge_ls[1..501] {
        assert_eq!(span_fg(line, "let"), Some(code_block()));
    }

    let open = layout("```rust\nlet x = 1;\n", 40);
    assert_eq!(open.len(), 2);
    assert_eq!(span_fg(&open[1], "let"), Some(syntax_keyword()));
    assert_eq!(layout("```\n```\n", 20).len(), 2);

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
                // Only the padding of the last line is removed, keeping the code's own spaces before the newline.
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
fn streaming_emphasis_lists_quotes_and_table_tail_formatting() {
    let partial = parse("# Title\n\n**bo").layout(40);
    assert_eq!(texts(&partial), vec!["Title", "", "**bo"]);
    let full = parse("# Title\n\n**bold**").layout(40);
    assert_eq!(texts(&full), vec!["Title", "", "bold"]);
    assert!(has_modifier(&full[2], 0, Modifier::BOLD));

    assert_eq!(texts(&layout("a\nb", 40)), vec!["a b"]);
    assert_eq!(texts(&layout("a  \nb", 40)), vec!["a", "b"]);

    let ul = layout("- alpha beta gamma\n- next", 12);
    assert_eq!(texts(&ul), vec!["• alpha beta", "  gamma", "• next"]);
    assert_eq!(fg(&ul[0], 0), Some(md_bullet()));
    assert_eq!(ul[1].spans[0].content, "  ");

    assert_eq!(
        texts(&layout("3. three\n4. four", 20)),
        vec!["3. three", "4. four"]
    );
    assert_eq!(
        texts(&layout("10. alpha beta", 12)),
        vec!["10. alpha", "    beta"]
    );
    let tasks = layout("- [x] done\n- [ ] todo", 20);
    assert_eq!(texts(&tasks), vec!["☑ done", "☐ todo"]);
    assert_eq!(fg(&tasks[0], 0), Some(md_bullet()));
    assert_eq!(
        texts(&layout("- a\n  - b\n    - c", 20)),
        vec!["• a", "  • b", "    • c"]
    );
    assert_eq!(
        texts(&layout("- first\n\n  second paragraph", 20)),
        vec!["• first", "", "  second paragraph"]
    );

    let quotes = layout("> alpha beta gamma", 11);
    assert_eq!(texts(&quotes), vec!["│ alpha", "│ beta", "│ gamma"]);
    for l in &quotes {
        assert_eq!(fg(l, 0), Some(muted()));
    }

    for (prefix, tail) in [
        (
            "| A | B |\r\n| --- | --- |\r\n| x | y |\r\n",
            "| x | 🙂 |\r",
        ),
        (
            "> | A | B |\n> | --- | --- |\n> | x | y |\n",
            "> | x | 🙂 |",
        ),
        (
            "- | A | B |\n  | --- | --- |\n  | x | y |\n",
            "  | x | 🙂 |",
        ),
        ("| A | B |\n", "| --- | --- |"),
    ] {
        let source = format!("{prefix}{tail}");
        let streaming = parse_streaming(&source, true);
        assert!(streaming.table_tail_start.is_some(), "{source}");
        assert_eq!(streaming.layout(16), layout(prefix, 16));
        let complete = format!("{source}\n");
        assert_eq!(
            parse_streaming(&complete, true).layout(16),
            layout(&source, 16)
        );
    }

    let prose = "| A | B |\n| --- | --- |\n| x | y |\n\n**Next paragraph**";
    assert_eq!(parse_streaming(prose, true).layout(30), layout(prose, 30));

    let shrunk = layout("| aaaa | bbbb |\n|---|---|\n| cccc | dddd |", 14);
    assert!(shrunk.iter().all(|line| line.width() == 14));
    assert_eq!(
        texts(&shrunk),
        vec![
            "┌─────┬──────┐",
            "│ aaa │ bbbb │",
            "│ a   │      │",
            "├─────┼──────┤",
            "│ ccc │ dddd │",
            "│ c   │      │",
            "└─────┴──────┘",
        ]
    );
    for width in 1..13 {
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
    let aligned = layout("| a | b |\n|:---:|---:|\n| x | y |", 40);
    assert_eq!(
        texts(&aligned),
        vec![
            "┌─────┬─────┐",
            "│  a  │   b │",
            "├─────┼─────┤",
            "│  x  │   y │",
            "└─────┴─────┘"
        ]
    );
    assert_eq!(span_fg(&aligned[1], "a"), Some(md_heading()));
    for width in [6, 16, 24] {
        let q = layout("> | a | b |\n> |---|---|\n> | x | y |", width);
        assert!(texts(&q).iter().all(|line| line.starts_with("│ ")));
        assert!(q.iter().all(|line| line.width() <= usize::from(width)));
    }
    let list_tbl = texts(&layout("- | a | b |\n  |---|---|\n  | x | y |", 24));
    assert!(list_tbl[0].starts_with("• ") && list_tbl[1..].iter().all(|l| l.starts_with("  ")));
    assert!(
        texts(&layout("| 中文 | x |\n|---|---|\n| 中文中文中文 | y |", 40))
            .join("\n")
            .contains("中文中文中文")
    );
}

#[test]
fn quoted_code_controls_tabs_html_and_blanks() {
    let lines = layout("> ```rust\n> abcdefghijkl\n>\n> ```", 8);
    let rows = texts(&lines);
    assert_eq!(rows.len(), 5);
    assert!(rows.iter().all(|row| row.starts_with("│ ")));
    assert_eq!(
        (rows[1].as_str(), rows[2].as_str(), rows[3].as_str()),
        ("│ abcdef", "│ ghijkl", "│ ")
    );
    let indented = texts(&layout("    first\n\n    last\n", 30));
    assert!(
        indented
            .windows(3)
            .any(|rows| rows == ["first", "", "last"])
    );

    let sanitized = layout(
        "![a\u{1b}b](https://example.test)\n\n```ru\u{1b}st\ncode\n```\n\n**one\ntwo**",
        60,
    );
    assert!(
        texts(&sanitized)
            .iter()
            .all(|line| !line.contains('\u{1b}'))
    );
    assert!(
        texts(&sanitized)
            .iter()
            .any(|line| line.contains("Image: a\u{fffd}b"))
    );
    let bold = sanitized
        .iter()
        .find(|line| text(line) == "one two")
        .unwrap();
    assert!(
        bold.spans
            .iter()
            .all(|s| s.style.add_modifier.contains(Modifier::BOLD))
    );
    assert_eq!(
        texts(&layout("a\u{1b}[0mb\u{7f}c\u{0}d", 20)),
        vec!["a\u{fffd}[0mb\u{fffd}c\u{fffd}d"]
    );
    assert_eq!(texts(&layout("```\n\tgo\n```", 20)[1..2]), vec!["    go"]);
    assert_eq!(texts(&layout("a\tb", 20)), vec!["a   b"]);
    assert_eq!(
        texts(&layout("```\nabcdefghijklmnopqrstuvwxyz\n```", 10))[1..3],
        ["abcdefghij", "klmnopqrst"]
    );
    assert_eq!(texts(&layout("<div>x</div>", 20)), vec!["<div>x</div>"]);
    assert!(layout("", 20).is_empty() && layout("   \n  ", 20).is_empty());
    let dash = "─".repeat(20);
    assert_eq!(
        texts(&layout("para\n\n```\nc\n```\n\nafter", 20)),
        vec!["para", "", dash.as_str(), "c", dash.as_str(), "", "after"]
    );
}

#[test]
fn tables_frame_every_row_and_keep_unicode_and_inline_styles_when_wrapping() {
    let source = "| 方案 | 做法 |\n|---|---|\n| **推荐** | `todo`中文é🙂 |\n| B | 下一行 |";
    let lines = layout(source, 15);
    assert!(lines.iter().all(|line| line.width() == 15));
    let rows = texts(&lines);
    assert_eq!(rows.first().unwrap(), "┌──────┬──────┐");
    assert_eq!(rows.last().unwrap(), "└──────┴──────┘");
    assert_eq!(rows.iter().filter(|line| line.starts_with('├')).count(), 2);
    for line in rows.iter().filter(|line| line.starts_with('│')) {
        assert!(line.ends_with('│'));
        assert_eq!(line.matches('│').count(), 3);
    }
    let content: String = rows
        .iter()
        .filter(|line| line.starts_with('│'))
        .map(|line| line.replace(['│', ' '], ""))
        .collect();
    assert_eq!(content, "方案做法推荐todo中文é🙂B下一行");
    assert!(
        lines
            .iter()
            .any(|line| span_fg(line, "推荐") == Some(md_strong()))
    );
    assert!(
        lines
            .iter()
            .any(|line| span_fg(line, "todo") == Some(md_code()))
    );

    let header_only = texts(&layout("| A |\n|---|", 7));
    assert_eq!(header_only, ["┌─────┐", "│ A   │", "└─────┘"]);

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
fn controls_tabs_spaces_html_blanks_and_lists() {
    let ls = layout("a\u{1b}[0mb\u{7f}c\u{0}d", 20);
    assert_eq!(texts(&ls), vec!["a\u{fffd}[0mb\u{fffd}c\u{fffd}d"]);
    assert_eq!(texts(&layout("中文\u{1b}文", 20)), vec!["中文\u{fffd}文"]);
    assert_eq!(text(&layout("```\n\tgo\n```", 20)[1]), "    go");
    assert_eq!(texts(&layout("a\tb", 20)), vec!["a   b"]);
    assert_eq!(text(&layout("```\nab\tc\n```", 20)[1]), "ab  c");
    assert_eq!(
        text(&layout("```\n  keep  spacing\n```", 20)[1]),
        "  keep  spacing"
    );
    let long = layout("```\nabcdefghijklmnopqrstuvwxyz\n```", 10);
    assert_eq!(
        (texts(&long)[1].as_str(), texts(&long)[2].as_str()),
        ("abcdefghij", "klmnopqrst")
    );
    assert_eq!(texts(&layout("<div>x</div>", 20)), vec!["<div>x</div>"]);
    assert_eq!(texts(&layout("a <b>x</b> c", 20)), vec!["a <b>x</b> c"]);
    assert!(
        layout("", 20).is_empty()
            && layout("\n\n", 20).is_empty()
            && layout("   \n  ", 20).is_empty()
    );
    let dash = "─".repeat(20);
    assert_eq!(
        texts(&layout("para\n\n```\nc\n```\n\nafter", 20)),
        vec!["para", "", dash.as_str(), "c", dash.as_str(), "", "after"]
    );
    assert_eq!(
        texts(&layout("- first\n\n  second paragraph", 20)),
        vec!["• first", "", "  second paragraph"]
    );
}

/// Every streaming append must keep the semantic blocks, the recorded source offsets and the physical
/// rows identical to a freshly parsed document, because the preview slices the source at those offsets.
#[test]
fn streaming_appends_match_a_full_parse_for_every_block_shape() {
    // CJK and emoji payloads make byte offsets differ from the visible column widths.
    let base = "# Plan 🙂\n\nIntro paragraph with **bold**, `code` and a [link](https://example.test/a).\n\n\
> quoted line with a [second](https://example.test/b) link\n\n\
- first item\n- second item\n  - nested item\n\n\
Setext title\n------------\n\n\
| A | B |\n| --- | --- |\n| 1 | 2 |\n\n\
```rust\nfn main() {\n    let value = 1;\n}\n```\n\n\
open tail paragraph with 中文";
    let tail = " and 🙂 words\n\n- appended item\n- second appended item\n\n\
> appended quote with `code`\n\n\
| C | D |\n| --- | --- |\n| 3 | 4 |\n\n\
## appended heading\n\n\
```rust\nlet tail = 2;\n";
    let boundaries: Vec<usize> = tail
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(tail.len()))
        .collect();
    let steps = [1usize, 2, 3, 5, 8, 13, 21, 34, 64];
    let mut document = parse_streaming(base, true);
    let mut previous = base.to_string();
    let mut cursor = 0usize;
    let mut step = 0usize;
    while cursor + 1 < boundaries.len() {
        cursor = (cursor + steps[step % steps.len()]).min(boundaries.len() - 1);
        step += 1;
        let source = format!("{base}{}", &tail[..boundaries[cursor]]);
        document.update(&previous, &source, true);
        previous.clone_from(&source);
        let label = format!("append step {step} ({} source bytes)", source.len());
        let mut reference = parse_streaming(&source, true);
        assert!(
            document.blocks == reference.blocks,
            "semantic blocks differ: {label}"
        );
        assert_eq!(
            document.block_top_starts, reference.block_top_starts,
            "top-level block offsets differ: {label}"
        );
        assert_eq!(
            document.last_top_is_list, reference.last_top_is_list,
            "{label}"
        );
        assert_eq!(
            document.ends_with_blank_line, reference.ends_with_blank_line,
            "{label}"
        );
        assert_eq!(
            document.trailing_line_start, reference.trailing_line_start,
            "{label}"
        );
        assert_eq!(
            document.table_tail_start, reference.table_tail_start,
            "{label}"
        );
        assert_eq!(
            document.ends_with_table(),
            reference.ends_with_table(),
            "{label}"
        );
        for width in [8u16, 40, 24, 80] {
            let (rows, meta) = document.layout_cached_with_meta(width);
            let (expected, expected_meta) = reference.layout_cached_with_meta(width);
            assert_eq!(
                rows, expected,
                "physical rows differ: {label}, width {width}"
            );
            assert_eq!(meta.stable_rows, expected_meta.stable_rows, "{label}");
            assert_eq!(
                meta.tail_source_start, expected_meta.tail_source_start,
                "tail start differs from the full parse: {label}"
            );
            assert_eq!(
                meta.needs_separator, expected_meta.needs_separator,
                "{label}"
            );
            assert_eq!(
                meta.open_code_prefix, expected_meta.open_code_prefix,
                "{label}"
            );
            assert_eq!(
                meta.fallback_prefix, expected_meta.fallback_prefix,
                "{label}"
            );
            let start = meta.tail_source_start;
            assert!(
                start < source.len() && source.is_char_boundary(start),
                "tail start {start} is not a character boundary: {label}"
            );
            assert!(
                start == 0 || source.as_bytes()[start - 1] == b'\n',
                "tail start {start} is not at a line start: {label}"
            );
            assert!(meta.stable_rows < rows.len(), "{label}, width {width}");
        }
    }
    // An unchanged snapshot must not drift away from a fresh parse either.
    document.update(&previous, &previous, true);
    assert_eq!(
        document.layout(24),
        parse_streaming(&previous, true).layout(24)
    );
}

/// Snapshot shapes that cannot be proven incremental must still match a full parse; the definition
/// cases would otherwise keep resolving links from a stale prefix.
#[test]
fn streaming_fallbacks_match_a_full_parse_for_definitions_replacement_and_terminal_state() {
    // A definition appended after the text resolves a link in the settled prefix.
    let prefix = "Intro [docs][id] text.\n\nsecond paragraph";
    let defined = format!("{prefix}\n\n[id]: https://example.test/defined\n");
    let mut document = parse_streaming(prefix, true);
    document.update(prefix, &defined, true);
    let reference = parse_streaming(&defined, true);
    assert!(
        document.blocks == reference.blocks,
        "an appended definition must re-parse the whole document"
    );
    let rows = texts(&document.layout(80));
    assert_eq!(rows, texts(&reference.layout(80)));
    assert!(
        rows.iter()
            .any(|row| row.contains("(https://example.test/defined)")),
        "the appended definition must resolve the settled link: {rows:?}"
    );

    // A definition settled in the prefix must resolve a link that only appears in the appended tail.
    let defined = "[id]: https://example.test/settled\n\nIntro";
    let tail_appended = format!("{defined} [docs][id]");
    let mut document = parse_streaming(defined, true);
    document.update(defined, &tail_appended, true);
    let reference = parse_streaming(&tail_appended, true);
    assert!(
        document.blocks == reference.blocks,
        "a prefix definition must re-parse the appended tail"
    );
    let rows = texts(&document.layout(80));
    assert_eq!(rows, texts(&reference.layout(80)));
    assert!(
        rows.iter()
            .any(|row| row.contains("(https://example.test/settled)")),
        "the appended tail must resolve against the prefix definition: {rows:?}"
    );
    assert_eq!(document.block_top_starts, reference.block_top_starts);
    assert_eq!(document.trailing_line_start, reference.trailing_line_start);

    // A cancelled run or a switched message replaces the source instead of extending it.
    let replaced = "Different body 🙂\n\n> quoted\n\n```rust\nlet x = 1;\n```";
    document.update(&tail_appended, replaced, true);
    let reference = parse_streaming(replaced, true);
    assert!(
        document.blocks == reference.blocks,
        "a replaced source body must re-parse from scratch"
    );
    assert_eq!(document.block_top_starts, reference.block_top_starts);
    assert_eq!(
        texts(&document.layout(40)),
        texts(&reference.layout(40)),
        "replaced source rows"
    );

    // The terminal snapshot ends the stream and completes the table the streaming preview held back.
    let partial_table = "intro\n\n| A | B |\n| --- | --- |";
    let still_streaming = format!("{partial_table} | 1 |");
    let mut document = parse_streaming(partial_table, true);
    assert!(document.table_tail_start.is_some());
    document.update(partial_table, &still_streaming, true);
    let reference = parse_streaming(&still_streaming, true);
    assert!(
        document.blocks == reference.blocks,
        "a growing table tail must keep matching the full parse"
    );
    let terminal = format!("{partial_table} | 1 | 2 |\n");
    document.update(&still_streaming, &terminal, false);
    let reference = parse(&terminal);
    assert!(
        document.blocks == reference.blocks,
        "the terminal snapshot must re-parse the held-back table"
    );
    assert_eq!(document.table_tail_start, reference.table_tail_start);
    assert_eq!(texts(&document.layout(40)), texts(&reference.layout(40)));
}
