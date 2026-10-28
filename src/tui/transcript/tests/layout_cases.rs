use super::*;

#[test]
fn styled_blocks_expand_tabs_and_keep_explicit_line_boundaries() {
    let rows = wrap_block(
        block_line(
            "ab\t中\nsecond\n\nthird\n",
            Style::default().fg(palette::ADDED),
        ),
        12,
        "",
    );
    let text: Vec<String> = rows
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect();
    assert_eq!(text, ["ab  中", "second", "", "third", ""]);
    assert!(
        rows.iter()
            .flat_map(|line| &line.spans)
            .all(|span| span.style.fg == Some(palette::ADDED))
    );
}

#[test]
fn styled_rows_preserve_graphemes_and_neutralize_terminal_controls() {
    let text = "中e\u{301}🙂\t\u{1b}[31m";
    let rows = wrap_block(block_line(text, Style::default().fg(palette::ADDED)), 3, "");
    let visible: String = rows
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.content.as_ref())
        .collect();
    assert!(visible.starts_with("中e\u{301}🙂"));
    assert!(visible.contains('�'));
    assert!(!visible.contains('\u{1b}') && !visible.contains('\t'));
    assert!(rows.iter().all(|line| line.width() <= 3));
    assert!(
        rows.iter()
            .flat_map(|line| &line.spans)
            .all(|span| span.style.fg == Some(palette::ADDED))
    );
}
