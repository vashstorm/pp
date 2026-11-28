use super::*;

#[test]
fn task_cancel_idle_menu_repeat_and_modified_escape_do_not_cancel() {
    let mut app = app();
    app.state = State::Ready;
    app.input.insert_str("保留草稿");
    let cursor = app.input.cursor();
    let mut dirty = false;
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert_eq!(app.input.lines(), &["保留草稿"]);
    assert_eq!(app.input.cursor(), cursor);
    assert!(app.active_cancellation.is_none());
    app.input = editor();
    app.input.insert_str("/");
    app.refresh_candidates(true);
    assert!(app.menu.is_some());
    let token = CancellationToken::new();
    app.active_cancellation = Some(token.clone());
    app.state = State::Running;
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.menu.is_none());
    assert!(!token.is_cancelled());
    app.handle(
        Event::Key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Repeat,
        )),
        &mut dirty,
    );
    app.handle(
        Event::Key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        )),
        &mut dirty,
    );
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::ALT)),
        &mut dirty,
    );
    assert!(!token.is_cancelled());
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(token.is_cancelled());
    assert!(matches!(app.state, State::Cancelling));
}

#[test]
fn bang_shell_input_history_exits_and_cleanup_failure() {
    let mut app = app();
    app.state = State::Ready;
    let (commands, requests) = mpsc::channel();
    app.commands = Some(commands);
    let mut dirty = false;
    app.handle(key(KeyCode::Char('!')), &mut dirty);
    assert!(app.shell_mode && app.input.lines() == [""]);
    assert!(
        draw_buffer(&mut app, 100, 30)
            .content
            .iter()
            .any(|cell| cell.symbol() == "─" && cell.fg == statusline::shell_mode())
    );
    app.submit(PendingKind::Steering);
    assert!(requests.try_recv().is_err());
    app.handle(key(KeyCode::Char('!')), &mut dirty);
    app.handle(Event::Paste(" false".into()), &mut dirty);
    assert_eq!(app.input.lines(), &["! false"]);
    app.handle(key(KeyCode::Enter), &mut dirty);
    let WorkerCommand::Shell { command, .. } = requests.try_recv().unwrap() else {
        panic!("should submit a local shell command");
    };
    assert_eq!(command, "! false");
    assert_eq!(app.input_history, ["!! false"]);
    assert!(!app.shell_mode && app.input.lines() == [""]);

    let mut paste = super::app();
    paste.state = State::Ready;
    paste.handle(
        Event::Paste("  !printf '你好!'\n! false".into()),
        &mut dirty,
    );
    assert!(paste.shell_mode && paste.input.lines() == ["  printf '你好!'", "! false"]);
    assert_eq!(paste.input.cursor(), ratatui_textarea::DataCursor(1, 7));
    assert!(paste.input.undo() && paste.input.lines() == [""]);
    assert!(paste.input.redo() && paste.input.lines() == ["  printf '你好!'", "! false"]);
    paste.input_history.push("ordinary prompt".into());
    paste.navigate_history(true);
    assert!(!paste.shell_mode);
    paste.navigate_history(false);
    assert!(paste.shell_mode && paste.input.lines() == ["  printf '你好!'", "! false"]);
    paste.open_model_picker();
    assert!(!paste.shell_mode);
    paste.close_model_panel();
    assert!(paste.shell_mode);
    paste.remember_input();
    paste.clear_input();
    paste.navigate_history(true);
    assert!(paste.shell_mode && paste.input.lines() == ["  printf '你好!'", "! false"]);

    let mut exits = super::app();
    exits.state = State::Ready;
    exits.handle(key(KeyCode::Char('!')), &mut dirty);
    exits.handle(key(KeyCode::Backspace), &mut dirty);
    assert!(!exits.shell_mode);
    exits.handle(Event::Paste("!/model @file".into()), &mut dirty);
    assert!(
        exits.shell_mode
            && exits.menu.is_none()
            && exits.model_panel.is_none()
            && exits.file_query.is_none()
    );
    exits.handle(key(KeyCode::Esc), &mut dirty);
    assert!(!exits.shell_mode && exits.input.lines() == ["/model @file"]);
    exits.clear_input();
    exits.handle(Event::Paste("!echo ok".into()), &mut dirty);
    exits.handle(control('a'), &mut dirty);
    exits.handle(control('k'), &mut dirty);
    assert!(!exits.shell_mode && exits.input.lines() == [""]);

    let mut cleanup = super::app();
    cleanup.state = State::RunningShell;
    cleanup.input.insert_str("draft remains");
    let (commands, requests) = mpsc::channel();
    let (events, receiver) = mpsc::channel();
    cleanup.commands = Some(commands);
    cleanup.events = Some(receiver);
    events
        .send(WorkerEvent::ShellCompleted(Err(
            pp_core::cancellation::CleanupError(anyhow::anyhow!("process group still alive"))
                .into(),
        )))
        .unwrap();
    drop(events);
    cleanup.receive();
    assert!(matches!(cleanup.state, State::Unavailable));
    assert_eq!(cleanup.input.lines(), &["draft remains"]);
    assert_eq!(
        cleanup.unavailable.as_deref(),
        Some("Execution unavailable: shell cleanup failed; restart pp.")
    );
    assert!(draw(&mut cleanup, 100, 30).contains("process group still alive"));
    cleanup.submit(PendingKind::Steering);
    assert!(requests.try_recv().is_err());
}

#[cfg(debug_assertions)]
#[test]
fn test_web_search_dispatches_cancels_and_restores_state() {
    let mut app = app();
    app.state = State::Ready;
    let (commands, requests) = mpsc::channel();
    let (events, receiver) = mpsc::channel();
    app.commands = Some(commands);
    app.events = Some(receiver);

    app.input
        .insert_str("/test-web-search --provider hackernews rust");
    app.submit(PendingKind::Steering);
    assert!(matches!(app.state, State::RunningWebSearch));
    assert_eq!(app.input.lines(), &[""]);
    let WorkerCommand::TestWebSearch {
        queries,
        provider,
        cancellation,
        ..
    } = requests.try_recv().unwrap()
    else {
        panic!("expected WorkerCommand::TestWebSearch");
    };
    assert_eq!(queries, vec!["rust"]);
    assert_eq!(
        provider,
        Some(pp_config::config::WebSearchProvider::Hackernews)
    );

    // Esc cancels the in-flight diagnostic search without corrupting input.
    let mut dirty = false;
    app.input.insert_str("keep-draft");
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(cancellation.is_cancelled());
    assert!(matches!(app.state, State::RunningWebSearch));

    events
        .send(WorkerEvent::WebSearchCompleted(Ok(())))
        .unwrap();
    app.receive();
    assert!(matches!(app.state, State::Ready));
    assert_eq!(app.input.lines(), &["keep-draft"]);

    // Bare /test-web-search dispatches both Hacker News and GitHub probes.
    app.input = editor();
    app.input.insert_str("/test-web-search");
    app.submit(PendingKind::Steering);
    let WorkerCommand::TestWebSearch {
        queries, provider, ..
    } = requests.try_recv().unwrap()
    else {
        panic!("expected WorkerCommand::TestWebSearch");
    };
    assert_eq!(
        queries,
        vec![
            "site:news.ycombinator.com rust programming language",
            "site:github.com rust programming language",
        ]
    );
    assert_eq!(provider, None);
    events
        .send(WorkerEvent::WebSearchCompleted(Ok(())))
        .unwrap();
    app.receive();
    assert!(matches!(app.state, State::Ready));
    drop(events);

    // Cancelling during the first probe skips remaining probes.
    let cancel = pp_core::cancellation::CancellationToken::new();
    let mut seen = Vec::new();
    crate::tui::web_search_test::run_queries(
        &[
            "site:news.ycombinator.com rust programming language".into(),
            "site:github.com rust programming language".into(),
        ],
        "test-web-search-1",
        &cancel,
        |query, call_id| {
            seen.push((query.to_owned(), call_id.to_owned()));
            cancel.cancel();
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(
        seen,
        vec![(
            "site:news.ycombinator.com rust programming language".to_owned(),
            "test-web-search-1-1".to_owned(),
        )]
    );

    // Busy states reject /test-web-search and preserve draft.
    app.state = State::Running;
    app.input = editor();
    app.input.insert_str("/test-web-search rust");
    app.submit(PendingKind::Steering);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.input.lines(), &["/test-web-search rust"]);
}

#[test]
fn turn_completion_timing_and_cancel_failure_summary() {
    use chrono::TimeZone;

    for (seconds, duration) in [(0, "0s"), (5, "5s"), (65, "1m 5s"), (3725, "1h 2m 5s")] {
        let mut app = app();
        app.state = State::Running;
        app.show_welcome = false;
        app.conversation = app.conversation.fresh();
        app.input.insert_str("下一轮草稿");
        let cursor = app.input.cursor();
        let (events, receiver) = mpsc::channel();
        app.events = Some(receiver);
        events
            .send(WorkerEvent::Progress(AgentEvent::AssistantFinished {
                text: "finished answer".into(),
            }))
            .unwrap();
        events
            .send(WorkerEvent::Completed {
                result: Ok(RunOutcome::Completed),
                elapsed: Duration::from_secs(seconds),
                finished_at: chrono::Local
                    .with_ymd_and_hms(2026, 10, 19, 19, 6, 0)
                    .unwrap(),
            })
            .unwrap();
        app.receive();
        let expected = "✻ Completed · 19:06";
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains(expected), "{screen}");
        assert_eq!(app.conversation.task_duration().as_deref(), Some(duration));
        assert_eq!(screen.matches("Completed ·").count(), 1);
        assert!(screen.find("finished answer").unwrap() < screen.find(expected).unwrap());
        assert!(matches!(app.state, State::Ready));
        assert_eq!(app.input.lines(), &["下一轮草稿"]);
        assert_eq!(app.input.cursor(), cursor);
        app.receive();
        assert_eq!(draw(&mut app, 100, 30).matches("Completed ·").count(), 1);
        assert!(draw(&mut app, 35, 30).contains("Completed ·"));
    }

    for result in [
        Ok(RunOutcome::Cancelled),
        Err(anyhow::anyhow!("request failed")),
    ] {
        let mut app = app();
        app.state = State::Running;
        app.show_welcome = false;
        let (events, receiver) = mpsc::channel();
        app.events = Some(receiver);
        events
            .send(WorkerEvent::Completed {
                result,
                elapsed: Duration::from_secs(65),
                finished_at: chrono::Local::now(),
            })
            .unwrap();
        app.receive();
        assert!(!draw(&mut app, 100, 30).contains("Completed ·"));
        assert_eq!(app.conversation.task_duration().as_deref(), Some("1m 5s"));
    }
}

#[test]
fn task_cancel_waits_and_committed_token_isolation() {
    let mut app = app();
    app.state = State::Ready;
    app.show_welcome = false;
    let (commands, requests) = mpsc::channel();
    let (events, receiver) = mpsc::channel();
    app.commands = Some(commands);
    app.events = Some(receiver);
    let mut dirty = false;
    app.input.insert_str("first task");
    app.handle(key(KeyCode::Enter), &mut dirty);
    let WorkerCommand::Run { cancellation, .. } = requests.try_recv().unwrap() else {
        panic!()
    };
    app.input.insert_str("保留草稿");
    app.input.move_cursor(CursorMove::Back);
    let cursor = app.input.cursor();
    let history = app.input_history.clone();
    app.secondary_status = Some("secondary intact".into());
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(cancellation.is_cancelled());
    app.handle(key(KeyCode::Esc), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::Char('p'), FUNCTION_MODIFIER)),
        &mut dirty,
    );
    assert!(app.model_panel.is_some());
    app.handle(key(KeyCode::Esc), &mut dirty);
    app.handle(key(KeyCode::BackTab), &mut dirty);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.input.lines(), &["保留草稿"]);
    assert_eq!(app.input.cursor(), cursor);
    assert_eq!(app.input_history, history);
    assert!(matches!(app.state, State::Cancelling));
    let screen = draw(&mut app, 100, 30);
    assert!(screen.lines().any(|row| {
        row.strip_prefix("  ")
            .and_then(|row| row.strip_prefix(super::transcript_style::WAITING_CLOCK_FRAMES))
            .is_some_and(|row| row.trim() == "Cancelling")
    }));
    assert_eq!(screen.matches("Cancelling").count(), 1);
    assert!(screen.contains("secondary intact"));
    app.handle(Event::Paste("新".into()), &mut dirty);
    events
        .send(WorkerEvent::completed(
            Instant::now(),
            Ok(RunOutcome::Cancelled),
        ))
        .unwrap();
    app.receive();
    assert!(matches!(app.state, State::Ready));
    assert!(app.active_cancellation.is_none());
    assert_eq!(app.input.lines(), &["保留草新稿"]);

    let mut race = super::app();
    race.state = State::Ready;
    race.show_welcome = false;
    let (commands, requests) = mpsc::channel();
    let (events, receiver) = mpsc::channel();
    race.commands = Some(commands);
    race.events = Some(receiver);
    race.input.insert_str("first task");
    race.handle(key(KeyCode::Enter), &mut dirty);
    let WorkerCommand::Run {
        cancellation: old, ..
    } = requests.try_recv().unwrap()
    else {
        panic!()
    };
    events
        .send(WorkerEvent::completed(
            Instant::now(),
            Ok(RunOutcome::Completed),
        ))
        .unwrap();
    race.handle(key(KeyCode::Esc), &mut dirty);
    assert!(old.is_cancelled());
    race.receive();
    race.handle(key(KeyCode::Esc), &mut dirty);
    assert!(matches!(race.state, State::Ready));
    race.input.insert_str("next task");
    race.handle(key(KeyCode::Enter), &mut dirty);
    let WorkerCommand::Run {
        cancellation: next, ..
    } = requests.try_recv().unwrap()
    else {
        panic!()
    };
    old.cancel();
    assert!(!next.is_cancelled());
    race.handle(key(KeyCode::Esc), &mut dirty);
    assert!(next.is_cancelled() && matches!(race.state, State::Cancelling));
    assert!(race.handle(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &mut dirty
    ));
}

#[test]
fn notifications_task_startup_and_filtering() {
    let mut app = app();
    app.state = State::Running;
    app.show_welcome = false;
    app.input.insert_str("保留草稿🙂");
    let cursor = app.input.cursor();
    app.conversation.progress(AgentEvent::AssistantStarted);
    app.conversation.progress(AgentEvent::AssistantDelta {
        text: "partial answer".into(),
        refusal: false,
    });
    let (sender, receiver) = mpsc::channel();
    app.events = Some(receiver);
    sender
        .send(WorkerEvent::SessionStarted("fixture-session".into()))
        .unwrap();
    app.receive();
    assert!(!draw(&mut app, 100, 30).contains("Session: fixture-session"));
    sender
        .send(WorkerEvent::completed(
            Instant::now(),
            Err(anyhow::anyhow!("request failed")),
        ))
        .unwrap();
    app.receive();
    assert!(matches!(app.state, State::Ready));
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
    assert!(draw(&mut app, 100, 30).contains("request failed"));
    let snapshot = draw_buffer(&mut app, 100, 30);
    let mut scrollback = Vec::new();
    app.conversation
        .write_full_for_test(&mut scrollback, &snapshot)
        .unwrap();
    let history = String::from_utf8_lossy(&scrollback);
    assert!(history.contains("partial answer") && !history.contains("fixture-session"));
    assert_eq!(history.matches("request failed").count(), 1);
    let screen = draw(&mut app, 100, 30);
    assert!(
        screen.contains("partial answer")
            && screen.contains("request failed")
            && !screen.contains("Session:")
    );
    assert_eq!(app.input.lines(), &["保留草稿🙂"]);
    assert_eq!(app.input.cursor(), cursor);

    let mut unavail = App::new(Err(anyhow::anyhow!("fixture configuration unavailable"))).unwrap();
    assert_notification(&mut unavail, Color::Rgb(224, 108, 117));
    assert!(draw(&mut unavail, 100, 30).contains("fixture configuration unavailable"));
    assert!(!unavail.show_welcome);
    unavail.input.insert_str("草稿");
    unavail.fail_worker();
    assert_notification(&mut unavail, Color::Rgb(224, 108, 117));
    assert!(draw(&mut unavail, 100, 30).contains("agent worker channel closed"));
    assert!(matches!(unavail.state, State::Unavailable));
    let screen = draw(&mut unavail, 100, 30);
    assert!(
        !screen.contains("▄▄▄██▀▀██")
            && screen.contains("configuration unavailable")
            && screen.contains("channel closed")
    );
    assert_eq!(unavail.input.lines(), &["草稿"]);

    let mut filtered = super::app();
    let before = draw(&mut filtered, 100, 30);
    filtered.notify(Level::Normal, "routine success".into());
    filtered.notify(Level::Error, " \n\t".into());
    let (events, receiver) = mpsc::channel();
    filtered.events = Some(receiver);
    events
        .send(WorkerEvent::Progress(AgentEvent::RequestRetrying {
            reason: pp_core::events::RequestRetryReason::HttpStatus(503),
            attempt: 1,
            wait_ms: 30_000,
        }))
        .unwrap();
    filtered.receive();
    let retrying = draw(&mut filtered, 100, 30);
    assert_ne!(retrying, before);
    assert!(
        retrying.contains("Retrying")
            && retrying.contains("attempt 1 failed: HTTP status 503; retrying in"),
        "{retrying}"
    );
    events
        .send(WorkerEvent::Progress(AgentEvent::ModelRequestFinished {
            elapsed_ms: 1,
            usage: None,
            success: false,
        }))
        .unwrap();
    filtered.receive();
    assert_eq!(draw(&mut filtered, 100, 30), before);
    filtered.notify(Level::Error, "first failure".into());
    filtered.notify(Level::Error, "second failure".into());
    let screen = draw(&mut filtered, 100, 30);
    assert!(screen.contains("first failure") && screen.contains("second failure"));
}

#[test]
fn retry_notice() {
    let mut retry_app = app();
    let (events, receiver) = mpsc::channel();
    retry_app.events = Some(receiver);
    events
        .send(WorkerEvent::Progress(AgentEvent::RequestRetrying {
            reason: pp_core::events::RequestRetryReason::HttpStatus(400),
            attempt: 3,
            wait_ms: 60_000,
        }))
        .unwrap();
    retry_app.receive();
    let screen = draw(&mut retry_app, 100, 30);
    assert!(
        screen.contains("Retrying")
            && screen.contains("attempt 3 failed: HTTP status 400; retrying in 60s")
            && screen.contains("Esc cancels"),
        "{screen}"
    );
    let now = Instant::now();
    assert!(!retry_app.retry_tick(now + Duration::from_millis(100)));
    assert!(retry_app.retry_tick(now + Duration::from_secs(1)));
    assert!(draw(&mut retry_app, 100, 30).contains("retrying in 59s"));
    assert!(!retry_app.retry_tick(now + Duration::from_millis(1100)));
    assert!(retry_app.retry_visible());
    assert!(retry_app.retry_tick(Instant::now() + Duration::from_secs(61)));
    assert!(!retry_app.retry_visible());
    assert!(!draw(&mut retry_app, 100, 30).contains("Retrying"));
}

#[test]
fn initial_commands_file_shell_and_parse_errors() {
    let (mut app, requests) = file_commands_app(review_files());
    assert!(!app.submit_initial_command("/review src/main.rs"));
    let WorkerCommand::Run { prompt, .. } = requests.try_recv().unwrap() else {
        panic!("a startup file command should dispatch WorkerCommand::Run");
    };
    assert_eq!(prompt, "Review src/main.rs now.");
    assert_eq!(app.input.lines(), &[""]);
    assert_eq!(app.input_history, ["/review src/main.rs"]);
    assert!(matches!(app.state, State::Running));

    let (mut err_app, err_req) = file_commands_app(review_files());
    assert!(!err_app.submit_initial_command("/mcp nope"));
    assert!(err_req.try_recv().is_err());
    assert_eq!(err_app.input.lines(), &["/mcp nope"]);
    assert!(draw(&mut err_app, 100, 30).contains("usage: /mcp"));
    assert_notification(&mut err_app, Color::Rgb(224, 108, 117));

    let (mut sh_app, sh_req) = file_commands_app(review_files());
    assert!(!sh_app.submit_initial_command("!echo hi"));
    let WorkerCommand::Shell { command, .. } = sh_req.try_recv().unwrap() else {
        panic!("a startup shell command should dispatch WorkerCommand::Shell");
    };
    assert_eq!(command, "echo hi");
    assert_eq!(sh_app.input.lines(), &[""]);
}

#[test]
fn permission_switches_independently_across_states_queues_and_panels() {
    use pp_config::config::PermissionMode;

    let dir = tempfile::tempdir().unwrap();
    let gate = pp_permission::Gate::for_process(dir.path(), None, false);
    let (mut app, requests, _events) = plan_app();
    app.permission = gate.clone();
    app.permission_mode = Some(gate.snapshot());
    app.input.insert_str("保留草稿");

    // 1. While running, cancelling, reviewing a Plan or unavailable, F6 / Alt+Shift+A cycle the level and keep the draft.
    for (state, key_event, expected) in [
        (
            State::Running,
            key(KeyCode::F(6)),
            PermissionMode::AutoReview,
        ),
        (
            State::Cancelling,
            Event::Key(KeyEvent::new(
                KeyCode::Char('A'),
                KeyModifiers::ALT | KeyModifiers::SHIFT,
            )),
            PermissionMode::DangerFullAccess,
        ),
        (
            State::ReviewingPlan,
            key(KeyCode::F(6)),
            PermissionMode::ReadOnly,
        ),
        (
            State::Unavailable,
            key(KeyCode::F(6)),
            PermissionMode::WorkspaceWrite,
        ),
    ] {
        app.state = state;
        app.handle(key_event, &mut false);
        assert_eq!(gate.snapshot(), expected);
        assert_eq!(app.permission_mode, Some(expected));
        assert_eq!(app.input.lines(), &["保留草稿"]);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::SyncPermissions)
        ));
    }

    // 2. With a queued input and an ask dialog / panel open, the shortcut still works and does not close the dialog.
    app.state = State::Running;
    app.input_queue
        .enqueue(PendingKind::Steering, "queued".into(), None, true)
        .unwrap();
    assert!(app.has_pending());
    let (_answers, _token) = ask_fixture(&mut app);
    app.handle(key(KeyCode::F(6)), &mut false);
    assert_eq!(gate.snapshot(), PermissionMode::AutoReview);
    assert_eq!(app.permission_mode, Some(PermissionMode::AutoReview));
    assert!(app.ask_panel.is_some());
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::SyncPermissions)
    ));
    app.ask_panel = None;
    app.ask_reply = None;
    app.ask_id = None;
    app.ask_cancellation = None;

    // 3. Submitting `/permissions` during a Plan review or with a queued input also takes effect immediately.
    app.state = State::ReviewingPlan;
    app.clear_input();
    app.insert_input("/permissions yolo".into());
    app.submit(PendingKind::Steering);
    assert_eq!(gate.snapshot(), PermissionMode::DangerFullAccess);
    assert_eq!(app.permission_mode, Some(PermissionMode::DangerFullAccess));
    assert_eq!(app.input.lines(), &[""]);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::SyncPermissions)
    ));
}

#[test]
fn permission_dialog_shows_details_and_submits_global_scope_at_compact_height() {
    let (events, receiver) = mpsc::sync_channel(8);
    let token = CancellationToken::new();
    let handle = std::thread::spawn(move || {
        crate::tui::app::permissions::request_permission(
            &events,
            pp_permission::PermissionRequest {
                tool: "mcp__remote__echo".into(),
                agent: None,
                reason: "Runs outside the workspace sandbox.\nReview the requested permission."
                    .into(),
                session_grant: true,
                persistent_grant: true,
            },
            &token,
        )
    });

    let mut app = app();
    app.input.insert_str("draft");
    app.receive_event_for_test(receiver.recv_timeout(Duration::from_secs(2)).unwrap());
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("MCP remote Echo"), "{screen}");
    let title = screen
        .lines()
        .find(|row| row.contains("Permission required"))
        .unwrap();
    assert!(title.starts_with('╭') && title.ends_with('╮'), "{screen}");
    assert!(
        screen
            .lines()
            .zip(screen.lines().skip(1))
            .any(|(first, second)| {
                first.contains("workspace sandbox")
                    && second.contains("Review the requested permission")
            }),
        "{screen}"
    );
    let mut options = screen.lines().skip_while(|row| !row.contains("Allow once"));
    let mut description_columns = Vec::new();
    for (label, description) in [
        ("Allow once", "this request only"),
        ("Allow for this session", "until this session ends"),
        ("Always allow globally", "saved to permission.toml"),
        ("Deny", "reject this request"),
    ] {
        let row = options.next().unwrap_or_default();
        assert!(row.contains(label), "{screen}");
        let start = row.find(description).expect(&screen);
        description_columns.push(row[..start].chars().count());
    }
    // The description column of each option aligns to the longest label.
    assert!(
        description_columns
            .windows(2)
            .all(|pair| pair[0] == pair[1]),
        "{screen}"
    );
    let (separator, hint) = screen
        .lines()
        .zip(screen.lines().skip(1))
        .find(|(_, row)| row.contains("Enter confirm"))
        .unwrap();
    assert!(
        separator.contains("────") && hint.contains("Esc cancel"),
        "{screen}"
    );

    let mut dirty = false;
    let short = draw(&mut app, 48, 10);
    assert!(short.contains("MCP remote Echo"), "{short}");
    assert!(!short.contains("workspace sandbox"), "{short}");
    // The description column shows or hides as a whole card, so narrow cards have no ragged edges.
    assert!(!short.contains("this request only"), "{short}");
    app.handle(key(KeyCode::PageDown), &mut dirty);
    let scrolled = draw(&mut app, 48, 10);
    assert!(scrolled.contains("workspace sandbox"), "{scrolled}");
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    let compact = draw(&mut app, 28, 9);
    assert!(compact.contains("❯ Always allow globally"), "{compact}");
    assert_eq!(app.input.lines(), &["draft"]);
    app.handle(key(KeyCode::Enter), &mut dirty);
    let decision = handle.join().unwrap().unwrap();
    assert_eq!(
        decision,
        pp_permission::PermissionDecision::ApprovePersistently
    );
}

#[test]
fn unavailable_model_binds_permissions_and_modal_overlay_accepts_real_keys() {
    {
        use pp_config::config::PermissionMode;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let bootstrap = pp_runtime::agent::Bootstrap {
            dir: root.clone(),
            cwd: root.clone(),
            skills: pp_config::skills::Skills::default().shared(),
            prompt: pp_runtime::system_prompt::Snapshot::bundled(),
            permission: std::sync::Arc::new(pp_permission::Gate::for_process(&root, None, false)),
            sandbox_override: None,
            sandbox: pp_sandbox::SharedSandbox::default(),
            mcp: std::sync::Arc::new(pp_mcp::McpManager::empty()),
            models: pp_config::model_state::SharedModels::default(),
            allowed_tools: None,
            subagents_enabled: true,
            hooks_enabled: true,
            ephemeral: false,
            max_turns: None,
            output_schema: None,
        };
        let mut app = App::new(Err(anyhow::anyhow!("missing models.yml"))).unwrap();
        app.bootstrap = Some(bootstrap);
        app.bind_bootstrap_permissions();
        assert_eq!(app.permission_mode, Some(PermissionMode::WorkspaceWrite));
        assert_eq!(app.permission.snapshot(), PermissionMode::WorkspaceWrite);

        // The first F6 press should go from the default WorkspaceWrite to AutoReview.
        app.handle(key(KeyCode::F(6)), &mut false);
        assert_eq!(app.permission_mode, Some(PermissionMode::AutoReview));
        assert_eq!(app.permission.snapshot(), PermissionMode::AutoReview);
    }

    {
        use pp_config::config::PermissionMode;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let rules_path = root.join("permission.toml");
        std::fs::write(&rules_path, "[default.tools]\nwrite = \"allow\"\n").unwrap();
        std::fs::set_permissions(&rules_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let gate = pp_permission::Gate::for_process(&root, None, false);
        let (mut app, requests, _events) = plan_app();
        app.permission = gate.clone();
        app.permission_mode = Some(gate.snapshot());
        app.state = State::Running;

        let type_text = |app: &mut App, text: &str| {
            for ch in text.chars() {
                app.handle(key(KeyCode::Char(ch)), &mut false);
            }
        };

        // 1. With the permission dialog open, real keys F7 -> type `yolo` -> Enter switch the level directly without closing the dialog underneath.
        let (_answers, _token) = ask_fixture(&mut app);
        app.handle(key(KeyCode::F(7)), &mut false);
        assert!(app.permission_overlay.is_some());
        assert!(
            draw(&mut app, 100, 30).contains("Permission mode: Ask for approval (workspace-write)")
        );
        type_text(&mut app, "yolo");
        app.handle(key(KeyCode::Enter), &mut false);
        assert_eq!(gate.snapshot(), PermissionMode::DangerFullAccess);
        assert_eq!(app.permission_mode, Some(PermissionMode::DangerFullAccess));
        assert!(app.ask_panel.is_some());
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::SyncPermissions)
        ));
        app.handle(key(KeyCode::Esc), &mut false);
        assert!(app.permission_overlay.is_none());
        assert!(app.ask_panel.is_some());
        app.ask_panel = None;
        app.ask_reply = None;
        app.ask_id = None;
        app.ask_cancellation = None;

        // 2. With a full-screen modal panel (context_panel) open, F7 -> Enter still shows details and switches the level.
        app.context_panel = Some(crate::tui::context_panel::Panel::new());
        app.handle(key(KeyCode::F(7)), &mut false);
        app.handle(key(KeyCode::Enter), &mut false);
        assert!(
            draw(&mut app, 100, 30).contains("Permission mode: Full Access (danger-full-access)")
        );
        type_text(&mut app, "auto_review");
        app.handle(key(KeyCode::Enter), &mut false);
        assert_eq!(gate.snapshot(), PermissionMode::AutoReview);
        assert!(app.context_panel.is_some());

        // 3. While another handle holds an exclusive lock on permission.lock, `/permissions revoke` goes to a dedicated worker and does not block the event loop.
        use std::os::fd::AsRawFd;
        let lock_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("permission.lock"))
            .unwrap();
        std::fs::set_permissions(
            root.join("permission.lock"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        // SAFETY: `lock_file` stays open here and while the lock is held, so the fd is valid.
        assert_eq!(
            unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) },
            0
        );

        type_text(&mut app, "revoke write");
        app.handle(key(KeyCode::Enter), &mut false);
        assert!(draw(&mut app, 100, 30).contains("Revoking saved permission rule..."));
        // The event loop is not blocked while the lock is held: pressing F6 immediately still switches the in-memory permission level.
        app.handle(key(KeyCode::F(6)), &mut false);
        assert_eq!(gate.snapshot(), PermissionMode::DangerFullAccess);
        assert!(
            draw(&mut app, 100, 30).contains("Permission mode: Full Access (danger-full-access)")
        );

        // After the lock is released the background worker finishes the revoke and reports the result.
        drop(lock_file);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !draw(&mut app, 100, 30).contains("Saved permission revoked.") {
            assert!(Instant::now() < deadline, "revoke worker reply timed out");
            app.receive();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(gate.list_rules().unwrap(), "No saved permission rules.");

        // 4. Add a command-level prefix rule through the overlay `/permissions allow cargo test` and revoke it as `bash:cargo test`.
        type_text(&mut app, "allow cargo test");
        app.handle(key(KeyCode::Enter), &mut false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !draw(&mut app, 100, 30).contains("Saved bash permission rule: cargo test") {
            assert!(Instant::now() < deadline, "save bash rule timed out");
            app.receive();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            gate.list_rules()
                .unwrap()
                .contains("Rule (exact, allow): cargo test")
        );
        type_text(&mut app, "revoke bash:cargo test");
        app.handle(key(KeyCode::Enter), &mut false);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !draw(&mut app, 100, 30).contains("Saved permission revoked.") {
            assert!(Instant::now() < deadline, "revoke bash rule timed out");
            app.receive();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(gate.list_rules().unwrap(), "No saved permission rules.");
    }
}
