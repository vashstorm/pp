use super::*;

#[test]
fn mcp_list_preserves_draft_and_viewport_and_subcommands_complete() {
    let mut app = app();
    app.state = State::Ready;
    app.input.insert_str("pending draft");
    app.conversation.append("You", "previous conversation");
    let before = app.input.lines().to_vec();
    let view = crate::mcp::View {
        entries: vec![crate::mcp::EntryView {
            name: "context7".into(),
            source: "Claude Code".into(),
            path: "/fixture/.claude.json".into(),
            status: "connected",
        }],
        diagnostics: vec![],
    };
    app.mcp_panel = Some(crate::tui::mcp::Panel::list(&view));
    app.mcp_view = view;
    assert!(app.needs_alternate());
    let text = draw(&mut app, 110, 25);
    assert!(text.contains("Configured MCP Servers"));
    assert!(text.contains("context7"));
    app.handle(key(KeyCode::Esc), &mut false);
    assert!(app.mcp_panel.is_none());
    assert_eq!(app.input.lines(), before);
    app.input = editor();
    app.input.insert_str("/mcp rec");
    app.refresh_candidates(true);
    assert_eq!(app.menu.as_ref().unwrap().items()[0].label, "reconnect");
    let buffer = draw_buffer(&mut app, 110, 25);
    assert!(
        buffer
            .content
            .iter()
            .any(|cell| cell.fg == Color::Rgb(255, 190, 55))
    );
    app.apply_menu_action(MenuAction::McpSubcommand(3), false);
    assert_eq!(app.input.lines()[0], "/mcp reconnect ");
    assert_eq!(app.menu.as_ref().unwrap().items()[0].label, "context7");
    app.apply_menu_action(MenuAction::McpServer(0), false);
    assert_eq!(app.input.lines()[0], "/mcp reconnect context7");
}
