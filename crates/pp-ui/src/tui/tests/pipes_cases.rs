use super::*;

fn pipe_store() -> (pp_runtime::tools::PipeStore, String) {
    let store = pp_runtime::tools::PipeStore::new();
    let id = "a".repeat(32);
    drop(store.start_run(id.clone(), "root").expect("pipe run"));
    (store, id)
}

fn press(app: &mut App, code: KeyCode) {
    app.handle(
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE)),
        &mut false,
    );
}

fn wait_for(app: &mut App, predicate: impl Fn(&App) -> bool) {
    for _ in 0..200 {
        app.receive();
        if predicate(app) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("pipe panel condition was not reached");
}

#[test]
fn pipes_command_opens_locally_while_running_without_steering() {
    let (mut app, requests) = file_commands_app(commands::FileCommands::default());
    let (store, id) = pipe_store();
    app.pipe_store = Some(store);
    app.state = State::Running;
    app.input.insert_str("/pipes");
    press(&mut app, KeyCode::Enter);
    assert!(app.pipes_panel.is_some());
    assert!(app.needs_alternate());
    assert!(requests.try_recv().is_err(), "local panel must not steer");
    wait_for(&mut app, |app| {
        app.pipes_panel
            .as_ref()
            .is_some_and(|panel| panel.runs().len() == 1)
    });
    assert_eq!(
        app.pipes_panel
            .as_ref()
            .and_then(pipes::Panel::selected_run_id_for_test),
        Some(id.as_str())
    );
    assert_eq!(app.input_text(), "");
    press(&mut app, KeyCode::Esc);
    assert!(app.pipes_panel.is_none());
    assert!(!app.needs_alternate());
}

#[test]
fn f8_preserves_draft_and_escape_restores_the_conversation() {
    let mut app = app();
    app.state = State::Ready;
    let (store, _) = pipe_store();
    app.pipe_store = Some(store);
    app.input.insert_str("keep this draft");
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
        &mut false,
    );
    assert!(app.pipes_panel.is_some());
    assert!(app.needs_alternate());
    wait_for(&mut app, |app| {
        app.pipes_panel
            .as_ref()
            .is_some_and(|panel| !panel.runs().is_empty())
    });
    assert_eq!(app.input_text(), "keep this draft");
    press(&mut app, KeyCode::Esc);
    assert!(app.pipes_panel.is_none());
    assert_eq!(app.input_text(), "keep this draft");
}

#[test]
fn unknown_explicit_run_shows_unavailable_without_switching() {
    let (mut app, _) = file_commands_app(commands::FileCommands::default());
    let (store, _) = pipe_store();
    app.pipe_store = Some(store);
    let missing = "b".repeat(32);
    app.input.insert_str(format!("/pipes {missing}"));
    press(&mut app, KeyCode::Enter);
    wait_for(&mut app, |app| {
        app.pipes_panel
            .as_ref()
            .is_some_and(|panel| panel.error().is_some())
    });
    let panel = app.pipes_panel.as_ref().expect("pipes panel");
    let error = panel.error().expect("unavailable error");
    assert!(error.contains("unavailable"));
    let text = draw(&mut app, 100, 20);
    assert!(text.contains("unavailable"), "{text}");
    assert!(text.contains(&missing), "{text}");
}

#[test]
fn pane_store_root_change_closes_panel_and_worker() {
    let mut app = app();
    app.state = State::Ready;
    let (store, _) = pipe_store();
    app.pipe_store = Some(store);
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
        &mut false,
    );
    wait_for(&mut app, |app| {
        app.pipes_panel
            .as_ref()
            .is_some_and(|panel| !panel.runs().is_empty())
    });
    assert!(app.pipes_worker.is_some());
    let replacement = pp_runtime::tools::PipeStore::new();
    drop(replacement.start_run("c".repeat(32), "root").expect("run"));
    let before = app.pipe_store_epoch;
    app.receive_event_for_test(WorkerEvent::PipeStoreBound { store: replacement });
    assert_eq!(app.pipe_store_epoch, before.wrapping_add(1));
    assert!(app.pipes_panel.is_none());
    assert!(app.pipes_worker.is_none());
}

#[test]
fn f8_does_not_override_an_ask_modal() {
    let mut app = app();
    let _ = ask_fixture(&mut app);
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
        &mut false,
    );
    assert!(app.pipes_panel.is_none());
    assert!(app.ask_panel.is_some());
}

#[test]
fn session_adoption_closes_pipes_panel() {
    let mut app = app();
    app.state = State::Ready;
    let (store, _) = pipe_store();
    app.pipe_store = Some(store);
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
        &mut false,
    );
    assert!(app.pipes_panel.is_some());
    app.adopt_history(Vec::new());
    assert!(app.pipes_panel.is_none());
    assert!(app.pipes_worker.is_none());
}

#[test]
fn pipes_keys_do_not_edit_input_or_leak_global_shortcuts() {
    let mut app = app();
    app.state = State::Ready;
    let (store, _) = pipe_store();
    app.pipe_store = Some(store);
    app.input.insert_str("draft");
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
        &mut false,
    );
    wait_for(&mut app, |app| {
        app.pipes_panel
            .as_ref()
            .is_some_and(|panel| !panel.runs().is_empty())
    });
    let cursor = app.input.cursor();
    for code in [
        KeyCode::Left,
        KeyCode::Right,
        KeyCode::Up,
        KeyCode::Down,
        KeyCode::PageUp,
        KeyCode::PageDown,
        KeyCode::Home,
        KeyCode::End,
        KeyCode::Char('f'),
        KeyCode::Char('r'),
        KeyCode::Char('x'),
    ] {
        press(&mut app, code);
    }
    assert_eq!(app.input_text(), "draft");
    assert_eq!(app.input.cursor(), cursor);
    assert!(app.pipes_panel.is_some());
    assert!(app.conversation.following());
}

#[test]
fn stale_root_or_view_replies_are_dropped() {
    let mut app = app();
    app.pipe_store_epoch = 3;
    app.pipes_view_epoch = 5;
    app.pipes_panel = Some(pipes::Panel::new(None));
    for (epoch, view) in [(2, 5), (3, 4)] {
        assert!(!app.apply_pipes_reply(pipes::Reply::Runs {
            epoch,
            view,
            runs: Vec::new(),
            selected_run: None,
            selected_stream: None,
        }));
    }
}
