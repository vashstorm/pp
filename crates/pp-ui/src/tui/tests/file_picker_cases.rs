use super::*;

#[test]
fn file_picker_navigation_and_completion_keys() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new_with_skills(
        Err(anyhow::anyhow!("fixture")),
        pp_config::skills::Skills::default().shared(),
        commands::FileCommands::default(),
        dir.path(),
        None,
    )
    .unwrap();
    let (requests, scans) = mpsc::channel();
    let (replies, receiver) = mpsc::channel();
    app.file_worker = files::Worker {
        requests,
        replies: receiver,
    };
    let mut dirty = false;
    app.handle(Event::Paste("分析\n查看 @".into()), &mut dirty);
    scans.try_recv().unwrap();
    assert!(app.files_pending);
    app.handle(Event::Paste("main".into()), &mut dirty);
    assert!(scans.try_recv().is_err());
    replies
        .send(Ok(vec!["src/main.rs".into(), "src/menu.rs".into()]))
        .unwrap();
    app.receive();
    assert_eq!(menu_values(&app), [MenuAction::FileDirectory(0)]);
    assert_eq!(app.file_query.as_ref().unwrap().prefix, "main");
    app.state = State::Running;
    app.refresh_candidates(false);
    assert!(app.menu.is_some() && app.file_query.is_some());

    for confirm in [KeyCode::Enter, KeyCode::Tab, KeyCode::Right] {
        let (mut app, scans, replies, commands) = file_picker_fixture();
        let mut dirty = false;
        app.handle(
            Event::Paste("首行\n分析🙂 @src/old.rs 后续".into()),
            &mut dirty,
        );
        for _ in 0..9 {
            app.input.move_cursor(CursorMove::Back);
        }
        app.refresh_candidates(false);
        scans.try_recv().unwrap();
        replies.send(Ok(vec!["src/main.rs".into()])).unwrap();
        app.receive();
        app.handle(key(confirm), &mut dirty);
        assert_eq!(app.input.lines(), &["首行", "分析🙂 @src/ 后续"]);
        assert_eq!(app.input.cursor(), (1, 9));
        assert_eq!(app.file_directory, "src/");
        assert!(commands.try_recv().is_err());
        app.handle(key(confirm), &mut dirty);
        assert_eq!(app.input.lines(), &["首行", "分析🙂 @src/main.rs 后续"]);
        assert_eq!(app.input.cursor(), (1, 17));
        assert!(app.menu.is_none() && app.file_query.is_none());
        assert!(app.input_history.is_empty());
        assert!(commands.try_recv().is_err());
        assert!(matches!(app.state, State::Ready));
    }

    let (mut app, scans, replies, commands) = file_picker_fixture();
    let mut dirty = false;
    app.handle(Event::Paste("查看 @".into()), &mut dirty);
    scans.try_recv().unwrap();
    replies
        .send(Ok(vec![
            "README.md".into(),
            "docs/my folder/说明.md".into(),
            "src/main.rs".into(),
            "src/tui/menu.rs".into(),
        ]))
        .unwrap();
    app.receive();
    let labels = |app: &App| {
        app.menu
            .as_ref()
            .unwrap()
            .items()
            .iter()
            .map(|item| item.label.as_str())
            .collect::<Vec<_>>()
            .join(",")
    };
    assert_eq!(labels(&app), "docs/,src/,README.md");
    let rendered = draw(&mut app, 100, 30);
    assert_eq!(rendered.matches("README.md").count(), 1);
    assert!(!rendered.contains("Enter/Tab"));
    let cursor = app.input.cursor();
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert_eq!(app.input.lines(), &["查看 @src/"]);
    assert_eq!(app.input.cursor(), (0, 8));
    assert_eq!(app.file_directory, "src/");
    assert_eq!(labels(&app), "../,tui/,main.rs");
    app.handle(key(KeyCode::Right), &mut dirty);
    assert_eq!(app.file_directory, "src/tui/");
    assert_eq!(app.input.lines(), &["查看 @src/tui/"]);
    assert_eq!(app.input.cursor(), (0, 12));
    assert_eq!(labels(&app), "../,menu.rs");
    for code in [KeyCode::Left, KeyCode::Right] {
        let Event::Key(mut repeat) = key(code) else {
            unreachable!();
        };
        repeat.kind = KeyEventKind::Repeat;
        app.handle(Event::Key(repeat), &mut dirty);
        assert_eq!(app.file_directory, "src/tui/");
        assert_eq!(app.input.lines(), &["查看 @src/tui/"]);
    }
    app.handle(key(KeyCode::Left), &mut dirty);
    assert_eq!(app.file_directory, "src/");
    assert_eq!(app.input.lines(), &["查看 @src/"]);
    app.handle(key(KeyCode::Left), &mut dirty);
    assert!(app.file_directory.is_empty());
    assert_eq!(labels(&app), "docs/,src/,README.md");
    assert_eq!(app.input.lines(), &["查看 @"]);
    assert_eq!(app.input.cursor(), cursor);
    app.handle(key(KeyCode::Left), &mut dirty);
    assert_eq!(app.input.lines(), &["查看 @"]);
    assert_eq!(app.input.cursor(), cursor);
    assert!(app.input_history.is_empty());
    assert!(commands.try_recv().is_err());
    assert!(scans.try_recv().is_err());
    app.handle(key(KeyCode::Right), &mut dirty);
    assert_eq!(app.file_directory, "docs/");
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert_eq!(app.file_directory, "docs/my folder/");
    assert_eq!(app.input.lines(), &["查看 @\"docs/my folder/\""]);
    assert_eq!(app.input.cursor(), (0, 20));
    app.handle(Event::Paste("说明".into()), &mut dirty);
    assert_eq!(app.input.lines(), &["查看 @\"docs/my folder/说明\""]);
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &["查看 @\"docs/my folder/说明.md\" "]);
    assert!(app.menu.is_none());
    assert!(commands.try_recv().is_err());

    let (mut app, scans, replies, commands) = file_picker_fixture();
    let mut dirty = false;
    app.handle(Event::Paste("首行\n查看 @\"test".into()), &mut dirty);
    scans.try_recv().unwrap();
    replies
        .send(Ok(vec!["test 中文甲.md".into(), "test 中文申.md".into()]))
        .unwrap();
    app.receive();
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &["首行", "查看 @\"test 中文\""]);
    assert_eq!(app.input.cursor(), (1, 12));
    assert_eq!(
        menu_values(&app),
        [MenuAction::File(0), MenuAction::File(1)]
    );
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &["首行", "查看 @\"test 中文\""]);
    app.handle(key(KeyCode::Char('申')), &mut dirty);
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &["首行", "查看 @\"test 中文申.md\" "]);
    assert!(app.menu.is_none() && commands.try_recv().is_err());
}

#[test]
fn file_picker_reset_pending_and_cursor_history() {
    let (mut app, scans, replies, commands) = file_picker_fixture();
    let mut dirty = false;
    app.handle(Event::Paste("@".into()), &mut dirty);
    scans.try_recv().unwrap();
    replies
        .send(Ok(vec!["src/main.rs".into(), "src/tui/menu.rs".into()]))
        .unwrap();
    app.receive();
    app.handle(key(KeyCode::Tab), &mut dirty);
    app.handle(Event::Paste("menu".into()), &mut dirty);
    assert_eq!(
        menu_values(&app),
        [MenuAction::FileParent, MenuAction::FileDirectory(1)]
    );
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.file_directory, "src/tui/");
    assert_eq!(
        menu_values(&app),
        [MenuAction::FileParent, MenuAction::File(1)]
    );
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert_eq!(app.input.lines(), &["@src/tui/"]);
    app.handle(key(KeyCode::Backspace), &mut dirty);
    scans.try_recv().unwrap();
    assert!(app.file_directory.is_empty());
    replies.send(Ok(vec!["src/tui/menu.rs".into()])).unwrap();
    app.receive();
    assert_eq!(menu_values(&app), [MenuAction::FileDirectory(0)]);
    assert!(commands.try_recv().is_err());

    let (mut app, scans, replies, commands) = file_picker_fixture();
    let mut dirty = false;
    app.handle(Event::Paste("查看 @".into()), &mut dirty);
    scans.try_recv().unwrap();
    for code in [KeyCode::Enter, KeyCode::Tab, KeyCode::Right, KeyCode::Left] {
        app.handle(key(code), &mut dirty);
    }
    assert_eq!(app.input.lines(), &["查看 @"]);
    assert!(commands.try_recv().is_err());
    app.handle(key(KeyCode::Esc), &mut dirty);
    replies.send(Ok(vec!["new.rs".into()])).unwrap();
    app.receive();
    assert!(app.menu.is_none() && app.file_query.is_none());
    assert_eq!(app.input.lines(), &["查看 @"]);
    app.handle(key(KeyCode::Char('n')), &mut dirty);
    scans.try_recv().unwrap();
    replies.send(Ok(vec!["new.rs".into()])).unwrap();
    app.receive();
    assert_eq!(menu_values(&app), [MenuAction::File(0)]);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.handle(control('c'), &mut dirty));

    let (mut app, scans, replies, _) = file_picker_fixture();
    let mut dirty = false;
    app.handle(Event::Paste("查看 @".into()), &mut dirty);
    scans.try_recv().unwrap();
    replies
        .send(Ok(vec!["a.rs".into(), "b.rs".into()]))
        .unwrap();
    app.receive();
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(Event::Resize(20, 4), &mut dirty);
    draw(&mut app, 20, 4);
    app.handle(Event::Resize(100, 30), &mut dirty);
    app.refresh_candidates(false);
    assert_eq!(app.menu.as_ref().unwrap().selected_index(), Some(1));
    app.handle(control('a'), &mut dirty);
    assert!(app.file_query.is_none());
    app.handle(control('e'), &mut dirty);
    scans.try_recv().unwrap();
    app.handle(key(KeyCode::Left), &mut dirty);
    assert!(app.file_query.is_some());
    app.handle(key(KeyCode::Esc), &mut dirty);
    app.handle(key(KeyCode::Left), &mut dirty);
    assert!(app.file_query.is_none());
    app.handle(key(KeyCode::Right), &mut dirty);
    assert!(app.file_query.is_none());
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert!(app.file_query.is_some());
    assert!(scans.try_recv().is_err());
    app.handle(control('a'), &mut dirty);
    app.input_history.push("历史 @old".into());
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["历史 @old"]);
    assert!(app.file_query.is_none() && app.menu.is_none());
    app.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(app.input.lines(), &["查看 @"]);
    assert_eq!(app.input.cursor(), (0, 0));
    replies.send(Ok(vec!["a.rs".into()])).unwrap();
    app.receive();
    assert!(app.menu.is_none());
}

#[test]
fn file_picker_errors_and_suppression() {
    for result in [
        Ok(Vec::new()),
        Ok(vec!["src/other.rs".into()]),
        Err(anyhow::anyhow!("fixture scan failed")),
    ] {
        let (mut app, scans, replies, commands) = file_picker_fixture();
        let mut dirty = false;
        app.handle(Event::Paste("查看 @missing".into()), &mut dirty);
        scans.try_recv().unwrap();
        replies.send(result).unwrap();
        app.receive();
        assert!(app.menu.is_none());
        assert!(scans.try_recv().is_err());
        app.handle(key(KeyCode::Enter), &mut dirty);
        let deadline = Instant::now() + Duration::from_secs(2);
        while app.resources.job.is_some() && Instant::now() < deadline {
            app.receive();
            thread::yield_now();
        }
        assert!(app.resources.job.is_none());
        assert!(commands.try_recv().is_err());
        assert_eq!(app.input_text(), "查看 @missing");
    }

    let (mut app, scans, replies, commands) = file_picker_fixture();
    drop(replies);
    let mut dirty = false;
    app.handle(Event::Paste("@".into()), &mut dirty);
    scans.try_recv().unwrap();
    app.receive();
    assert!(matches!(app.state, State::Ready) && app.input.lines() == ["@"]);
    assert!(commands.try_recv().is_err());
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
    assert!(draw(&mut app, 100, 30).contains("File picker unavailable; restart pp."));

    let (mut app, scans, _replies, commands) = file_picker_fixture();
    drop(scans);
    let mut dirty = false;
    app.handle(Event::Paste("查看 @".into()), &mut dirty);
    assert!(matches!(app.state, State::Ready) && !app.files_pending);
    assert_eq!(app.input.lines(), &["查看 @"]);
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
    assert!(draw(&mut app, 100, 30).contains("File picker unavailable; restart pp."));
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(
        matches!(commands.try_recv(), Ok(WorkerCommand::Run { prompt: text, .. }) if text == "查看 @")
    );

    let (mut app, scans, replies, commands) = file_picker_fixture();
    let mut dirty = false;
    app.handle(Event::Paste("first\n@".into()), &mut dirty);
    scans.try_recv().unwrap();
    app.handle(key(KeyCode::Up), &mut dirty);
    assert!(app.file_query.is_none());
    replies.send(Ok(vec!["src/main.rs".into()])).unwrap();
    app.receive();
    assert!(app.menu.is_none());
    app.handle(key(KeyCode::Down), &mut dirty);
    scans.try_recv().unwrap();
    app.state = State::Running;
    app.refresh_candidates(false);
    replies.send(Ok(vec!["src/main.rs".into()])).unwrap();
    app.receive();
    assert!(app.menu.is_some() && app.file_query.is_some());
    assert_eq!(app.input.lines(), &["first", "@"]);
    assert!(commands.try_recv().is_err());
    app.state = State::Ready;
    app.clear_input();
    app.handle(Event::Paste("/model @".into()), &mut dirty);
    assert!(app.file_query.is_none() && scans.try_recv().is_err());
    app.handle(key(KeyCode::Esc), &mut dirty);
    app.clear_input();
    app.handle(Event::Paste("@".into()), &mut dirty);
    scans.try_recv().unwrap();
    app.handle(function_key('p'), &mut dirty);
    assert!(app.file_query.is_none());
    replies.send(Ok(vec!["src/main.rs".into()])).unwrap();
    app.receive();
    assert_eq!(app.input.lines(), &[""]);
    assert!(app.model_panel.is_some() && app.file_query.is_none());
}

#[test]
fn file_picker_templates_and_plan_focus() {
    for text in ["/review @", "/review first\n查看 @"] {
        let (mut app, scans, replies, commands) = file_picker_fixture();
        app.state = State::Running;
        app.file_commands = review_files();
        let mut dirty = false;
        app.handle(Event::Paste(text.into()), &mut dirty);
        scans.try_recv().unwrap();
        replies.send(Ok(vec!["README.md".into()])).unwrap();
        app.receive();
        assert_eq!(menu_values(&app), [MenuAction::File(0)]);
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines().join("\n"), format!("{text}README.md "));
        assert!(commands.try_recv().is_err());
        assert!(matches!(app.state, State::Running));
    }

    let (mut app, scans, replies, requests) = file_picker_fixture();
    app.workflow = ready_plan_workflow();
    app.plan_list_panel = Some(plan_list::Panel::new());
    app.plan_panel = Some(plan::Panel::review(plan_review_fixture(PlanStatus::Ready)));
    app.handle(Event::Paste("查看 @".into()), &mut false);
    assert!(app.completion_input_focused() && app.resource_input_available());
    scans.try_recv().unwrap();
    app.handle(key(KeyCode::BackTab), &mut false);
    assert!(!app.completion_input_focused());
    replies.send(Ok(vec!["README.md".into()])).unwrap();
    app.receive();
    assert!(app.menu.is_none() && app.file_query.is_none());
    app.handle(key(KeyCode::Tab), &mut false);
    assert!(app.completion_input_focused());
    scans.try_recv().unwrap();
    replies.send(Ok(vec!["src/README.md".into()])).unwrap();
    app.receive();
    assert_eq!(menu_values(&app), [MenuAction::FileDirectory(0)]);
    app.handle(key(KeyCode::Right), &mut false);
    assert_eq!(app.input.lines(), &["查看 @src/"]);
    app.handle(key(KeyCode::Left), &mut false);
    assert_eq!(app.input.lines(), &["查看 @"]);
    app.handle(key(KeyCode::Right), &mut false);
    app.handle(key(KeyCode::Right), &mut false);
    assert_eq!(app.input.lines(), &["查看 @src/README.md "]);
    assert!(app.plan_panel.is_some() && requests.try_recv().is_err());
    app.clear_input();
    app.handle(Event::Paste("/model something".into()), &mut false);
    assert!(app.model_panel.is_none() && app.menu.is_none());
    assert_eq!(app.input.lines(), &["/model something"]);

    let (mut app, scans, replies, requests) = file_picker_fixture();
    app.plan_panel = Some(plan::Panel::review(plan_review_fixture(PlanStatus::Ready)));
    app.handle(Event::Paste("@".into()), &mut false);
    scans.try_recv().unwrap();
    app.handle(key(KeyCode::Esc), &mut false);
    assert!(app.plan_panel.is_some());
    replies.send(Ok(vec!["README.md".into()])).unwrap();
    app.receive();
    assert!(app.menu.is_none());
    app.handle(key(KeyCode::Tab), &mut false);
    scans.try_recv().unwrap();
    replies.send(Ok(vec!["README.md".into()])).unwrap();
    app.receive();
    assert_eq!(menu_values(&app), [MenuAction::File(0)]);
    assert!(app.plan_panel.as_ref().unwrap().input_focused());
    assert!(requests.try_recv().is_err());
}

#[test]
fn file_picker_recall_and_stale_token_guard() {
    let (mut app, scans, replies, requests) = file_picker_fixture();
    app.input_queue
        .enqueue(PendingKind::FollowUp, "查看 @".into(), None, false)
        .unwrap();
    let id = app.input_queue.snapshot_since(None).unwrap().unwrap().items[0].id;
    app.recall_pending(id);
    scans.try_recv().unwrap();
    let (_answers, _cancellation) = ask_fixture(&mut app);
    replies.send(Ok(vec!["README.md".into()])).unwrap();
    app.receive();
    assert!(app.menu.is_none() && app.file_query.is_none());
    assert!(!app.resource_input_available());
    assert_eq!(app.input.lines(), &["查看 @"]);
    assert!(requests.try_recv().is_err());

    let (mut app, scans, replies, commands) = file_picker_fixture();
    let mut dirty = false;
    app.handle(Event::Paste("@a @b".into()), &mut dirty);
    scans.try_recv().unwrap();
    replies.send(Ok(vec!["b.rs".into()])).unwrap();
    app.receive();
    app.input.move_cursor(CursorMove::Head);
    app.input.move_cursor(CursorMove::Forward);
    app.apply_menu_action(MenuAction::File(0), true);
    assert_eq!(app.input.lines(), &["@a @b"]);
    assert_eq!(app.file_query.as_ref().unwrap().start, 0);
    assert!(commands.try_recv().is_err());
    app.apply_menu_action(MenuAction::File(usize::MAX), true);
    assert_eq!(app.input.lines(), &["@a @b"]);
}
