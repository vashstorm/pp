use super::*;

#[test]
fn file_picker_activation_filters_without_rescanning() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = App::new_with_skills(
        Err(anyhow::anyhow!("fixture")),
        crate::skills::Skills::default().shared(),
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
    assert!(app.menu.is_none());
    assert!(app.file_query.is_none());
}

#[test]
fn file_picker_enter_and_tab_replace_only_current_reference() {
    for confirm in [KeyCode::Enter, KeyCode::Tab] {
        let (mut app, scans, replies, commands) = file_picker_fixture();
        let mut dirty = false;
        app.handle(
            Event::Paste("首行\n分析🙂 @src/old.rs 后续".into()),
            &mut dirty,
        );
        for _ in 0..9 {
            app.handle(key(KeyCode::Left), &mut dirty);
        }
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
}

#[test]
fn file_picker_directory_navigation_completes_draft_without_submitting() {
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
    let cursor = app.input.cursor();
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert_eq!(app.input.lines(), &["查看 @src/"]);
    assert_eq!(app.input.cursor(), (0, 8));
    assert_eq!(app.file_directory, "src/");
    assert_eq!(labels(&app), "../,tui/,main.rs");
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.file_directory, "src/tui/");
    assert_eq!(app.input.lines(), &["查看 @src/tui/"]);
    assert_eq!(app.input.cursor(), (0, 12));
    assert_eq!(labels(&app), "../,menu.rs");
    app.handle(key(KeyCode::Home), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert_eq!(app.file_directory, "src/");
    assert_eq!(app.input.lines(), &["查看 @src/"]);
    app.handle(key(KeyCode::Home), &mut dirty);
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert!(app.file_directory.is_empty());
    assert_eq!(labels(&app), "docs/,src/,README.md");
    assert_eq!(app.input.lines(), &["查看 @"]);
    assert_eq!(app.input.cursor(), cursor);
    assert!(app.input_history.is_empty());
    assert!(commands.try_recv().is_err());
    assert!(scans.try_recv().is_err());
    app.handle(key(KeyCode::Tab), &mut dirty);
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
}

#[test]
fn file_picker_nested_filtering_and_reactivation_reset_directory() {
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
}

#[test]
fn file_picker_pending_blocks_confirmation_and_escape_ignores_late_reply() {
    let (mut app, scans, replies, commands) = file_picker_fixture();
    let mut dirty = false;
    app.handle(Event::Paste("查看 @".into()), &mut dirty);
    scans.try_recv().unwrap();
    app.handle(key(KeyCode::Enter), &mut dirty);
    app.handle(key(KeyCode::Tab), &mut dirty);
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
}

#[test]
fn file_picker_cursor_history_and_resize_preserve_draft_and_selection() {
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
    assert!(app.file_query.is_none());
    app.handle(key(KeyCode::Right), &mut dirty);
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
fn file_picker_empty_results_submit_raw_text_and_errors_preserve_worker() {
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
        assert!(
            matches!(commands.try_recv(), Ok(WorkerCommand::Run { prompt: text, .. }) if text == "查看 @missing")
        );
    }
    let (mut app, scans, replies, commands) = file_picker_fixture();
    drop(replies);
    let mut dirty = false;
    app.handle(Event::Paste("@".into()), &mut dirty);
    scans.try_recv().unwrap();
    app.receive();
    assert!(matches!(app.state, State::Ready));
    assert_eq!(app.input.lines(), &["@"]);
    assert!(commands.try_recv().is_err());
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
    assert!(draw(&mut app, 100, 30).contains("File picker unavailable; restart pp."));
}

#[test]
fn file_picker_request_disconnect_preserves_draft_and_agent() {
    let (mut app, scans, _replies, commands) = file_picker_fixture();
    drop(scans);
    let mut dirty = false;
    app.handle(Event::Paste("查看 @".into()), &mut dirty);
    assert!(matches!(app.state, State::Ready));
    assert!(!app.files_pending);
    assert_eq!(app.input.lines(), &["查看 @"]);
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
    assert!(draw(&mut app, 100, 30).contains("File picker unavailable; restart pp."));
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(
        matches!(commands.try_recv(), Ok(WorkerCommand::Run { prompt: text, .. }) if text == "查看 @")
    );
}

#[test]
fn file_picker_busy_slash_and_vertical_movement_do_not_reopen_files() {
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
    assert!(app.menu.is_none() && app.file_query.is_none());
    assert_eq!(app.input.lines(), &["first", "@"]);
    assert!(commands.try_recv().is_err());
    app.state = State::Ready;
    app.clear_input();
    app.handle(Event::Paste("/model @".into()), &mut dirty);
    assert!(app.file_query.is_none());
    assert!(scans.try_recv().is_err());
    app.handle(key(KeyCode::Esc), &mut dirty);
    app.clear_input();
    app.handle(Event::Paste("@".into()), &mut dirty);
    scans.try_recv().unwrap();
    app.handle(function_key('p'), &mut dirty);
    assert!(app.file_query.is_none());
    replies.send(Ok(vec!["src/main.rs".into()])).unwrap();
    app.receive();
    assert_eq!(app.input.lines(), &[""]);
    assert!(app.model_panel.is_some());
    assert!(app.file_query.is_none());
}

#[test]
fn file_picker_stale_action_cannot_replace_another_token() {
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
