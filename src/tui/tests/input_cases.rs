use super::*;

#[test]
fn input_history_copies_submissions_and_restores_multiline_draft() {
    let mut app = app();
    let (sender, requests) = mpsc::channel();
    app.commands = Some(sender);
    let mut dirty = false;
    for prompt in ["first-中", "second\n🙂末行"] {
        app.state = State::Ready;
        app.handle(Event::Paste(prompt.into()), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt: text, .. }) if text == prompt)
        );
    }
    app.handle(Event::Paste("草稿\n未发送🙂".into()), &mut dirty);
    app.handle(key(KeyCode::Left), &mut dirty);
    let cursor = app.input.cursor();
    app.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(app.input.cursor(), cursor);
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["草稿", "未发送🙂"]);
    assert_eq!(app.input.cursor(), (0, 2));
    assert!(app.history_index.is_none());
    let cursor = app.input.cursor();
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["second", "🙂末行"]);
    app.handle(key(KeyCode::Char('!')), &mut dirty);
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["second", "🙂末行!"]);
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["first-中"]);
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["first-中"]);
    app.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(app.input.lines(), &["second", "🙂末行"]);
    app.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(app.input.lines(), &["草稿", "未发送🙂"]);
    assert_eq!(app.input.cursor(), cursor);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));

    app.state = State::Ready;
    app.handle(key(KeyCode::Up), &mut dirty);
    app.handle(key(KeyCode::Char('!')), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt: text, .. }) if text == "second\n🙂末行!")
    );
    assert_eq!(
        app.input_history,
        ["first-中", "second\n🙂末行", "second\n🙂末行!"]
    );
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["second", "🙂末行!"]);
    app.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(app.input.lines(), &[""]);
}

#[test]
fn input_history_ignores_rejected_submissions_and_survives_reset() {
    let mut app = app();
    let (sender, _requests) = mpsc::channel();
    app.commands = Some(sender);
    let mut dirty = false;
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &[""]);
    for (state, text) in [
        (State::Ready, "  "),
        (State::Ready, "/unknown"),
        (State::Running, "/new"),
        (State::Cancelling, "cancelling draft"),
        (State::Unavailable, "unavailable draft"),
    ] {
        app.state = state;
        app.clear_input();
        app.handle(Event::Paste(text.into()), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(app.input_history.is_empty());
        assert_eq!(app.input.lines(), &[text]);
    }
    app.state = State::Ready;
    app.clear_input();
    app.handle(Event::Paste("/help".into()), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    app.handle(Event::Paste("/new".into()), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    let (events, receiver) = mpsc::channel();
    app.events = Some(receiver);
    app.notify(Level::Error, "stale session feedback".into());
    events.send(WorkerEvent::Reset).unwrap();
    app.receive();
    assert!(app.show_welcome);
    assert!(matches!(app.state, State::Ready));
    assert_eq!(app.input.lines(), &[""]);
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("▄▄▄██▀▀██"));
    assert!(!screen.contains("Session reset."));
    assert!(!screen.contains("stale session feedback"));
    assert!(!screen.contains("╭"));
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["/new"]);
    assert!(app.menu.is_none());
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["/help"]);
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(app.input.lines(), &[""]);
}

#[test]
fn runtime_input_pending_recall_protects_draft_and_never_reuses_claimed_text() {
    let (mut app, _requests) = file_commands_app(commands::FileCommands::default());
    app.state = State::Running;
    app.input_queue.begin_run().unwrap();
    let id = app
        .input_queue
        .enqueue(
            PendingKind::FollowUp,
            "/review 原文\n".into(),
            Some("expanded once".into()),
            false,
        )
        .unwrap();
    app.input.insert_str(" ");
    let cursor = app.input.cursor();
    let mut dirty = false;
    app.handle(function_key('q'), &mut dirty);
    assert!(app.queue_menu);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert_eq!(app.input.lines(), &[" "]);
    assert_eq!(app.input.cursor(), cursor);
    assert!(app.has_pending());
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert!(app.has_pending());
    app.input = editor();
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert_eq!(app.input.lines(), &["/review 原文", ""]);
    assert!(!app.queue_menu);
    assert!(!app.has_pending());
    assert!(app.input_history.is_empty());
    assert!(app.input_queue.recall(id).unwrap().is_none());
    app.input = editor();
    let id = app
        .input_queue
        .enqueue(PendingKind::Steering, "claimed".into(), None, false)
        .unwrap();
    app.toggle_pending_menu();
    let claimed = app.input_queue.claim_steering(false).unwrap();
    app.recall_pending(id);
    assert!(app.input.is_empty());
    assert!(!app.queue_menu);
    assert_eq!(claimed.front().unwrap().source, "claimed");
}

#[test]
fn runtime_input_pending_menu_preserves_selection_and_escape_does_not_cancel() {
    let (mut app, _requests) = file_commands_app(commands::FileCommands::default());
    app.state = State::Running;
    let token = CancellationToken::new();
    app.active_cancellation = Some(token.clone());
    for text in ["first", "界\u{1b}[31m\tpreview", "last"] {
        app.input_queue
            .enqueue(PendingKind::FollowUp, text.into(), None, false)
            .unwrap();
    }
    let mut dirty = false;
    app.handle(opposite_function_key('q'), &mut dirty);
    assert!(!app.queue_menu);
    app.handle(function_key('q'), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    let selected = app.menu.as_ref().unwrap().items()[1].value;
    app.input_queue
        .enqueue(PendingKind::Steering, "priority".into(), None, false)
        .unwrap();
    app.receive();
    let menu = app.menu.as_ref().unwrap();
    assert_eq!(menu.items()[menu.selected_index().unwrap()].value, selected);
    for (width, height) in [(100, 30), (40, 10)] {
        let screen = draw(&mut app, width, height);
        assert!(screen.contains("Steer #"));
        assert!(!screen.contains('\u{1b}'));
    }
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(!app.queue_menu);
    assert!(!token.is_cancelled());
    assert!(!app.queue_paused);
    app.handle(function_key('q'), &mut dirty);
    app.handle(Event::Paste("新草稿".into()), &mut dirty);
    assert!(!app.queue_menu);
    assert_eq!(app.input.lines(), &["新草稿"]);
    assert!(!token.is_cancelled());
}

#[test]
fn runtime_input_explicit_resume_preserves_draft_and_only_ready_dispatches() {
    let (mut app, requests) = file_commands_app(commands::FileCommands::default());
    app.input_queue
        .enqueue(PendingKind::FollowUp, "next task".into(), None, false)
        .unwrap();
    app.queue_paused = true;
    app.state = State::Ready;
    let mut dirty = false;
    app.handle(opposite_function_key('r'), &mut dirty);
    assert!(app.queue_paused);
    assert!(requests.try_recv().is_err());
    app.input.insert_str("草稿");
    let cursor = app.input.cursor();
    for state in [State::Running, State::Cancelling, State::Unavailable] {
        app.state = state;
        app.handle(function_key('r'), &mut dirty);
        assert!(app.queue_paused);
        assert!(requests.try_recv().is_err());
    }
    app.state = State::Ready;
    app.handle(function_key('q'), &mut dirty);
    app.handle(function_key('r'), &mut dirty);
    assert!(!app.queue_paused);
    assert!(!app.queue_menu);
    assert!(
        matches!(requests.try_recv().unwrap(), WorkerCommand::Run { prompt, .. } if prompt == "next task")
    );
    assert_eq!(app.input.lines(), &["草稿"]);
    assert_eq!(app.input.cursor(), cursor);
    assert!(app.input_history.is_empty());
}

#[test]
fn runtime_input_scheduler_prioritizes_steering_and_pauses_on_cancel_race() {
    let (mut app, requests) = file_commands_app(commands::FileCommands::default());
    let (events, receiver) = mpsc::channel();
    app.events = Some(receiver);
    let mut dirty = false;
    app.input.insert_str("initial");
    app.handle(key(KeyCode::Enter), &mut dirty);
    let WorkerCommand::Run {
        cancellation: initial,
        ..
    } = requests.try_recv().unwrap()
    else {
        panic!()
    };
    app.input.insert_str("queued");
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
        &mut dirty,
    );
    app.input.insert_str("steer");
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(app.input.is_empty());
    assert_eq!(app.input_history, ["initial", "queued", "steer"]);
    assert!(requests.try_recv().is_err());
    app.input.insert_str("草稿\nline");
    let cursor = app.input.cursor();
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(initial.is_cancelled());
    // A committed final reply can win cancellation, but must not resume the queue.
    events
        .send(WorkerEvent::completed(
            Instant::now(),
            Ok(RunOutcome::Completed),
        ))
        .unwrap();
    app.receive();
    assert!(matches!(app.state, State::Ready));
    assert!(app.queue_paused);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.input.cursor(), cursor);
    assert_eq!(app.input.lines(), &["草稿", "line"]);
    app.queue_paused = false;
    app.dispatch_next(true);
    let WorkerCommand::Run {
        prompt,
        cancellation: next,
        ..
    } = requests.try_recv().unwrap()
    else {
        panic!()
    };
    assert_eq!(prompt, "steer");
    assert!(!next.is_cancelled());
    assert_eq!(app.input.cursor(), cursor);
    events
        .send(WorkerEvent::completed(
            Instant::now(),
            Ok(RunOutcome::Completed),
        ))
        .unwrap();
    app.receive();
    assert!(
        matches!(requests.try_recv().unwrap(), WorkerCommand::Run { prompt, .. } if prompt == "queued")
    );
    assert_eq!(app.input.lines(), &["草稿", "line"]);
    assert_eq!(app.input_history, ["initial", "queued", "steer"]);
}

#[test]
fn runtime_input_modified_and_repeated_enter_preserve_draft_and_history_guard() {
    let (mut app, requests) = file_commands_app(commands::FileCommands::default());
    app.state = State::Running;
    let mut dirty = false;
    app.input.insert_str("one");
    for modifiers in [
        KeyModifiers::CONTROL,
        KeyModifiers::SHIFT,
        KeyModifiers::ALT | KeyModifiers::SHIFT,
    ] {
        app.handle(
            Event::Key(KeyEvent::new(KeyCode::Enter, modifiers)),
            &mut dirty,
        );
    }
    app.handle(
        Event::Key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )),
        &mut dirty,
    );
    assert_eq!(app.input.lines(), &["one"]);
    assert!(app.input_history.is_empty());
    app.handle(key(KeyCode::Enter), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert_eq!(app.input_history, ["one"]);
    assert_eq!(
        app.input_queue
            .snapshot_since(None)
            .unwrap()
            .unwrap()
            .steering(),
        1
    );
    app.state = State::Ready;
    app.queue_paused = true;
    for command in ["/new", "/resume", "/tree"] {
        app.input = editor();
        app.input.insert_str(command);
        let cursor = app.input.cursor();
        app.submit(PendingKind::Steering);
        assert_eq!(app.input.lines(), &[command]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(matches!(app.state, State::Ready));
        assert!(requests.try_recv().is_err());
    }
    app.input = editor();
    app.handle(key(KeyCode::Esc), &mut dirty);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.tree_panel.is_none());
    assert!(requests.try_recv().is_err());
}

#[test]
fn input_arrows_move_between_wrapped_lines_before_history() {
    let mut app = app();
    app.input_history.push("history".into());
    let text = "abcdefghijklmnopqrstuvwxyz0123456789!";
    let mut dirty = false;
    app.handle(Event::Paste(text.into()), &mut dirty);
    draw(&mut app, 14, 12);
    let cursor = app.input.cursor();
    let row = app.input.screen_cursor().row;
    assert!(row > 0);

    app.handle(key(KeyCode::Up), &mut dirty);
    draw(&mut app, 14, 12);
    assert_eq!(app.input.screen_cursor().row, row - 1);
    assert!(app.history_index.is_none());
    app.handle(key(KeyCode::Down), &mut dirty);
    draw(&mut app, 14, 12);
    assert_eq!(app.input.cursor(), cursor);
    assert_eq!(app.input.lines(), &[text]);
    assert!(app.history_index.is_none());
}

#[test]
fn input_native_cursor_follows_unicode_wrapping_scrolling_and_resize() {
    for (text, width, height, expected) in [
        ("", 100, 30, (2, 27)),
        ("中🙂h", 100, 30, (7, 27)),
        ("first\n中🙂h", 100, 30, (7, 27)),
        ("0\n1\n2\n3\n4\n5\n中🙂h", 100, 30, (7, 27)),
    ] {
        let mut app = app();
        app.input.insert_str(text);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, &StatusContext::default()))
            .unwrap();
        assert!(terminal.backend().cursor_visible());
        assert_eq!(terminal.backend().cursor_position(), expected.into());
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| !cell.modifier.contains(Modifier::REVERSED))
        );
    }

    let mut app = app();
    app.input.insert_str("abcdefghijklmnopqrstuvwxyz");
    for (width, column, top) in [(14, 4, 2), (40, 28, 0), (9, 7, 3)] {
        let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
        terminal
            .draw(|frame| app.draw(frame, &StatusContext::default()))
            .unwrap();
        assert_eq!(terminal.backend().cursor_position(), (column, 9).into());
        assert_eq!(app.input.scroll_offset(), (top, 0));
    }
}

#[test]
fn input_control_keys_respect_unicode_and_line_boundaries() {
    let mut app = app();
    let mut dirty = false;
    app.handle(Event::Paste("first\n中🙂abc\nlast".into()), &mut dirty);
    app.input.move_cursor(CursorMove::Jump(1, 2));
    app.handle(control('a'), &mut dirty);
    app.handle(control('a'), &mut dirty);
    assert_eq!(app.input.cursor(), (1, 0));
    app.handle(control('e'), &mut dirty);
    app.handle(control('e'), &mut dirty);
    assert_eq!(app.input.cursor(), (1, 5));
    app.handle(control('k'), &mut dirty);
    assert_eq!(app.input.lines(), &["first", "中🙂abc", "last"]);
    app.input.move_cursor(CursorMove::Jump(1, 2));
    app.handle(control('k'), &mut dirty);
    assert_eq!(app.input.lines(), &["first", "中🙂", "last"]);
    assert_eq!(app.input.cursor(), (1, 2));
    app.handle(control('a'), &mut dirty);
    app.handle(control('k'), &mut dirty);
    assert_eq!(app.input.lines(), &["first", "", "last"]);
    app.handle(control('k'), &mut dirty);
    assert_eq!(app.input.lines(), &["first", "", "last"]);
}

#[test]
fn input_history_yields_to_menus_and_control_edits_refresh_candidates() {
    let mut app = app();
    app.input_history.push("older task".into());
    let mut dirty = false;
    app.handle(Event::Paste("/".into()), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(app.menu.as_ref().unwrap().selected_index(), Some(1));
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["/"]);
    assert!(app.history_index.is_none());
    app.handle(control('a'), &mut dirty);
    assert_eq!(app.input.cursor(), (0, 0));
    app.handle(control('e'), &mut dirty);
    assert_eq!(app.input.cursor(), (0, 1));
    app.handle(control('a'), &mut dirty);
    app.handle(control('k'), &mut dirty);
    assert_eq!(app.input.lines(), &[""]);
    assert!(app.menu.is_none());
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["older task"]);
}
