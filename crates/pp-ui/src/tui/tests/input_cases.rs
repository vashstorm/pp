use super::*;

#[test]
fn input_history_multiline_and_rejection() {
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

    let (mut se, se_req) = file_commands_app(commands::FileCommands::default());
    let shift_enter = || Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    se.state = State::Ready;
    se.handle(Event::Paste("/eff".into()), &mut dirty);
    assert!(se.menu.is_some());
    se.handle(shift_enter(), &mut dirty);
    assert_eq!(se.input.lines(), &["/eff", ""]);
    assert!(se.menu.is_none() && se_req.try_recv().is_err() && se.input_history.is_empty());

    se.input = editor();
    se.state = State::Running;
    se.input_queue
        .enqueue(PendingKind::FollowUp, "queued".into(), None, false)
        .unwrap();
    se.handle(function_key('q'), &mut dirty);
    assert!(se.queue_menu);
    se.input.insert_str("draft");
    se.handle(shift_enter(), &mut dirty);
    assert!(se.queue_menu && se.has_pending());
    assert_eq!(se.input.lines(), &["draft", ""]);

    se.state = State::Ready;
    se.queue_menu = false;
    se.input = editor();
    se.input.insert_str("line");
    for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT | KeyModifiers::SHIFT] {
        se.handle(
            Event::Key(KeyEvent::new(KeyCode::Enter, modifiers)),
            &mut dirty,
        );
    }
    assert_eq!(se.input.lines(), &["line", "", ""]);
    assert!(se.input_history.is_empty() && se_req.try_recv().is_err());

    let mut rej = super::app();
    let (sender, _requests) = mpsc::channel();
    rej.commands = Some(sender);
    rej.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(rej.input.lines(), &[""]);
    for (state, text) in [
        (State::Ready, "  "),
        (State::Ready, "/unknown"),
        (State::Running, "/new"),
        (State::Cancelling, "cancelling draft"),
        (State::Unavailable, "unavailable draft"),
    ] {
        rej.state = state;
        rej.clear_input();
        rej.handle(Event::Paste(text.into()), &mut dirty);
        rej.handle(key(KeyCode::Enter), &mut dirty);
        assert!(rej.input_history.is_empty());
        assert_eq!(rej.input.lines(), &[text]);
    }
    rej.state = State::Ready;
    rej.clear_input();
    rej.handle(Event::Paste("/help".into()), &mut dirty);
    rej.handle(key(KeyCode::Enter), &mut dirty);
    rej.handle(Event::Paste("/new".into()), &mut dirty);
    rej.handle(key(KeyCode::Enter), &mut dirty);
    let (events, receiver) = mpsc::channel();
    rej.events = Some(receiver);
    rej.notify(Level::Error, "stale session feedback".into());
    events.send(WorkerEvent::Reset).unwrap();
    rej.receive();
    assert!(rej.show_welcome && matches!(rej.state, State::Ready) && rej.input.lines() == [""]);
    let screen = draw(&mut rej, 100, 30);
    assert!(screen.contains("▄▄▄██▀▀██"));
    assert!(
        !screen.contains("Session reset.")
            && !screen.contains("stale session feedback")
            && !screen.contains("╭")
    );
    rej.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(rej.input.lines(), &["/new"]);
    assert!(rej.menu.is_none());
    rej.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(rej.input.lines(), &["/help"]);
    rej.handle(key(KeyCode::Down), &mut dirty);
    rej.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(rej.input.lines(), &[""]);
}

#[test]
fn runtime_input_pending_queue_menu_and_resume() {
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
    assert!(!app.queue_menu && !app.has_pending() && app.input_history.is_empty());
    assert!(app.input_queue.recall(id).unwrap().is_none());
    app.input = editor();
    let id = app
        .input_queue
        .enqueue(PendingKind::Steering, "claimed".into(), None, false)
        .unwrap();
    app.toggle_pending_menu();
    let claimed = app.input_queue.claim_steering(false).unwrap();
    app.recall_pending(id);
    assert!(app.input.is_empty() && !app.queue_menu);
    assert_eq!(claimed.front().unwrap().source, "claimed");

    let (mut menu_app, _) = file_commands_app(commands::FileCommands::default());
    menu_app.state = State::Running;
    let token = CancellationToken::new();
    menu_app.active_cancellation = Some(token.clone());
    for text in ["first", "界\u{1b}[31m\tpreview", "last"] {
        menu_app
            .input_queue
            .enqueue(PendingKind::FollowUp, text.into(), None, false)
            .unwrap();
    }
    menu_app.handle(opposite_function_key('q'), &mut dirty);
    assert!(!menu_app.queue_menu);
    menu_app.handle(function_key('q'), &mut dirty);
    menu_app.handle(key(KeyCode::Down), &mut dirty);
    let selected = menu_app.menu.as_ref().unwrap().items()[1].value;
    menu_app
        .input_queue
        .enqueue(PendingKind::Steering, "priority".into(), None, false)
        .unwrap();
    menu_app.receive();
    let menu = menu_app.menu.as_ref().unwrap();
    assert_eq!(menu.items()[menu.selected_index().unwrap()].value, selected);
    for (width, height) in [(100, 30), (40, 10)] {
        let screen = draw(&mut menu_app, width, height);
        assert!(screen.contains("Steer #") && !screen.contains('\u{1b}'));
    }
    menu_app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(!menu_app.queue_menu && !token.is_cancelled() && !menu_app.queue_paused);
    menu_app.handle(function_key('q'), &mut dirty);
    menu_app.handle(Event::Paste("新草稿".into()), &mut dirty);
    assert!(!menu_app.queue_menu && menu_app.input.lines() == ["新草稿"]);
    assert!(!token.is_cancelled());

    let (mut res, requests) = file_commands_app(commands::FileCommands::default());
    res.input_queue
        .enqueue(PendingKind::FollowUp, "next task".into(), None, false)
        .unwrap();
    res.queue_paused = true;
    res.state = State::Ready;
    res.handle(opposite_function_key('r'), &mut dirty);
    assert!(res.queue_paused && requests.try_recv().is_err());
    res.input.insert_str("草稿");
    let cursor = res.input.cursor();
    for state in [State::Running, State::Cancelling, State::Unavailable] {
        res.state = state;
        res.handle(function_key('r'), &mut dirty);
        assert!(res.queue_paused && requests.try_recv().is_err());
    }
    res.state = State::Ready;
    res.handle(function_key('q'), &mut dirty);
    res.handle(function_key('r'), &mut dirty);
    assert!(!res.queue_paused && !res.queue_menu);
    assert!(
        matches!(requests.try_recv().unwrap(), WorkerCommand::Run { prompt, .. } if prompt == "next task")
    );
    assert_eq!(res.input.lines(), &["草稿"]);
    assert_eq!(res.input.cursor(), cursor);
    assert!(res.input_history.is_empty());
}

#[test]
fn runtime_input_scheduler_steering_and_command_guards() {
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
    events
        .send(WorkerEvent::completed(
            Instant::now(),
            Ok(RunOutcome::Completed),
        ))
        .unwrap();
    app.receive();
    assert!(matches!(app.state, State::Ready) && app.queue_paused && requests.try_recv().is_err());
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
    assert!(!next.is_cancelled() && app.input.cursor() == cursor);
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

    let (mut guarded, g_req) = file_commands_app(commands::FileCommands::default());
    guarded.state = State::Running;
    guarded.input.insert_str("one");
    guarded.handle(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
        &mut dirty,
    );
    guarded.handle(
        Event::Key(KeyEvent::new_with_kind(
            KeyCode::Enter,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )),
        &mut dirty,
    );
    assert_eq!(guarded.input.lines(), &["one"]);
    assert!(guarded.input_history.is_empty());
    guarded.handle(key(KeyCode::Enter), &mut dirty);
    guarded.handle(key(KeyCode::Enter), &mut dirty);
    assert_eq!(guarded.input_history, ["one"]);
    assert_eq!(
        guarded
            .input_queue
            .snapshot_since(None)
            .unwrap()
            .unwrap()
            .steering(),
        1
    );
    guarded.state = State::Ready;
    guarded.queue_paused = true;
    for command in ["/new", "/resume", "/tree"] {
        guarded.input = editor();
        guarded.input.insert_str(command);
        let cursor = guarded.input.cursor();
        guarded.submit(PendingKind::Steering);
        assert_eq!(guarded.input.lines(), &[command]);
        assert_eq!(guarded.input.cursor(), cursor);
        assert!(matches!(guarded.state, State::Ready) && g_req.try_recv().is_err());
    }
    guarded.input = editor();
    guarded.handle(key(KeyCode::Esc), &mut dirty);
    guarded.handle(key(KeyCode::Esc), &mut dirty);
    assert!(guarded.tree_panel.is_none() && g_req.try_recv().is_err());
}

#[test]
fn input_wrapping_height_and_native_cursor() {
    let mut arrow_app = app();
    arrow_app.input_history.push("history".into());
    let text = "abcdefghijklmnopqrstuvwxyz0123456789!";
    let mut dirty = false;
    arrow_app.handle(Event::Paste(text.into()), &mut dirty);
    draw(&mut arrow_app, 14, 12);
    let cursor = arrow_app.input.cursor();
    let row = arrow_app.input.screen_cursor().row;
    assert!(row > 0);
    arrow_app.handle(key(KeyCode::Up), &mut dirty);
    draw(&mut arrow_app, 14, 12);
    assert_eq!(arrow_app.input.screen_cursor().row, row - 1);
    assert!(arrow_app.history_index.is_none());
    arrow_app.handle(key(KeyCode::Down), &mut dirty);
    draw(&mut arrow_app, 14, 12);
    assert_eq!(arrow_app.input.cursor(), cursor);
    assert_eq!(arrow_app.input.lines(), &[text]);
    assert!(arrow_app.history_index.is_none());

    for (text, width, rows) in [
        ("", 14, 1),
        ("abcdefghijkl", 14, 1),
        ("abcdefghijklmnopqrstuvwxyz", 14, 3),
        ("hello world again", 14, 2),
        ("中文测试中文测试中文测试中", 14, 3),
        ("🙂🙂🙂🙂🙂🙂🙂", 14, 2),
        ("e\u{301}".repeat(13).as_str(), 14, 2),
        ("a\tb\tc\td", 14, 2),
        ("abcdefghijklmnop\n\nlast", 14, 4),
        ("abcdefghijklmnopqrstuvwxyz", 9, 4),
    ] {
        let mut a = app();
        a.handle(Event::Paste(text.into()), &mut dirty);
        let cursor = a.input.cursor();
        let screen = draw(&mut a, width, 16);
        assert_eq!(
            a.main_layout(Rect::new(0, 0, width, 16))
                .conversation
                .height,
            13 - rows,
            "the input box should grow by the number of wrapped rows: {text:?}"
        );
        assert_eq!(a.input.cursor(), cursor);
        assert_eq!(a.input_text(), text);
        assert_eq!(a.input.scroll_offset(), (0, 0));
        if text == "abcdefghijklmnopqrstuvwxyz" && width == 14 {
            assert!(
                screen.contains("❯ abcdefghijkl")
                    && screen.contains("  mnopqrstuvwx")
                    && screen.contains("  yz")
            );
        }
    }

    let mut capped = app();
    let long = "a".repeat(100);
    capped.handle(Event::Paste(long.clone()), &mut dirty);
    draw(&mut capped, 14, 16);
    assert_eq!(
        capped
            .main_layout(Rect::new(0, 0, 14, 16))
            .conversation
            .height,
        7
    );
    assert_eq!(capped.input.scroll_offset(), (3, 0));
    capped.input.move_cursor(CursorMove::Jump(0, 0));
    capped.input.start_selection();
    capped.input.move_cursor(CursorMove::Jump(0, 24));
    let selection = capped.input.selection_range();
    draw(&mut capped, 14, 16);
    assert_eq!(capped.input.selection_range(), selection);
    assert_eq!(capped.input.scroll_offset(), (2, 0));
    capped.input.cancel_selection();
    capped.handle(control('a'), &mut dirty);
    capped.handle(control('k'), &mut dirty);
    draw(&mut capped, 14, 16);
    assert_eq!(capped.input_text(), "");
    assert_eq!(
        capped
            .main_layout(Rect::new(0, 0, 14, 16))
            .conversation
            .height,
        12
    );
    assert_eq!(capped.input.scroll_offset(), (0, 0));
    capped.handle(control('u'), &mut dirty);
    draw(&mut capped, 14, 16);
    assert_eq!(capped.input_text(), long);
    assert_eq!(
        capped
            .main_layout(Rect::new(0, 0, 14, 16))
            .conversation
            .height,
        7
    );

    for (text, width, height, expected) in [
        ("", 100, 30, (2, 27)),
        ("中🙂h", 100, 30, (7, 27)),
        ("first\n中🙂h", 100, 30, (7, 27)),
        ("0\n1\n2\n3\n4\n5\n中🙂h", 100, 30, (7, 27)),
    ] {
        let mut a = app();
        a.input.insert_str(text);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| a.draw(frame, &StatusContext::default()))
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

    let mut resize = app();
    resize.input.insert_str("abcdefghijklmnopqrstuvwxyz");
    for (width, column, top) in [(14, 4, 0), (40, 28, 0), (9, 7, 0)] {
        let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
        terminal
            .draw(|frame| resize.draw(frame, &StatusContext::default()))
            .unwrap();
        assert_eq!(terminal.backend().cursor_position(), (column, 9).into());
        assert_eq!(resize.input.scroll_offset(), (top, 0));
    }
}

#[test]
fn input_control_keys_and_menu_history() {
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
    assert_eq!(app.input.lines(), &["first", "中🙂abclast"]);
    assert_eq!(app.input.cursor(), (1, 5));
    app.input.move_cursor(CursorMove::Jump(1, 2));
    app.handle(control('k'), &mut dirty);
    assert_eq!(app.input.lines(), &["first", "中🙂"]);
    assert_eq!(app.input.cursor(), (1, 2));
    app.handle(control('a'), &mut dirty);
    app.handle(control('k'), &mut dirty);
    assert_eq!(app.input.lines(), &["first", ""]);
    app.handle(control('k'), &mut dirty);
    assert_eq!(app.input.lines(), &["first", ""]);

    let mut hist = super::app();
    hist.input_history.push("older task".into());
    hist.handle(Event::Paste("/".into()), &mut dirty);
    hist.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(hist.menu.as_ref().unwrap().selected_index(), Some(1));
    hist.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(hist.input.lines(), &["/"]);
    assert!(hist.history_index.is_none());
    hist.handle(control('a'), &mut dirty);
    assert_eq!(hist.input.cursor(), (0, 0));
    hist.handle(control('e'), &mut dirty);
    assert_eq!(hist.input.cursor(), (0, 1));
    hist.handle(control('a'), &mut dirty);
    hist.handle(control('k'), &mut dirty);
    assert_eq!(hist.input.lines(), &[""]);
    assert!(hist.menu.is_none());
    hist.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(hist.input.lines(), &["older task"]);
}
