use super::*;

#[test]
fn mcp_tip_renders_multiline_below_welcome_hints_without_reserving_input_row() {
    let mut app = app();
    app.state = State::Ready;
    app.input.insert_str("draft");
    let baseline_layout = app.main_layout(Rect::new(0, 0, 100, 32));

    app.mcp_tip = vec![
        "MCP: context7 connected (874ms) ✓".into(),
        "MCP: exa failed (1.1s) ✗".into(),
        "  starting MCP process failed".into(),
        "Connected to MCP servers: 1 succeeded, 1 failed (1.8s)".into(),
    ];
    let with_tip_layout = app.main_layout(Rect::new(0, 0, 100, 32));
    // 不再在输入框上方占用额外行，输入区与对话区几何保持不变。
    assert_eq!(with_tip_layout.input, baseline_layout.input);
    assert_eq!(with_tip_layout.conversation, baseline_layout.conversation);

    let buffer = draw_buffer(&mut app, 100, 32);
    let rows: Vec<String> = (0..32)
        .map(|y| (0..100).map(|x| buffer[(x, y)].symbol()).collect())
        .collect();
    let hints_y = rows
        .iter()
        .position(|row| row.contains("/tips  All shortcuts"))
        .expect("welcome hints row");
    let first_mcp_y = rows
        .iter()
        .position(|row| row.contains("MCP: context7 connected (874ms) ✓"))
        .expect("first mcp line");
    let second_mcp_y = rows
        .iter()
        .position(|row| row.contains("MCP: exa failed (1.1s) ✗"))
        .expect("second mcp line");
    let reason_y = rows
        .iter()
        .position(|row| row.contains("  starting MCP process failed"))
        .expect("failure reason line");
    let summary_y = rows
        .iter()
        .position(|row| row.contains("Connected to MCP servers: 1 succeeded, 1 failed (1.8s)"))
        .expect("summary mcp line");

    assert_eq!(first_mcp_y, hints_y + 2);
    assert_eq!(second_mcp_y, first_mcp_y + 1);
    assert_eq!(reason_y, second_mcp_y + 1);
    assert_eq!(summary_y, reason_y + 1);
    assert!(summary_y < with_tip_layout.input.y as usize);

    let hints_x = rows[hints_y].find("/tips").unwrap() as u16;
    let mcp_x = rows[first_mcp_y].find("MCP:").unwrap() as u16;
    assert_eq!(mcp_x, hints_x);
    assert_eq!(
        rows[reason_y].find("starting MCP process failed").unwrap() as u16,
        mcp_x + 2
    );
    let check_x = (0..100)
        .find(|&x| buffer[(x, first_mcp_y as u16)].symbol() == "✓")
        .expect("check mark cell");
    let cross_x = (0..100)
        .find(|&x| buffer[(x, second_mcp_y as u16)].symbol() == "✗")
        .expect("cross mark cell");
    assert_eq!(
        buffer[(mcp_x, first_mcp_y as u16)].fg,
        themes::palette().welcome_muted
    );
    assert_eq!(
        buffer[(check_x, first_mcp_y as u16)].fg,
        themes::palette().added
    );
    assert_eq!(
        buffer[(mcp_x, second_mcp_y as u16)].fg,
        themes::palette().welcome_muted
    );
    assert_eq!(
        buffer[(cross_x, second_mcp_y as u16)].fg,
        themes::palette().removed
    );
    assert_eq!(
        buffer[(mcp_x + 2, reason_y as u16)].fg,
        themes::palette().welcome_muted
    );
    assert_eq!(
        buffer[(mcp_x, summary_y as u16)].fg,
        themes::palette().welcome_muted
    );

    // 欢迎页关闭后不再渲染 MCP tip。
    app.show_welcome = false;
    let hidden = draw(&mut app, 100, 32);
    assert!(!hidden.contains("MCP: context7 connected"), "{hidden}");
    assert!(!hidden.contains("Connected to MCP servers"), "{hidden}");
}

#[test]
fn mcp_tip_wraps_long_lines_within_welcome_column() {
    let mut app = app();
    app.mcp_tip = vec![
        "MCP unavailable: invalid configuration in mcp.json that exceeds the welcome column width and should wrap cleanly".into(),
    ];
    let text = draw(&mut app, 80, 32);
    assert!(text.contains("MCP unavailable:"), "{text}");
    assert!(text.contains("should wrap cleanly"), "{text}");
}

#[test]
fn mcp_tip_tick_clears_the_lines_when_no_manager_is_attached() {
    let mut app = app();
    app.mcp_tip = vec!["MCP: exa connecting…".into()];
    // 无 bootstrap（测试与 mock）时没有建连进度，提示行不残留。
    assert!(app.mcp_tip_tick());
    assert!(app.mcp_tip.is_empty());
    assert!(!app.mcp_tip_tick());

    app.mcp_tip = vec!["MCP: exa connecting…".into()];
    app.dismiss_mcp_tip();
    assert!(!app.mcp_tip_tick());
    assert!(app.mcp_tip.is_empty());
}
