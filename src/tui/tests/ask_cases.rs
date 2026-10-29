use super::*;

#[test]
fn ask_roundtrip_disconnect_and_cancelled_answer_return_typed_cancelled() {
    for cancel_before_reply in [false, true] {
        let (events, requests) = mpsc::sync_channel(1);
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let worker = thread::spawn(move || {
            let args = crate::interaction::AskArgs::parse(
                r#"{"questions":[{"id":"q","question":"Question","options":[]}]}"#,
            )
            .unwrap();
            request_ask(
                &events,
                crate::interaction::AskRequest {
                    call_id: "race".into(),
                    args: std::sync::Arc::new(args),
                },
                &worker_token,
            )
        });
        let WorkerEvent::Ask { reply, .. } = requests.recv_timeout(Duration::from_secs(1)).unwrap()
        else {
            panic!("ask request expected")
        };
        if cancel_before_reply {
            token.cancel();
            let _ = reply.send(vec![crate::interaction::AskAnswer {
                id: "q".into(),
                selected_options: Vec::new(),
                custom_input: Some("late".into()),
            }]);
        }
        drop(reply);
        let error = worker.join().unwrap().unwrap_err();
        assert!(error.is::<crate::cancellation::Cancelled>());
        assert!(token.is_cancelled());
    }
}

#[test]
fn ctrl_o_is_global_press_only_and_keeps_ask_and_main_draft() {
    let mut app = app();
    folding_fixture(&mut app);
    app.input.insert_str("主草稿🙂");
    app.input.move_cursor(CursorMove::Back);
    let cursor = app.input.cursor();
    let (answers, token) = ask_fixture(&mut app);
    let mut dirty = false;
    app.handle(key(KeyCode::Down), &mut dirty);
    let collapsed = draw(&mut app, 100, 100);
    assert!(collapsed.contains("Ctrl + O to expand"));
    app.handle(control('o'), &mut dirty);
    let expanded = draw(&mut app, 100, 100);
    for expected in ["first-19", "second-19", "Postgres"] {
        assert!(expanded.contains(expected), "{expanded}");
    }
    let mut repeat = KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL);
    repeat.kind = KeyEventKind::Repeat;
    app.handle(Event::Key(repeat), &mut dirty);
    repeat.kind = KeyEventKind::Release;
    app.handle(Event::Key(repeat), &mut dirty);
    assert!(!draw(&mut app, 100, 100).contains("to expand"));
    app.handle(control('o'), &mut dirty);
    assert!(draw(&mut app, 100, 100).contains("to expand"));
    assert_eq!(app.input.lines(), &["主草稿🙂"]);
    assert_eq!(app.input.cursor(), cursor);
    assert!(app.ask_panel.is_some());
    assert!(!token.is_cancelled());
    assert!(matches!(answers.try_recv(), Err(TryRecvError::Empty)));
    app.handle(key(KeyCode::Enter), &mut dirty);
    let response = answers.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(response[0].selected_options, ["Postgres"]);
}

#[test]
fn ctrl_o_keeps_normal_menu_and_alt_o_is_not_a_toggle() {
    let mut app = app();
    app.state = State::Ready;
    folding_fixture(&mut app);
    app.input.insert_str("/");
    app.refresh_candidates(true);
    let mut dirty = false;
    app.handle(key(KeyCode::Down), &mut dirty);
    let selected = app.menu.as_ref().unwrap().selected_index();
    let cursor = app.input.cursor();
    app.handle(control('o'), &mut dirty);
    assert_eq!(app.menu.as_ref().unwrap().selected_index(), selected);
    assert_eq!(app.input.cursor(), cursor);
    assert_eq!(app.input.lines(), &["/"]);
    assert!(!draw(&mut app, 100, 100).contains("to expand"));
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::ALT)),
        &mut dirty,
    );
    assert!(!draw(&mut app, 100, 100).contains("to expand"));
}

#[test]
fn ask_layout_keeps_latest_message_above_panel_on_resize() {
    let mut app = app();
    let (_answers, _token) = ask_fixture(&mut app);
    for i in 0..40 {
        app.append("Assistant", &format!("message {i}"));
    }
    app.append("Assistant", "latest message stays visible");
    app.input.insert_str("main draft");
    for (width, height) in [(100, 30), (40, 18), (100, 30)] {
        let screen = draw(&mut app, width, height);
        let rows: Vec<_> = screen.lines().collect();
        let message = rows
            .iter()
            .position(|row| row.contains("latest message stays visible"))
            .expect("最新消息不应被 ask 面板遮住");
        let panel = rows.iter().position(|row| row.contains("╭ Ask")).unwrap();
        assert!(message < panel, "消息 viewport 必须止于面板上方：{screen}");
        let bottom = rows.iter().position(|row| row.contains('╰')).unwrap();
        assert_eq!(bottom - panel + 1, 7, "两项选择、Other 与四行框架应占七行");
        assert!(screen.contains("main draft"));
    }
}

#[test]
fn ask_focus_submission_keeps_main_draft_and_sends_once() {
    let mut app = app();
    app.input.insert_str("主草稿🙂");
    app.input.move_cursor(CursorMove::Back);
    let cursor = app.input.cursor();
    let (answers, token) = ask_fixture(&mut app);
    let mut dirty = false;
    app.handle(function_key('p'), &mut dirty);
    app.handle(key(KeyCode::F(1)), &mut dirty);
    app.handle(Event::Paste("must stay inside panel".into()), &mut dirty);
    app.handle(Event::Resize(24, 10), &mut dirty);
    draw(&mut app, 24, 10);
    assert_eq!(app.input.lines(), &["主草稿🙂"]);
    assert_eq!(app.input.cursor(), cursor);
    assert!(matches!(answers.try_recv(), Err(TryRecvError::Empty)));
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    let response = answers.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(response[0].selected_options, ["Postgres"]);
    assert!(!token.is_cancelled());
    assert!(app.ask_panel.is_none());
    assert!(matches!(
        answers.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
    assert_eq!(app.input.cursor(), cursor);
}

#[test]
fn ask_cancel_exit_and_late_request_never_reopen_panel() {
    for exit in [false, true] {
        let mut app = app();
        let (answers, token) = ask_fixture(&mut app);
        if exit {
            app.close_ask_on_exit();
        } else {
            app.handle(key(KeyCode::Esc), &mut false);
        }
        assert!(token.is_cancelled());
        assert!(matches!(
            answers.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
        let args = crate::interaction::AskArgs::parse(
            r#"{"questions":[{"id":"late","question":"Late question","options":[]}]}"#,
        )
        .unwrap();
        let (reply, late) = mpsc::sync_channel(1);
        let (events, receiver) = mpsc::channel();
        app.events = Some(receiver);
        events
            .send(WorkerEvent::Ask {
                cancellation: CancellationToken::new(),
                request: crate::interaction::AskRequest {
                    call_id: "late".into(),
                    args: std::sync::Arc::new(args),
                },
                reply,
            })
            .unwrap();
        app.receive();
        assert!(app.ask_panel.is_none());
        assert!(matches!(late.try_recv(), Err(TryRecvError::Disconnected)));
    }
}
