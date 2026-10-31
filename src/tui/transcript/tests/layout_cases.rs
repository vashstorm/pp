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

#[test]
fn notification_error_card_wraps_full_plain_text_and_survives_resize_and_folding() {
    let mut transcript = Transcript::new();
    let text = format!(
        "**plain error**\n中文🙂\tend\n{}",
        (0..90)
            .map(|i| format!("detail-{i:03}\n"))
            .collect::<String>()
    );
    transcript.append_error(&text);
    for width in [100, 20, 5, 4, 40] {
        let buffer = render(&mut transcript, width, 1100);
        let rows = screen(&buffer);
        if width >= 9 {
            assert!(rows.contains("Error"));
            assert_eq!(buffer[(0, 0)].symbol(), "╭");
            assert_eq!(buffer[(width - 1, 0)].symbol(), "╮");
            assert_eq!(buffer[(0, 0)].fg, palette::ERROR_BORDER);
            let bottom = transcript.entries[0].layout.as_ref().unwrap().height as u16 - 1;
            assert_eq!(buffer[(0, bottom)].symbol(), "╰");
            assert_eq!(buffer[(width - 1, bottom)].symbol(), "╯");
            assert!(rows.contains("detail-089"));
            assert!(rows.replace(' ', "").contains("中文🙂"));
            assert!(rows.contains("**plain error**"));
            assert!(!rows.lines().nth(1).unwrap().contains("中文"));
            assert!(
                rows.lines()
                    .nth(2)
                    .unwrap()
                    .replace(' ', "")
                    .contains("中文🙂")
            );
        }
        assert!(!rows.contains('\t'));
    }
    let before = screen(&render(&mut transcript, 80, 120));
    transcript.toggle_tool_output();
    assert_eq!(screen(&render(&mut transcript, 80, 120)), before);
    assert_eq!(transcript.entries[0].text, text);
}

#[test]
fn notification_error_preserves_browse_anchor_and_native_history_order() {
    let mut transcript = Transcript::new();
    transcript.append(
        "Assistant",
        &(0..60)
            .map(|i| format!("history-{i:02}\n"))
            .collect::<String>(),
    );
    render(&mut transcript, 40, 10);
    transcript.scroll_up(15);
    let before = screen(&render(&mut transcript, 40, 10));
    transcript.append_error("first error");
    transcript.append_error("second error");
    transcript.append_error(" \n\t");
    assert_eq!(screen(&render(&mut transcript, 40, 10)), before);
    assert!(!transcript.following());
    transcript.follow();
    let output = retire_native(&mut transcript, 40, 1, true);
    assert_eq!(output.matches("first error").count(), 1);
    assert_eq!(output.matches("second error").count(), 1);
    assert!(output.find("first error").unwrap() < output.find("second error").unwrap());
    assert!(
        retire_native(&mut transcript, 40, 1, true)
            .trim()
            .is_empty()
    );
    assert_eq!(transcript.fresh().entry_count(), 0);
}
