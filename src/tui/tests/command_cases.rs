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
            .any(|cell| { cell.symbol() == "─" && cell.fg == statusline::SHELL_MODE })
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
            crate::cancellation::CleanupError(anyhow::anyhow!("process group still alive")).into(),
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
        let expected = format!("✻ Completed · {duration} · done 19:06");
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains(&expected), "{screen}");
        assert_eq!(app.conversation.task_duration().as_deref(), Some(duration));
        assert_eq!(screen.matches("Completed ·").count(), 1);
        assert!(screen.find("finished answer").unwrap() < screen.find(&expected).unwrap());
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
    assert!(screen.contains("\u{f12b7} Cancelling"));
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
    assert_eq!(
        draw(&mut app, 100, 30)
            .matches(crate::cancellation::CANCELLED_NOTE)
            .count(),
        1
    );
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
    assert!(!draw(&mut app, 100, 30).contains(crate::cancellation::CANCELLED_NOTE));
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
fn notification_filters_routine_feedback_and_keeps_independent_errors() {
    let mut app = app();
    let before = draw(&mut app, 100, 30);
    app.notify(Level::Normal, "routine success".into());
    app.notify(Level::Error, " \n\t".into());
    let (events, receiver) = mpsc::channel();
    app.events = Some(receiver);
    events
        .send(WorkerEvent::Progress(AgentEvent::RequestRetrying {
            reason: crate::events::RequestRetryReason::HttpStatus(503),
            attempt: 1,
            wait_ms: 30_000,
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
