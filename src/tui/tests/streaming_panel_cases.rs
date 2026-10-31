use super::*;

#[test]
fn notification_command_error_stays_in_history_and_preserves_editor() {
    let mut app = app();
    app.state = State::Ready;
    app.show_welcome = true;
    app.input.insert_str("/bogus");
    let cursor = app.input.cursor();
    app.submit(PendingKind::Steering);
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
    assert_eq!(app.input.lines(), &["/bogus"]);
    assert_eq!(app.input.cursor(), cursor);
    assert!(!app.show_welcome);
    assert!(draw(&mut app, 100, 30).contains("Error"));
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("unknown command"));
    assert!(screen.contains("╭"));
}

#[test]
fn startup_skill_summary_keeps_complete_diagnostics_in_panel() {
    let mut registry = crate::skills::Skills::default();
    registry.diagnostics = vec![
        "skill 根路径必须是目录：first".into(),
        "second diagnostic".into(),
    ];
    let shared = registry.shared();
    let history_dir = tempfile::tempdir().unwrap();
    let mut app = App::new_with_skills(
        Err(anyhow::anyhow!("fixture unavailable")),
        shared.clone(),
        commands::FileCommands::default(),
        Path::new("."),
        Some(history_dir.path().join("history.db")),
    )
    .unwrap();
    // 非致命 skill 诊断仅保留在管理面板，不合并进启动错误。
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
    let screen = draw(&mut app, 100, 30).replace(' ', "");
    assert!(!screen.contains("Skillscanissues"));
    assert!(screen.contains("fixtureunavailable"));
    assert!(!app.show_welcome);
    assert_eq!(shared.lock().unwrap().diagnostics.len(), 2);
    app.skill_panel = Some(skills::Panel::new(shared.lock().unwrap().clone()));
    app.conversation = app.conversation.fresh();
    app.show_welcome = true;
    let screen = draw(&mut app, 100, 30).replace(' ', "");
    assert!(screen.contains("skill根路径必须是目录：first"));
    assert!(screen.contains("seconddiagnostic"));
    assert!(!screen.contains("Skillscanissues"));
    app.skill_panel = None;
    assert!(!draw(&mut app, 100, 30).contains("Skill scan issues"));
}

#[test]
fn init_menu_submits_model_task_and_displays_command() {
    let (mut app, requests) = file_commands_app(commands::FileCommands::default());
    let mut dirty = false;
    app.handle(Event::Paste("/INt".into()), &mut dirty);
    assert!(draw(&mut app, 100, 30).contains("Create or update project AGENTS.md"));
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &["/init"]);
    assert!(requests.try_recv().is_err());
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt, .. })
            if prompt == crate::project_instructions::INIT_PROMPT)
    );
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("/init"));
    assert!(!screen.contains("First inspect"));
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["/init"]);
}

#[test]
fn file_command_menu_ranks_matches_together_with_builtin_commands() {
    let mut files = review_files();
    files.entries.push(commands::FileCommand {
        name: "/re".into(),
        description: "Exact match".into(),
        content: "Review now.".into(),
    });
    let (mut app, requests) = file_commands_app(files);
    let mut dirty = false;
    app.handle(Event::Paste("/RE".into()), &mut dirty);
    let labels: Vec<_> = app
        .menu
        .as_ref()
        .unwrap()
        .items()
        .iter()
        .map(|item| item.label.as_str())
        .collect();
    assert_eq!(labels, ["/re", "/resume", "/review", "/tree", "/Roles"]);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::Run { prompt, .. }) if prompt == "Review now."
    ));
}

#[test]
fn file_command_menu_completes_name_and_submits_expanded_prompt() {
    let (mut app, requests) = file_commands_app(review_files());
    let mut dirty = false;
    // 自定义命令支持忽略大小写的模糊匹配，并显示加载的 description。
    app.handle(Event::Paste("/RVW".into()), &mut dirty);
    assert_eq!(menu_values(&app), vec![MenuAction::FileCommand(0)]);
    assert!(draw(&mut app, 100, 30).contains("Review source"));
    // Tab 只补全名字，不展开也不发送。
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &["/review"]);
    assert!(requests.try_recv().is_err());
    // 参数不打开第二种菜单。
    app.handle(Event::Paste(" auth".into()), &mut dirty);
    assert!(app.menu.is_none());
    // Enter 发送展开后的正文并回显；历史保存原始 slash 输入。
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::Run { prompt: text, .. }) if text == "Review auth now."
    ));
    assert!(app.input.is_empty());
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("Review auth now."));
    assert!(!screen.contains("/review auth"));
    app.handle(key(KeyCode::Up), &mut dirty);
    assert_eq!(app.input.lines(), &["/review auth"]);
    app.input = editor();
    app.refresh_candidates(true);
    // 忙碌时展开一次入队，原始历史不丢失；未应用前不回显。
    app.handle(Event::Paste("/review later".into()), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(app.input.is_empty());
    assert_eq!(app.input_history.last().unwrap(), "/review later");
    let queued = app.input_queue.claim_next().unwrap().unwrap();
    assert_eq!(queued.source, "/review later");
    assert_eq!(queued.prompt(), "Review later now.");
    assert!(!draw(&mut app, 100, 30).contains("Review later now."));
    // 配置不可用和取消收尾仍保护草稿。
    for state in [State::Unavailable, State::Cancelling] {
        app.state = state;
        app.handle(Event::Paste("/review later".into()), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(requests.try_recv().is_err());
        assert_eq!(app.input.lines(), &["/review later"]);
        app.input = editor();
        app.refresh_candidates(true);
    }
}

#[test]
fn file_command_esc_keeps_draft_and_startup_notice_keeps_error_level() {
    let (mut app, _requests) = file_commands_app(review_files());
    let mut dirty = false;
    app.handle(Event::Paste("/rev".into()), &mut dirty);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.menu.is_none());
    assert_eq!(app.input.lines(), &["/rev"]);

    // 非致命命令诊断不再形成启动通知，完整详情仍在 /help。
    let mut files = review_files();
    files.diagnostics = vec![
        "commands/broken.md: Invalid command metadata".into(),
        "commands/dup.md: duplicate command name: /dup".into(),
    ];
    let shared = crate::skills::Skills::default().shared();
    let history_dir = tempfile::tempdir().unwrap();
    let mut app = App::new_with_skills(
        Err(anyhow::anyhow!("fixture unavailable")),
        shared,
        files,
        Path::new("."),
        Some(history_dir.path().join("history.db")),
    )
    .unwrap();
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
    let screen = draw(&mut app, 100, 30).replace(' ', "");
    assert!(!screen.contains("Commandscanissues"));
    assert!(screen.contains("fixtureunavailable"));
    // /help 附加用户命令说明与完整命令诊断。
    app.conversation = app.conversation.fresh();
    app.show_welcome = true;
    app.state = State::Ready;
    app.show_help();
    let buffer = draw_buffer(&mut app, 100, 30);
    let mut scrollback = Vec::new();
    app.conversation
        .write_full_for_test(&mut scrollback, &buffer)
        .unwrap();
    let text = String::from_utf8_lossy(&scrollback);
    assert!(text.contains("Review source"));
    assert!(text.contains("Command load issues:"));
    assert!(text.contains("commands/broken.md: Invalid command metadata"));
    assert!(text.contains("commands/dup.md: duplicate command name: /dup"));
}

#[test]
fn effort_menu_filters_completes_and_cancels_without_changing_level() {
    let mut app = app();
    app.state = State::Ready;
    let mut dirty = false;
    app.handle(Event::Paste("/effort ".into()), &mut dirty);
    assert!(app.menu.is_some());
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.menu.is_none());
    app.refresh_candidates(false);
    assert!(app.menu.is_none());
    assert_eq!(app.input.lines(), &["/effort "]);
    app.handle(key(KeyCode::Char('x')), &mut dirty);
    assert_eq!(
        menu_values(&app),
        vec![MenuAction::Effort(ThinkLevel::Xhigh)]
    );
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &["/effort xhigh"]);
    assert_eq!(app.think_level, ThinkLevel::None);
    app.handle(key(KeyCode::Backspace), &mut dirty);
    app.handle(Event::Paste("\nother".into()), &mut dirty);
    assert!(app.menu.is_none());
    app.input = editor();
    app.input.insert_str("/effort ");
    for state in [State::Running, State::SettingThinkLevel, State::Unavailable] {
        app.state = state;
        app.refresh_candidates(true);
        assert!(app.menu.is_none());
    }
}

#[test]
fn token_speed_completion_and_worker_fallback_preserve_success() {
    let mut app = app();
    app.usage = Some(crate::context::ContextUsage {
        estimated_tokens: 100,
        context_window: Some(40000),
        auto_compact: false,
        threshold_percent: 80,
        last_request: None,
    });
    let (events, receiver) = mpsc::channel();
    app.events = Some(receiver);
    let usage = crate::context::TokenUsage {
        input_tokens: 12000,
        output_tokens: 300,
        cache_write_tokens: None,
        cached_tokens: Some(40),
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
    app.receive();
    assert_eq!(app.usage.unwrap().last_request, Some(usage));
    assert_eq!(app.token_speed.value(), Some((150.0, false)));
    app.fail_worker();
    assert_eq!(app.token_speed.value(), Some((150.0, false)));
    app.events = Some(mpsc::channel().1);
    app.token_speed.start(Instant::now());
    app.token_speed.progress(600, Instant::now());
    app.fail_worker();
    assert!(!app.token_speed.is_active());
    assert_eq!(app.token_speed.value(), None);
}

#[test]
fn token_speed_missing_usage_replaces_counts_but_failure_preserves_them() {
    let mut app = app();
    let usage = crate::context::TokenUsage {
        input_tokens: 12000,
        output_tokens: 300,
        cache_write_tokens: None,
        cached_tokens: Some(40),
    };
    app.usage = Some(crate::context::ContextUsage {
        estimated_tokens: 100,
        context_window: Some(40000),
        last_request: Some(usage),
        auto_compact: false,
        threshold_percent: 80,
    });
    let (events, receiver) = mpsc::channel();
    app.events = Some(receiver);
    for success in [false, true] {
        events
            .send(WorkerEvent::Progress(AgentEvent::ModelRequestStarted))
            .unwrap();
        events
            .send(WorkerEvent::Progress(AgentEvent::ModelOutputProgress {
                output_bytes: 600,
            }))
            .unwrap();
        events
            .send(WorkerEvent::Progress(AgentEvent::ModelRequestFinished {
                elapsed_ms: 2000,
                usage: None,
                success,
            }))
            .unwrap();
        events
            .send(WorkerEvent::Completed {
                result: Ok(RunOutcome::Completed),
                elapsed: Duration::from_secs(2),
                finished_at: chrono::Local::now(),
            })
            .unwrap();
        app.receive();
        assert_eq!(
            app.usage.unwrap().last_request,
            if success { None } else { Some(usage) }
        );
        assert_eq!(
            app.token_speed.value(),
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
    assert!(lines[29].starts_with(" \u{f1d8} · \u{ec19} — · \u{f0766} off · \u{f07c} fixture-cwd"));

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
        assert_eq!(buffer[(x, 25)].fg, statusline::ACCENT);
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
fn streaming_receive_is_bounded_and_preserves_busy_draft() {
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
    assert!(matches!(app.state, State::Running));
    assert_eq!(app.input.lines(), &["draft-中"]);
    let mut batches = 0;
    while matches!(app.state, State::Running) && batches < 257 {
        assert!(app.receive());
        batches += 1;
    }
    assert!(batches >= 4);
    assert!(matches!(app.state, State::Ready));
    assert_eq!(app.input.lines(), &["draft-中"]);
    assert!(draw(&mut app, 100, 30).contains("draft-中"));
}

#[test]
fn streaming_resize_paging_and_failure_keep_editor_and_ctrl_c_immediate() {
    let mut app = app();
    app.state = State::Running;
    for index in 0..60 {
        app.append("Assistant", &format!("history-{index}"));
    }
    app.input.insert_str("draft-中");
    app.conversation.progress(AgentEvent::AssistantStarted);
    app.conversation.progress(AgentEvent::AssistantDelta {
        text: "live-first".into(),
        refusal: false,
    });
    draw(&mut app, 100, 30);
    let mut dirty = false;
    assert!(!app.handle(key(KeyCode::PageUp), &mut dirty));
    let before = draw(&mut app, 100, 30);
    app.conversation.progress(AgentEvent::AssistantDelta {
        text: "-last".into(),
        refusal: false,
    });
    assert_eq!(before, draw(&mut app, 100, 30));
    assert_eq!(app.input.lines(), &["draft-中"]);
    assert!(!app.handle(Event::Resize(20, 4), &mut dirty));
    let small = draw(&mut app, 20, 4);
    assert!(small.contains("❯"));
    assert_eq!(app.input.lines(), &["draft-中"]);
    assert!(!app.handle(Event::Resize(100, 30), &mut dirty));
    assert!(draw(&mut app, 100, 30).contains("draft-中"));
    for _ in 0..10 {
        app.handle(key(KeyCode::PageDown), &mut dirty);
    }
    assert!(draw(&mut app, 100, 30).contains("live-first-last"));
    let (sender, receiver) = mpsc::sync_channel(256);
    app.events = Some(receiver);
    assert!(
        sender
            .send(WorkerEvent::completed(
                Instant::now(),
                Err(anyhow::anyhow!("stream failed"))
            ))
            .is_ok()
    );
    app.receive();
    assert!(matches!(app.state, State::Ready));
    assert_eq!(app.input.lines(), &["draft-中"]);
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("live-first-last"));
    assert!(screen.contains("stream failed"));
    assert!(app.handle(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &mut dirty,
    ));
}

#[test]
fn skills_panel_cancels_edits_preserves_draft_and_rejects_busy_management() {
    let mut app = app();
    let mut snapshot = crate::skills::Skills::default();
    snapshot.entries.push(crate::skills::Skill {
        name: "rust".into(),
        description: "Rust 指导".into(),
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
    assert!(app.skill_panel.is_none());
    assert_eq!(app.input.lines(), &["保留草稿"]);
    app.input = editor();
    app.input.insert_str("/skills");
    app.state = State::Running;
    assert!(!app.submit(PendingKind::Steering));
    assert!(!app.skills_pending);
    assert_eq!(app.input.lines(), &["/skills"]);
}

#[test]
fn streaming_frame_batches_flush_before_completion_and_cancel() {
    for outcome in [RunOutcome::Completed, RunOutcome::Cancelled] {
        let mut app = app();
        app.state = State::Running;
        app.show_welcome = false;
        let (sender, receiver) = mpsc::sync_channel(512);
        app.events = Some(receiver);
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
        app.receive();
        assert!(app.stream.take().is_some());
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
        app.receive();
        assert!(app.stream.take().is_none());
        let screen = draw(&mut app, 100, 30);
        assert!(
            screen.contains("partpartpartpartpartpartpartpartpartpart"),
            "{screen}"
        );
    }
}

#[test]
fn notification_skill_save_failure_keeps_panel_draft_and_full_message() {
    let mut app = app();
    app.state = State::Ready;
    app.input.insert_str("main draft");
    app.skill_panel = Some(skills::Panel::new(crate::skills::Skills::default()));
    let (requests, _receiver) = mpsc::channel();
    let (sender, replies) = mpsc::channel();
    app.skill_worker = skills::Worker { requests, replies };
    let mut dirty = false;
    app.handle(key(KeyCode::Char(' ')), &mut dirty);
    let before = draw(&mut app, 100, 30);
    let error = format!("save failed\n{}", "complete details\n".repeat(15));
    sender
        .send(skills::Reply {
            result: Err(anyhow::anyhow!(error)),
            saved: true,
            preview: false,
            open_sources: false,
        })
        .unwrap();
    app.receive();
    let shown = draw(&mut app, 100, 30);
    assert!(shown.contains("save failed"));
    assert!(shown.contains("Skills"));
    assert_eq!(shown.contains("[x] claude"), before.contains("[x] claude"));
    assert!(app.skill_panel.is_some());
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert_eq!(app.input.lines(), &["main draft"]);
    let mut history = Vec::new();
    let buffer = draw_buffer(&mut app, 100, 30);
    app.conversation
        .write_full_for_test(&mut history, &buffer)
        .unwrap();
    let text = String::from_utf8_lossy(&history);
    assert_eq!(text.matches("save failed").count(), 1);
    assert_eq!(text.matches("complete details").count(), 15);
}
