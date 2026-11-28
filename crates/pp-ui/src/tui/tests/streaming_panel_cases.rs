use super::*;

#[test]
fn command_menu_unknown_init_skill_and_ranking() {
    let mut app = app();
    app.state = State::Ready;
    app.show_welcome = true;
    app.input.insert_str("/bogus");
    let cursor = app.input.cursor();
    app.submit(PendingKind::Steering);
    let screen = draw(&mut app, 100, 30);
    assert!(!screen.contains("Error") && !screen.contains("unknown command"));
    assert!(app.show_welcome && app.input.lines() == ["/bogus"]);
    assert_eq!(app.input.cursor(), cursor);

    let mut registry = pp_config::skills::Skills::default();
    registry.diagnostics = vec![
        "skill root must be a directory: first".into(),
        "second diagnostic".into(),
    ];
    let shared = registry.shared();
    let history_dir = tempfile::tempdir().unwrap();
    let mut skill_app = App::new_with_skills(
        Err(anyhow::anyhow!("fixture unavailable")),
        shared.clone(),
        commands::FileCommands::default(),
        Path::new("."),
        Some(history_dir.path().join("history.db")),
    )
    .unwrap();
    assert_notification(&mut skill_app, Color::Rgb(224, 108, 117));
    let screen = draw(&mut skill_app, 100, 30).replace(' ', "");
    assert!(!screen.contains("Skillscanissues") && screen.contains("fixtureunavailable"));
    assert!(!skill_app.show_welcome && shared.lock().unwrap().diagnostics.len() == 2);
    skill_app.skill_panel = Some(skills::Panel::new(shared.lock().unwrap().clone()));
    skill_app.conversation = skill_app.conversation.fresh();
    skill_app.show_welcome = true;
    let screen = draw(&mut skill_app, 100, 30).replace(' ', "");
    assert!(
        screen.contains("skillrootmustbeadirectory:first")
            && screen.contains("seconddiagnostic")
            && !screen.contains("Skillscanissues")
    );
    skill_app.skill_panel = None;
    assert!(!draw(&mut skill_app, 100, 30).contains("Skill scan issues"));

    let (mut init_app, requests) = file_commands_app(commands::FileCommands::default());
    let mut dirty = false;
    init_app.handle(Event::Paste("/INt".into()), &mut dirty);
    assert!(draw(&mut init_app, 100, 30).contains("Create or update project AGENTS.md"));
    init_app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(init_app.input.lines(), &["/init"]);
    assert!(requests.try_recv().is_err());
    init_app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt, .. })
            if prompt == pp_tools::project_instructions::INIT_PROMPT)
    );
    let screen = draw(&mut init_app, 100, 30);
    assert!(screen.contains("/init") && !screen.contains("First inspect"));
    init_app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(init_app.input.lines(), &["/init"]);

    let mut files = review_files();
    files.entries.push(commands::FileCommand {
        name: "/re".into(),
        description: "Exact match".into(),
        content: "Review now.".into(),
    });
    let (mut rank_app, rank_req) = file_commands_app(files);
    rank_app.handle(Event::Paste("/RE".into()), &mut dirty);
    let labels: Vec<_> = rank_app
        .menu
        .as_ref()
        .unwrap()
        .items()
        .iter()
        .map(|item| item.label.as_str())
        .collect();
    assert_eq!(labels, ["/re", "/resume", "/review"]);
    rank_app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(matches!(
        rank_req.try_recv(),
        Ok(WorkerCommand::Run { prompt, .. }) if prompt == "Review now."
    ));
}

#[test]
fn file_and_effort_command_completion_and_diagnostics() {
    let (mut app, requests) = file_commands_app(review_files());
    let mut dirty = false;
    app.handle(Event::Paste("/RVW".into()), &mut dirty);
    assert_eq!(menu_values(&app), vec![MenuAction::FileCommand(0)]);
    assert!(draw(&mut app, 100, 30).contains("Review source"));
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &["/review"]);
    assert!(requests.try_recv().is_err());
    app.handle(Event::Paste(" auth".into()), &mut dirty);
    assert!(app.menu.is_none());
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::Run { prompt: text, .. }) if text == "Review auth now."
    ));
    assert!(app.input.is_empty());
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("Review auth now.") && !screen.contains("/review auth"));
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["/review auth"]);
    app.input = editor();
    app.refresh_candidates(true);
    app.handle(Event::Paste("/review later".into()), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(app.input.is_empty());
    assert_eq!(app.input_history.last().unwrap(), "/review later");
    let queued = app.input_queue.claim_next().unwrap().unwrap();
    assert_eq!(
        (queued.source.as_str(), queued.prompt()),
        ("/review later", "Review later now.")
    );
    assert!(!draw(&mut app, 100, 30).contains("Review later now."));
    for state in [State::Unavailable, State::Cancelling] {
        app.state = state;
        app.handle(Event::Paste("/review later".into()), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(requests.try_recv().is_err() && app.input.lines() == ["/review later"]);
        app.input = editor();
        app.refresh_candidates(true);
    }

    let (mut diag_app, _) = file_commands_app(review_files());
    diag_app.handle(Event::Paste("/rev".into()), &mut dirty);
    diag_app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(diag_app.menu.is_none() && diag_app.input.lines() == ["/rev"]);
    let mut files = review_files();
    files.diagnostics = vec![
        "commands/broken.md: Invalid command metadata".into(),
        "commands/dup.md: duplicate command name: /dup".into(),
    ];
    let shared = pp_config::skills::Skills::default().shared();
    let history_dir = tempfile::tempdir().unwrap();
    let mut help_app = App::new_with_skills(
        Err(anyhow::anyhow!("fixture unavailable")),
        shared,
        files,
        Path::new("."),
        Some(history_dir.path().join("history.db")),
    )
    .unwrap();
    assert_notification(&mut help_app, Color::Rgb(224, 108, 117));
    let screen = draw(&mut help_app, 100, 30).replace(' ', "");
    assert!(!screen.contains("Commandscanissues") && screen.contains("fixtureunavailable"));
    help_app.conversation = help_app.conversation.fresh();
    help_app.show_welcome = true;
    help_app.state = State::Ready;
    help_app.show_help();
    assert!(help_app.help_panel.is_some());
    assert!(help_app.needs_alternate());
    assert!(help_app.show_welcome);
    assert!(!help_app.conversation.has_entries());
    let text = draw(&mut help_app, 100, 200);
    assert!(
        text.contains("Review source")
            && text.contains("Command load issues:")
            && text.contains("commands/broken.md: Invalid command metadata")
            && text.contains("commands/dup.md: duplicate command name: /dup"),
        "{text}"
    );
    help_app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(help_app.help_panel.is_none());
    assert!(!help_app.needs_alternate());
    assert!(help_app.show_welcome);
    assert!(!help_app.conversation.has_entries());

    let mut eff = super::app();
    eff.state = State::Ready;
    eff.handle(Event::Paste("/effort ".into()), &mut dirty);
    assert!(eff.menu.is_some());
    eff.handle(key(KeyCode::Esc), &mut dirty);
    assert!(eff.menu.is_none());
    eff.refresh_candidates(false);
    assert!(eff.menu.is_none() && eff.input.lines() == ["/effort "]);
    eff.handle(key(KeyCode::Char('x')), &mut dirty);
    assert_eq!(
        menu_values(&eff),
        vec![MenuAction::Effort(ThinkLevel::Xhigh)]
    );
    eff.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(eff.input.lines(), &["/effort xhigh"]);
    assert_eq!(eff.think_level, ThinkLevel::None);
    eff.handle(key(KeyCode::Backspace), &mut dirty);
    eff.handle(Event::Paste("\nother".into()), &mut dirty);
    assert!(eff.menu.is_none());
}

#[test]
fn status_duration_and_token_speed_updates() {
    let mut app = app();
    app.usage = Some(pp_core::context::ContextUsage {
        estimated_tokens: 100,
        context_window: Some(40000),
        auto_compact: false,
        threshold_percent: 80,
        last_request: None,
        session_output_tokens: 0,
    });
    let render_status = |app: &App, width| {
        let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
        terminal
            .draw(|frame| app.draw_input_status(frame, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    assert!(render_status(&app, 100).trim().is_empty());
    let usage = app.usage.take();
    app.subagent_status = Some((
        1,
        pp_runtime::subagent::SubagentCounts {
            running: 0,
            sessions: 21,
        },
    ));
    assert_eq!(render_status(&app, 6).trim(), "\u{f0c0} 21");
    assert!(render_status(&app, 5).trim().is_empty());
    app.subagent_status = None;
    app.usage = usage;

    app.conversation.begin_run();
    app.conversation
        .tick(Instant::now() + Duration::from_secs(85));
    assert!(
        render_status(&app, 100)
            .trim_end()
            .ends_with("\u{f051b} 1m25s")
    );
    assert!(render_status(&app, 100).contains("\u{e62a} 0.2%/40K"));
    let narrow = render_status(&app, 20);
    assert!(
        narrow.trim_end().ends_with("\u{e62a} 0.2%/40K")
            && !narrow.contains("1m")
            && !narrow.contains("25s"),
        "{narrow}"
    );
    app.conversation
        .finish_run("Completed", Duration::from_secs(85), chrono::Local::now());
    assert!(
        !app.conversation
            .tick(Instant::now() + Duration::from_secs(3600))
    );
    assert!(
        render_status(&app, 100)
            .trim_end()
            .ends_with("\u{f051b} 1m25s")
    );
    app.conversation.begin_run();
    assert!(
        render_status(&app, 100)
            .trim_end()
            .ends_with("\u{f051b} 0s")
    );
    app.conversation = app.conversation.fresh();
    assert!(render_status(&app, 100).trim().is_empty());

    let mut speed = super::app();
    speed.usage = Some(pp_core::context::ContextUsage {
        estimated_tokens: 100,
        context_window: Some(40000),
        auto_compact: false,
        threshold_percent: 80,
        last_request: None,
        session_output_tokens: 0,
    });
    let (events, receiver) = mpsc::channel();
    speed.events = Some(receiver);
    let usage = pp_core::context::TokenUsage {
        input_tokens: 12000,
        output_tokens: 300,
        cache_write_tokens: None,
        cached_tokens: Some(40),
        reasoning_tokens: None,
    };
    for event in [
        AgentEvent::ModelRequestStarted,
        AgentEvent::ModelOutputProgress { output_bytes: 600 },
        AgentEvent::ModelRequestFinished {
            elapsed_ms: 2000,
            usage: Some(usage),
            success: true,
        },
    ] {
        events.send(WorkerEvent::Progress(event)).unwrap();
    }
    speed.receive();
    assert_eq!(speed.usage.unwrap().last_request, Some(usage));
    assert_eq!(speed.token_speed.value(), Some((150.0, false)));
    speed.fail_worker();
    assert_eq!(speed.token_speed.value(), Some((150.0, false)));
    speed.events = Some(mpsc::channel().1);
    speed.token_speed.start(Instant::now());
    speed.token_speed.progress(600, Instant::now());
    speed.fail_worker();
    assert!(!speed.token_speed.is_active() && speed.token_speed.value().is_none());

    let mut missing = super::app();
    missing.usage = Some(pp_core::context::ContextUsage {
        estimated_tokens: 100,
        context_window: Some(40000),
        last_request: Some(usage),
        session_output_tokens: 300,
        auto_compact: false,
        threshold_percent: 80,
    });
    let (events, receiver) = mpsc::channel();
    missing.events = Some(receiver);
    for success in [false, true] {
        for event in [
            WorkerEvent::Progress(AgentEvent::ModelRequestStarted),
            WorkerEvent::Progress(AgentEvent::ModelOutputProgress { output_bytes: 600 }),
            WorkerEvent::Progress(AgentEvent::ModelRequestFinished {
                elapsed_ms: 2000,
                usage: None,
                success,
            }),
            WorkerEvent::Completed {
                result: Ok(RunOutcome::Completed),
                elapsed: Duration::from_secs(2),
                finished_at: chrono::Local::now(),
            },
        ] {
            events.send(event).unwrap();
        }
        missing.receive();
        assert_eq!(
            missing.usage.unwrap().last_request,
            if success { None } else { Some(usage) }
        );
        assert_eq!(
            missing.token_speed.value(),
            if success { Some((100.0, true)) } else { None }
        );
    }
}

#[test]
fn input_uses_horizontal_rules_and_reserves_secondary_status_on_the_right() {
    let mut app = app();
    app.state = State::Ready;
    app.input.insert_str("draft-中\nsecond line");
    let screen = draw(&mut app, 100, 30);
    let lines: Vec<_> = screen.lines().collect();
    assert!(lines[26].starts_with("❯ draft-中"));
    assert!(lines[27].starts_with("  second line"));
    assert_eq!(lines[28], "─".repeat(100));
    assert!(lines[29].starts_with(" \u{f1d9} · \u{ec19} — · \u{f0766} off · \u{f07c} fixture-cwd"));

    app.secondary_status = Some("extra info".into());
    let screen = draw(&mut app, 100, 30);
    let top = screen.lines().nth(25).unwrap();
    assert!(top.starts_with(&"─".repeat(50)));
    assert!(top.ends_with(" extra info "));
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, &StatusContext::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    for x in 0..88 {
        assert_eq!(buffer[(x, 25)].symbol(), "─");
        assert_eq!(buffer[(x, 25)].fg, statusline::accent());
    }
    assert_eq!(app.input.lines(), &["draft-中", "second line"]);
    for (width, height) in [(1, 1), (2, 2), (20, 4), (100, 30)] {
        draw(&mut app, width, height);
        assert_eq!(app.input.lines(), &["draft-中", "second line"]);
    }
}

#[test]
fn mouse_scroll_routes_without_editing_draft() {
    // Report over the input area: wheel navigation is deliberately global.
    let mouse = |kind| {
        Event::Mouse(MouseEvent {
            kind,
            column: 10,
            row: 26,
            modifiers: KeyModifiers::NONE,
        })
    };
    for state in [State::Ready, State::Running, State::Unavailable] {
        let mut app = app();
        app.state = state;
        let (sender, requests) = mpsc::channel();
        app.commands = Some(sender);
        for index in 0..60 {
            app.append("Assistant", &format!("history-{index:02}"));
        }
        app.input.insert_str("draft-中");
        let cursor = app.input.cursor();
        let bottom = draw(&mut app, 100, 30);
        assert!(bottom.contains("history-59"));
        let mut dirty = false;
        assert!(!app.handle(mouse(MouseEventKind::ScrollUp), &mut dirty));
        assert!(dirty);
        let older = draw(&mut app, 100, 30);
        assert!(older.contains("history-57"));
        assert!(!older.contains("history-59"));
        assert_eq!(app.input.lines(), &["draft-中"]);
        assert_eq!(app.input.cursor(), cursor);
        dirty = false;
        assert!(!app.handle(mouse(MouseEventKind::ScrollDown), &mut dirty));
        assert!(dirty);
        assert_eq!(draw(&mut app, 100, 30), bottom);

        for kind in [
            MouseEventKind::Moved,
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            dirty = false;
            assert!(!app.handle(mouse(kind), &mut dirty));
            assert!(!dirty);
            assert_eq!(draw(&mut app, 100, 30), bottom);
            assert_eq!(app.input.cursor(), cursor);
            assert_eq!(app.input.lines(), &["draft-中"]);
        }

        app.menu = Some(SelectionMenu::new(
            vec![MenuItem {
                value: MenuAction::Model(0),
                label: "fixture".into(),
                description: "fixture model".into(),
                icon: None,
                current: true,
            }],
            0,
        ));
        for kind in [MouseEventKind::ScrollUp, MouseEventKind::ScrollDown] {
            dirty = false;
            assert!(!app.handle(mouse(kind), &mut dirty));
            assert!(dirty);
            assert!(app.menu.is_some());
        }
        app.menu = None;
        assert_eq!(draw(&mut app, 100, 30), bottom);
        assert_eq!(app.input.lines(), &["draft-中"]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));

        if !matches!(state, State::Running) {
            app.clear_input();
            app.input.insert_str("/");
            app.refresh_candidates(true);
            app.handle(key(KeyCode::Down), &mut dirty);
            let before = draw(&mut app, 100, 30);
            assert!(before.contains("Show available commands"));
            dirty = false;
            assert!(!app.handle(mouse(MouseEventKind::ScrollUp), &mut dirty));
            assert!(dirty);
            assert_ne!(draw(&mut app, 100, 30), before);
            assert_eq!(app.menu.as_ref().unwrap().selected_index(), Some(1));
            assert_eq!(app.input.lines(), &["/"]);
            assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        }
    }
}

#[test]
fn streaming_receive_resize_batches_and_failure() {
    let mut app = app();
    let (sender, receiver) = mpsc::sync_channel(512);
    app.events = Some(receiver);
    app.state = State::Running;
    app.input.insert_str("draft-中");
    assert!(
        sender
            .send(WorkerEvent::Progress(AgentEvent::AssistantStarted))
            .is_ok()
    );
    for _ in 0..255 {
        assert!(
            sender
                .send(WorkerEvent::Progress(AgentEvent::AssistantDelta {
                    text: "x".into(),
                    refusal: false,
                }))
                .is_ok()
        );
    }
    assert!(
        sender
            .send(WorkerEvent::completed(
                Instant::now(),
                Ok(RunOutcome::Completed)
            ))
            .is_ok()
    );
    assert!(app.receive());
    assert!(matches!(app.state, State::Running) && app.input.lines() == ["draft-中"]);
    let mut batches = 0;
    while matches!(app.state, State::Running) && batches < 257 {
        assert!(app.receive());
        batches += 1;
    }
    assert!(batches >= 4 && matches!(app.state, State::Ready));
    assert_eq!(app.input.lines(), &["draft-中"]);
    assert!(draw(&mut app, 100, 30).contains("draft-中"));

    let mut resize = super::app();
    resize.state = State::Running;
    for index in 0..60 {
        resize.append("Assistant", &format!("history-{index}"));
    }
    resize.input.insert_str("draft-中");
    resize.conversation.progress(AgentEvent::AssistantStarted);
    resize.conversation.progress(AgentEvent::AssistantDelta {
        text: "live-first".into(),
        refusal: false,
    });
    draw(&mut resize, 100, 30);
    let mut dirty = false;
    assert!(!resize.handle(key(KeyCode::PageUp), &mut dirty));
    let before = draw(&mut resize, 100, 30);
    resize.conversation.progress(AgentEvent::AssistantDelta {
        text: "-last".into(),
        refusal: false,
    });
    assert_eq!(before, draw(&mut resize, 100, 30));
    assert_eq!(resize.input.lines(), &["draft-中"]);
    assert!(!resize.handle(Event::Resize(20, 4), &mut dirty));
    let small = draw(&mut resize, 20, 4);
    assert!(small.contains("❯") && resize.input.lines() == ["draft-中"]);
    assert!(!resize.handle(Event::Resize(100, 30), &mut dirty));
    assert!(draw(&mut resize, 100, 30).contains("draft-中"));
    for _ in 0..10 {
        resize.handle(key(KeyCode::PageDown), &mut dirty);
    }
    assert!(draw(&mut resize, 100, 30).contains("live-first-last"));
    let (sender, receiver) = mpsc::sync_channel(256);
    resize.events = Some(receiver);
    sender
        .send(WorkerEvent::completed(
            Instant::now(),
            Err(anyhow::anyhow!("stream failed")),
        ))
        .unwrap();
    resize.receive();
    assert!(matches!(resize.state, State::Ready) && resize.input.lines() == ["draft-中"]);
    let screen = draw(&mut resize, 100, 30);
    assert!(screen.contains("live-first-last") && screen.contains("stream failed"));
    assert!(resize.handle(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &mut dirty,
    ));

    for outcome in [RunOutcome::Completed, RunOutcome::Cancelled] {
        let mut batch = super::app();
        batch.state = State::Running;
        batch.show_welcome = false;
        let (sender, receiver) = mpsc::sync_channel(512);
        batch.events = Some(receiver);
        sender
            .send(WorkerEvent::Progress(AgentEvent::AssistantStarted))
            .unwrap();
        for _ in 0..10 {
            sender
                .send(WorkerEvent::Progress(AgentEvent::ThinkingDelta {
                    text: "中🙂".into(),
                }))
                .unwrap();
        }
        batch.receive();
        assert!(batch.stream.take().is_some());
        for _ in 0..10 {
            sender
                .send(WorkerEvent::Progress(AgentEvent::AssistantDelta {
                    text: "part".into(),
                    refusal: false,
                }))
                .unwrap();
        }
        sender
            .send(WorkerEvent::completed(Instant::now(), Ok(outcome)))
            .unwrap();
        batch.receive();
        assert!(batch.stream.take().is_none());
        let screen = draw(&mut batch, 100, 30);
        assert!(
            screen.contains("partpartpartpartpartpartpartpartpartpart"),
            "{screen}"
        );
    }
}

#[test]
fn skills_panel_cancel_busy_and_save_failure_notice() {
    let mut app = app();
    let mut snapshot = pp_config::skills::Skills::default();
    snapshot.entries.push(pp_config::skills::Skill {
        name: "rust".into(),
        description: "Rust guidance".into(),
        path: "/skills/rust/SKILL.md".into(),
        enabled: true,
    });
    app.skill_panel = Some(skills::Panel::new(snapshot));
    app.input.insert_str("保留草稿");
    let mut dirty = false;
    app.handle(key(KeyCode::Tab), &mut dirty);
    app.handle(key(KeyCode::Char(' ')), &mut dirty);
    assert!(!app.skill_panel.as_ref().unwrap().entries[0].enabled);
    for (width, height) in [(100, 30), (20, 4), (1, 1), (100, 30)] {
        draw(&mut app, width, height);
    }
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.skill_panel.is_none() && app.input.lines() == ["保留草稿"]);
    app.input = editor();
    app.input.insert_str("/skills");
    app.state = State::Running;
    assert!(!app.submit(PendingKind::Steering));
    assert!(!app.skills_pending && app.input.lines() == ["/skills"]);

    let mut failed = super::app();
    failed.state = State::Ready;
    failed.input.insert_str("main draft");
    failed.skill_panel = Some(skills::Panel::new(pp_config::skills::Skills::default()));
    let (requests, _receiver) = mpsc::channel();
    let (sender, replies) = mpsc::channel();
    failed.skill_worker = skills::Worker { requests, replies };
    failed.handle(key(KeyCode::Char(' ')), &mut dirty);
    let before = draw(&mut failed, 100, 30);
    let error = format!("save failed\n{}", "complete details\n".repeat(15));
    sender
        .send(skills::Reply {
            result: Err(anyhow::anyhow!(error)),
            saved: true,
            preview: false,
            open_sources: false,
        })
        .unwrap();
    failed.receive();
    let shown = draw(&mut failed, 100, 30);
    assert!(shown.contains("save failed") && shown.contains("Skills"));
    assert_eq!(
        shown.contains("\u{f0765} claude"),
        before.contains("\u{f0765} claude")
    );
    assert!(failed.skill_panel.is_some());
    failed.handle(key(KeyCode::Esc), &mut dirty);
    assert_eq!(failed.input.lines(), &["main draft"]);
    let mut history = Vec::new();
    let buffer = draw_buffer(&mut failed, 100, 30);
    failed
        .conversation
        .write_full_for_test(&mut history, &buffer)
        .unwrap();
    let text = String::from_utf8_lossy(&history);
    assert_eq!(text.matches("save failed").count(), 1);
    assert_eq!(text.matches("complete details").count(), 15);
}
