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
fn bang_shell_input_consumes_only_mode_prefix() {
    let mut app = app();
    app.state = State::Ready;
    let (commands, requests) = mpsc::channel();
    app.commands = Some(commands);
    let mut dirty = false;
    app.handle(key(KeyCode::Char('!')), &mut dirty);
    assert!(app.shell_mode);
    assert_eq!(app.input.lines(), &[""]);
    assert!(
        draw_buffer(&mut app, 100, 30)
            .content
            .iter()
            .any(|cell| { cell.symbol() == "─" && cell.fg == statusline::shell_mode() })
    );
    app.submit(PendingKind::Steering);
    assert!(requests.try_recv().is_err());

    app.handle(key(KeyCode::Char('!')), &mut dirty);
    app.handle(Event::Paste(" false".into()), &mut dirty);
    assert_eq!(app.input.lines(), &["! false"]);
    app.handle(key(KeyCode::Enter), &mut dirty);
    let WorkerCommand::Shell { command, .. } = requests.try_recv().unwrap() else {
        panic!("应提交本地 shell 命令");
    };
    assert_eq!(command, "! false");
    assert_eq!(app.input_history, ["!! false"]);
    assert!(!app.shell_mode);
    assert_eq!(app.input.lines(), &[""]);
}

#[test]
fn bang_shell_paste_history_and_picker_preserve_mode() {
    let mut app = app();
    app.state = State::Ready;
    let mut dirty = false;
    app.handle(
        Event::Paste("  !printf '你好!'\n! false".into()),
        &mut dirty,
    );
    assert!(app.shell_mode);
    assert_eq!(app.input.lines(), &["  printf '你好!'", "! false"]);
    assert_eq!(app.input.cursor(), ratatui_textarea::DataCursor(1, 7));
    assert!(app.input.undo());
    assert_eq!(app.input.lines(), &[""]);
    assert!(app.input.redo());
    assert_eq!(app.input.lines(), &["  printf '你好!'", "! false"]);
    app.input_history.push("ordinary prompt".into());
    app.navigate_history(true);
    assert!(!app.shell_mode);
    app.navigate_history(false);
    assert!(app.shell_mode);
    assert_eq!(app.input.lines(), &["  printf '你好!'", "! false"]);
    app.open_model_picker();
    assert!(!app.shell_mode);
    app.close_model_panel();
    assert!(app.shell_mode);
    app.remember_input();
    app.clear_input();
    app.navigate_history(true);
    assert!(app.shell_mode);
    assert_eq!(app.input.lines(), &["  printf '你好!'", "! false"]);
}

#[test]
fn bang_shell_input_exits_mode_and_suppresses_menus() {
    let mut app = app();
    app.state = State::Ready;
    let mut dirty = false;
    app.handle(key(KeyCode::Char('!')), &mut dirty);
    app.handle(key(KeyCode::Backspace), &mut dirty);
    assert!(!app.shell_mode);
    app.handle(Event::Paste("!/model @file".into()), &mut dirty);
    assert!(app.shell_mode);
    assert!(app.menu.is_none());
    assert!(app.model_panel.is_none());
    assert!(app.file_query.is_none());
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(!app.shell_mode);
    assert_eq!(app.input.lines(), &["/model @file"]);
    app.clear_input();
    app.handle(Event::Paste("!echo ok".into()), &mut dirty);
    app.handle(control('a'), &mut dirty);
    app.handle(control('k'), &mut dirty);
    assert!(!app.shell_mode);
    assert_eq!(app.input.lines(), &[""]);
}

#[test]
fn bang_shell_cleanup_failure_disables_future_execution() {
    let mut app = app();
    app.state = State::RunningShell;
    app.input.insert_str("draft remains");
    let (commands, requests) = mpsc::channel();
    let (events, receiver) = mpsc::channel();
    app.commands = Some(commands);
    app.events = Some(receiver);
    events
        .send(WorkerEvent::ShellCompleted(Err(
            pp_core::cancellation::CleanupError(anyhow::anyhow!("process group still alive"))
                .into(),
        )))
        .unwrap();
    drop(events);
    app.receive();
    assert!(matches!(app.state, State::Unavailable));
    assert_eq!(app.input.lines(), &["draft remains"]);
    assert_eq!(
        app.unavailable.as_deref(),
        Some("Execution unavailable: shell cleanup failed; restart pp.")
    );
    assert!(draw(&mut app, 100, 30).contains("process group still alive"));
    app.submit(PendingKind::Steering);
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
fn completed_turn_uses_worker_timing_and_preserves_draft() {
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
}

#[test]
fn cancelled_and_failed_turns_have_no_completion_summary() {
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
fn task_cancel_waits_for_completed_and_preserves_editable_draft() {
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
}

#[test]
fn task_cancel_committed_completion_wins_and_next_token_is_independent() {
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
    let WorkerCommand::Run {
        cancellation: old, ..
    } = requests.try_recv().unwrap()
    else {
        panic!()
    };
    // 完成已在 worker 提交，UI 尚未接到 Completed；迟到 Esc 只取消旧 token。
    events
        .send(WorkerEvent::completed(
            Instant::now(),
            Ok(RunOutcome::Completed),
        ))
        .unwrap();
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(old.is_cancelled());
    app.receive();
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(matches!(app.state, State::Ready));
    app.input.insert_str("next task");
    app.handle(key(KeyCode::Enter), &mut dirty);
    let WorkerCommand::Run {
        cancellation: next, ..
    } = requests.try_recv().unwrap()
    else {
        panic!()
    };
    old.cancel();
    assert!(!next.is_cancelled());
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(next.is_cancelled());
    assert!(matches!(app.state, State::Cancelling));
    assert!(app.handle(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &mut dirty
    ));
}

#[test]
fn notification_task_failure_enters_transcript_without_session_noise() {
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
    assert!(history.contains("partial answer"));
    assert!(!history.contains("fixture-session"));
    assert_eq!(history.matches("request failed").count(), 1);
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("partial answer"));
    assert!(screen.contains("request failed"));
    assert!(!screen.contains("Session:"));
    assert_eq!(app.input.lines(), &["保留草稿🙂"]);
    assert_eq!(app.input.cursor(), cursor);
}

#[test]
fn notification_startup_and_worker_errors_remain_in_message_history() {
    let mut app = App::new(Err(anyhow::anyhow!("fixture configuration unavailable"))).unwrap();
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
    assert!(draw(&mut app, 100, 30).contains("fixture configuration unavailable"));
    assert!(!app.show_welcome);
    app.input.insert_str("草稿");
    app.fail_worker();
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
    assert!(draw(&mut app, 100, 30).contains("agent worker channel closed"));
    assert!(matches!(app.state, State::Unavailable));
    let screen = draw(&mut app, 100, 30);
    assert!(!screen.contains("▄▄▄██▀▀██"));
    assert!(screen.contains("configuration unavailable"));
    assert!(screen.contains("channel closed"));
    assert_eq!(app.input.lines(), &["草稿"]);
}

#[test]
fn pipe_store_binding_is_idempotent_and_bumps_epoch_only_on_new_root() {
    let mut app = app();
    app.state = State::Ready;
    let first = pp_bash::PipeStore::new();
    app.receive_event_for_test(WorkerEvent::PipeStoreBound {
        store: first.clone(),
    });
    assert_eq!(app.pipe_store_epoch, 1);
    assert!(
        app.pipe_store
            .as_ref()
            .is_some_and(|store| store.same_instance(&first))
    );
    // 同一实例重复发布不产生新 epoch。
    app.receive_event_for_test(WorkerEvent::PipeStoreBound {
        store: first.clone(),
    });
    assert_eq!(app.pipe_store_epoch, 1);
    let second = pp_bash::PipeStore::new();
    app.receive_event_for_test(WorkerEvent::PipeStoreBound {
        store: second.clone(),
    });
    assert_eq!(app.pipe_store_epoch, 2);
    assert!(
        app.pipe_store
            .as_ref()
            .is_some_and(|store| store.same_instance(&second))
    );
}

#[test]
fn notification_filters_routine_feedback_and_keeps_independent_errors() {
    let mut app = app();
    let before = draw(&mut app, 100, 30);
    app.notify(Level::Normal, "routine success".into());
    app.notify(Level::Error, " \n\t".into());
    let (events, receiver) = mpsc::channel();
    app.events = Some(receiver);
    events
        .send(WorkerEvent::Progress(AgentEvent::RequestRetrying {
            reason: pp_core::events::RequestRetryReason::HttpStatus(503),
            attempt: 1,
            wait_ms: 30_000,
        }))
        .unwrap();
    app.receive();
    // 重试不再静默：固定信息框可见，但对话与历史不追加条目。
    let retrying = draw(&mut app, 100, 30);
    assert_ne!(retrying, before);
    assert!(retrying.contains("Retrying"), "{retrying}");
    assert!(
        retrying.contains("attempt 1 failed: HTTP status 503; retrying in"),
        "{retrying}"
    );
    events
        .send(WorkerEvent::Progress(AgentEvent::ModelRequestFinished {
            elapsed_ms: 1,
            usage: None,
            success: false,
        }))
        .unwrap();
    app.receive();
    assert_eq!(draw(&mut app, 100, 30), before);
    app.notify(Level::Error, "first failure".into());
    app.notify(Level::Error, "second failure".into());
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("first failure"));
    assert!(screen.contains("second failure"));
}

#[test]
fn retry_notice_counts_down_in_a_fixed_box_and_expires() {
    let mut app = app();
    let (events, receiver) = mpsc::channel();
    app.events = Some(receiver);
    events
        .send(WorkerEvent::Progress(AgentEvent::RequestRetrying {
            reason: pp_core::events::RequestRetryReason::HttpStatus(400),
            attempt: 3,
            wait_ms: 60_000,
        }))
        .unwrap();
    app.receive();
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("Retrying"), "{screen}");
    assert!(
        screen.contains("attempt 3 failed: HTTP status 400; retrying in 60s"),
        "{screen}"
    );
    assert!(screen.contains("Esc cancels"), "{screen}");
    // 同一秒内保持不变，整秒递减时才要求重绘。
    let now = Instant::now();
    assert!(!app.retry_tick(now + Duration::from_millis(100)));
    assert!(app.retry_tick(now + Duration::from_secs(1)));
    assert!(draw(&mut app, 100, 30).contains("retrying in 59s"));
    assert!(!app.retry_tick(now + Duration::from_millis(1100)));
    assert!(app.retry_visible());
    // 到期自动收起，不需要额外事件。
    assert!(app.retry_tick(Instant::now() + Duration::from_secs(61)));
    assert!(!app.retry_visible());
    let screen = draw(&mut app, 100, 30);
    assert!(!screen.contains("Retrying"), "{screen}");
}

#[test]
fn initial_file_command_submits_run_and_clears_input() {
    let (mut app, requests) = file_commands_app(review_files());
    assert!(!app.submit_initial_command("/review src/main.rs"));
    let WorkerCommand::Run { prompt, .. } = requests.try_recv().unwrap() else {
        panic!("启动文件命令应下发 WorkerCommand::Run");
    };
    assert_eq!(prompt, "Review src/main.rs now.");
    assert_eq!(app.input.lines(), &[""]);
    assert_eq!(app.input_history, ["/review src/main.rs"]);
    assert!(matches!(app.state, State::Running));
}

#[test]
fn initial_unknown_command_notifies_and_keeps_draft() {
    let (mut app, requests) = file_commands_app(review_files());
    assert!(!app.submit_initial_command("/nope"));
    assert!(requests.try_recv().is_err());
    assert_eq!(app.input.lines(), &["/nope"]);
    assert!(draw(&mut app, 100, 30).contains("unknown command: /nope; use /help"));
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
}

#[test]
fn initial_parse_error_notifies_and_keeps_draft() {
    let (mut app, requests) = file_commands_app(review_files());
    assert!(!app.submit_initial_command("/mcp nope"));
    assert!(requests.try_recv().is_err());
    assert_eq!(app.input.lines(), &["/mcp nope"]);
    assert!(draw(&mut app, 100, 30).contains("usage: /mcp"));
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
}

#[test]
fn initial_shell_command_consumes_mode_prefix() {
    let (mut app, requests) = file_commands_app(review_files());
    assert!(!app.submit_initial_command("!echo hi"));
    let WorkerCommand::Shell { command, .. } = requests.try_recv().unwrap() else {
        panic!("启动 shell 命令应下发 WorkerCommand::Shell");
    };
    assert_eq!(command, "echo hi");
    assert_eq!(app.input.lines(), &[""]);
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

    // 1. 运行中、取消中、Plan 审查中、不可用状态均可使用 F6 / Alt+Shift+A 循环切换且保留草稿。
    for (state, key_event, expected) in [
        (State::Running, key(KeyCode::F(6)), PermissionMode::Yolo),
        (
            State::Cancelling,
            Event::Key(KeyEvent::new(
                KeyCode::Char('A'),
                KeyModifiers::ALT | KeyModifiers::SHIFT,
            )),
            PermissionMode::AlwaysAsk,
        ),
        (
            State::ReviewingPlan,
            key(KeyCode::F(6)),
            PermissionMode::AcceptEdits,
        ),
        (State::Unavailable, key(KeyCode::F(6)), PermissionMode::Yolo),
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

    // 2. 存在待处理输入队列且处于问询弹窗 / 面板中时，快捷键依然生效且不关闭弹窗。
    app.state = State::Running;
    app.input_queue
        .enqueue(PendingKind::Steering, "queued".into(), None, true)
        .unwrap();
    assert!(app.has_pending());
    let (_answers, _token) = ask_fixture(&mut app);
    app.handle(key(KeyCode::F(6)), &mut false);
    assert_eq!(gate.snapshot(), PermissionMode::AlwaysAsk);
    assert_eq!(app.permission_mode, Some(PermissionMode::AlwaysAsk));
    assert!(app.ask_panel.is_some());
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::SyncPermissions)
    ));
    app.ask_panel = None;
    app.ask_reply = None;
    app.ask_id = None;
    app.ask_cancellation = None;

    // 3. Plan 审查与待处理队列期间提交 `/permissions` 命令也立即生效。
    app.state = State::ReviewingPlan;
    app.clear_input();
    app.insert_input("/permissions yolo".into());
    app.submit(PendingKind::Steering);
    assert_eq!(gate.snapshot(), PermissionMode::Yolo);
    assert_eq!(app.permission_mode, Some(PermissionMode::Yolo));
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
    assert!(screen.contains("mcp__remote__echo"), "{screen}");
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
    for label in [
        "Allow once",
        "Allow for this session",
        "Always allow globally",
        "Deny",
    ] {
        assert!(
            options.next().is_some_and(|row| row.contains(label)),
            "{screen}"
        );
    }
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
    assert!(short.contains("mcp__remote__echo"), "{short}");
    assert!(!short.contains("workspace sandbox"), "{short}");
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
fn unavailable_model_binds_configured_permission_mode_before_cycling() {
    use pp_config::config::PermissionMode;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(
        root.join("settings.yml"),
        "tools:\n  permissionMode: always-ask\n",
    )
    .unwrap();
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
    assert_eq!(app.permission_mode, Some(PermissionMode::AlwaysAsk));
    assert_eq!(app.permission.snapshot(), PermissionMode::AlwaysAsk);

    // 第一次按 F6 应从配置的 Always-ask 切到 Accept-edits，而非误从默认 Accept-edits 跳到 YOLO。
    app.handle(key(KeyCode::F(6)), &mut false);
    assert_eq!(app.permission_mode, Some(PermissionMode::AcceptEdits));
    assert_eq!(app.permission.snapshot(), PermissionMode::AcceptEdits);
}

#[test]
fn modal_permission_overlay_accepts_real_keys_and_runs_rule_io_off_event_loop() {
    use pp_config::config::PermissionMode;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let rules_path = root.join("permission.toml");
    std::fs::write(&rules_path, "tools = [\"write\"]\n").unwrap();
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

    // 1. 权限确认弹窗打开时，通过真实按键 F7 -> 输入 `yolo` -> Enter 直接切换档位，不关闭底层弹窗。
    let (_answers, _token) = ask_fixture(&mut app);
    app.handle(key(KeyCode::F(7)), &mut false);
    assert!(app.permission_overlay.is_some());
    assert!(draw(&mut app, 100, 30).contains("Permission mode: accept-edits"));
    type_text(&mut app, "yolo");
    app.handle(key(KeyCode::Enter), &mut false);
    assert_eq!(gate.snapshot(), PermissionMode::Yolo);
    assert_eq!(app.permission_mode, Some(PermissionMode::Yolo));
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

    // 2. 在全屏模态面板（context_panel）打开时，同样可通过 F7 -> Enter 查看详情并切档。
    app.context_panel = Some(crate::tui::context_panel::Panel::new());
    app.handle(key(KeyCode::F(7)), &mut false);
    app.handle(key(KeyCode::Enter), &mut false);
    assert!(draw(&mut app, 100, 30).contains("Permission mode: yolo"));
    type_text(&mut app, "always-ask");
    app.handle(key(KeyCode::Enter), &mut false);
    assert_eq!(gate.snapshot(), PermissionMode::AlwaysAsk);
    assert!(app.context_panel.is_some());

    // 3. 另一句柄持排他锁占用 permission.lock 时，`/permissions revoke` 交给独立 worker，不阻塞事件循环。
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
    // SAFETY: `lock_file` 在此处及后续持锁期间保持打开，fd 有效。
    assert_eq!(
        unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX) },
        0
    );

    type_text(&mut app, "revoke write");
    app.handle(key(KeyCode::Enter), &mut false);
    assert!(draw(&mut app, 100, 30).contains("Revoking saved permission rule..."));
    // 持锁期间事件循环未被阻塞：立即按 F6 仍可实时切换内存权限档位。
    app.handle(key(KeyCode::F(6)), &mut false);
    assert_eq!(gate.snapshot(), PermissionMode::AcceptEdits);
    assert!(draw(&mut app, 100, 30).contains("Permission mode: accept-edits"));

    // 释放锁后后台 worker 完成撤销并回传结果。
    drop(lock_file);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !draw(&mut app, 100, 30).contains("Saved permission revoked.") {
        assert!(Instant::now() < deadline, "revoke worker reply timed out");
        app.receive();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(gate.list_rules().unwrap(), "No saved permission rules.");

    // 4. 通过浮层 `/permissions allow cargo test` 添加命令级前缀规则并按 `bash:cargo test` 撤销。
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
            .contains("Bash (allow): cargo test")
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
