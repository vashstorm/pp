use super::*;

fn pipe_store() -> (pp_bash::PipeStore, String) {
    let store = pp_bash::PipeStore::new();
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
fn pipes_command_f8_and_missing_run() {
    let (mut app, requests) = file_commands_app(commands::FileCommands::default());
    let (store, id) = pipe_store();
    app.pipe_store = Some(store);
    app.state = State::Running;
    app.input.insert_str("/pipes");
    press(&mut app, KeyCode::Enter);
    assert!(app.pipes_panel.is_some() && app.needs_alternate());
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
    assert!(app.pipes_panel.is_none() && !app.needs_alternate());

    let mut f8 = super::app();
    f8.state = State::Ready;
    let (store, _) = pipe_store();
    f8.pipe_store = Some(store);
    f8.input.insert_str("keep this draft");
    f8.handle(
        Event::Key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
        &mut false,
    );
    assert!(f8.pipes_panel.is_some() && f8.needs_alternate());
    wait_for(&mut f8, |app| {
        app.pipes_panel
            .as_ref()
            .is_some_and(|panel| !panel.runs().is_empty())
    });
    assert_eq!(f8.input_text(), "keep this draft");
    press(&mut f8, KeyCode::Esc);
    assert!(f8.pipes_panel.is_none() && f8.input_text() == "keep this draft");

    let (mut missing_app, _) = file_commands_app(commands::FileCommands::default());
    let (store, _) = pipe_store();
    missing_app.pipe_store = Some(store);
    let missing = "b".repeat(32);
    missing_app.input.insert_str(format!("/pipes {missing}"));
    press(&mut missing_app, KeyCode::Enter);
    wait_for(&mut missing_app, |app| {
        app.pipes_panel
            .as_ref()
            .is_some_and(|panel| panel.error().is_some())
    });
    let error = missing_app
        .pipes_panel
        .as_ref()
        .expect("pipes panel")
        .error()
        .expect("unavailable error");
    assert!(error.contains("unavailable"));
    let text = draw(&mut missing_app, 100, 20);
    assert!(
        text.contains("unavailable") && text.contains(&missing),
        "{text}"
    );
}

#[test]
fn pipes_root_change_adopt_key_guards_and_stale_replies() {
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
    let replacement = pp_bash::PipeStore::new();
    drop(replacement.start_run("c".repeat(32), "root").expect("run"));
    let before = app.pipe_store_epoch;
    app.receive_event_for_test(WorkerEvent::PipeStoreBound { store: replacement });
    assert_eq!(app.pipe_store_epoch, before.wrapping_add(1));
    assert!(app.pipes_panel.is_none() && app.pipes_worker.is_none());

    let (store, _) = pipe_store();
    app.pipe_store = Some(store);
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
        &mut false,
    );
    assert!(app.pipes_panel.is_some());
    app.adopt_history(Vec::new());
    assert!(app.pipes_panel.is_none() && app.pipes_worker.is_none());

    let mut keys = super::app();
    keys.state = State::Ready;
    let (store, _) = pipe_store();
    keys.pipe_store = Some(store);
    keys.input.insert_str("draft");
    keys.handle(
        Event::Key(KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
        &mut false,
    );
    wait_for(&mut keys, |app| {
        app.pipes_panel
            .as_ref()
            .is_some_and(|panel| !panel.runs().is_empty())
    });
    let cursor = keys.input.cursor();
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
        press(&mut keys, code);
    }
    assert_eq!(
        (keys.input_text().as_str(), keys.input.cursor()),
        ("draft", cursor)
    );
    assert!(keys.pipes_panel.is_some() && keys.conversation.following());

    let mut stale = super::app();
    stale.pipe_store_epoch = 3;
    stale.pipes_view_epoch = 5;
    stale.pipes_panel = Some(pipes::Panel::new(None));
    for (epoch, view) in [(2, 5), (3, 4)] {
        assert!(!stale.apply_pipes_reply(pipes::Reply::Runs {
            epoch,
            view,
            runs: Vec::new(),
            selected_run: None,
            selected_stream: None,
        }));
    }
}
