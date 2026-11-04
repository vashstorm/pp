use super::*;

#[test]
fn agent_hub_shortcut_opens_over_completion_and_preserves_draft() {
    let mut app = app();
    app.input.insert_str("/sub");
    app.refresh_candidates(true);
    assert!(app.menu.is_some());
    let toggle = Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::ALT));
    app.handle(toggle.clone(), &mut false);
    assert!(app.subagent_panel.is_some());
    assert!(app.menu.is_none());
    assert!(draw(&mut app, 120, 30).contains("Agent Hub"));
    assert_eq!(app.input_text(), "/sub");
    app.handle(toggle, &mut false);
    assert!(app.subagent_panel.is_none());
    assert_eq!(app.input_text(), "/sub");
}

#[test]
fn agent_hub_shortcut_only_accepts_option_a_press() {
    use crate::tui::app::hub::is_hub_toggle;
    for modifiers in [
        KeyModifiers::NONE,
        KeyModifiers::CONTROL,
        KeyModifiers::ALT | KeyModifiers::CONTROL,
    ] {
        assert!(!is_hub_toggle(&Event::Key(KeyEvent::new(
            KeyCode::Char('a'),
            modifiers,
        ))));
    }
    for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
        assert!(!is_hub_toggle(&Event::Key(KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::ALT,
            kind,
        ))));
    }
    let mut app = app();
    app.input.insert_str("keep draft");
    app.handle(control('a'), &mut false);
    assert!(app.subagent_panel.is_none());
    assert_eq!(app.input.cursor().1, 0);
    assert_eq!(app.input_text(), "keep draft");
}
