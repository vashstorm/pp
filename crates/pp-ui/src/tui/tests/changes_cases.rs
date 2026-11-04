use super::*;

#[test]
fn changes_loading_escape_waits_for_reply_and_never_reopens_panel() {
    let (mut app, requests, events) = plan_app();
    app.input.insert_str("existing draft");
    let cursor = app.input.cursor();
    let before = draw(&mut app, 100, 30);
    app.open_changes();
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::ReadChanges)
    ));
    let loading = draw(&mut app, 100, 30);
    assert!(loading.contains("Latest task changes"));
    assert!(!loading.contains("existing draft"));
    let mut dirty = false;
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.changes_panel.is_none());
    assert!(matches!(app.state, State::ReadingChanges));
    app.open_model_picker();
    app.open_tree();
    assert!(requests.try_recv().is_err());
    events
        .send(WorkerEvent::ChangesRead(changes_view(false)))
        .unwrap();
    app.receive();
    assert!(matches!(app.state, State::Ready));
    assert!(app.changes_panel.is_none());
    assert_eq!(app.input.lines(), &["existing draft"]);
    assert_eq!(app.input.cursor(), cursor);
    assert_eq!(draw(&mut app, 100, 30), before);
}

#[test]
fn changes_panel_preserves_draft_history_viewport_and_blocks_shortcuts() {
    let (mut app, requests, events) = plan_app();
    for index in 0..40 {
        app.append("You", &format!("original message {index}"));
    }
    app.input_history = vec!["old prompt".into()];
    app.input.insert_str("existing draft\nsecond line");
    app.input.move_cursor(CursorMove::Head);
    let cursor = app.input.cursor();
    draw(&mut app, 100, 30);
    app.conversation.page_up();
    let before = draw(&mut app, 100, 30);
    app.open_changes();
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::ReadChanges)
    ));
    events
        .send(WorkerEvent::ChangesRead(changes_view(false)))
        .unwrap();
    app.receive();
    let panel = draw(&mut app, 100, 30);
    assert!(panel.contains("demo.txt"));
    assert!(panel.contains("user-local") && panel.contains("agent-final"));
    assert!(panel.contains("fixture-cwd"));
    assert!(!panel.contains("existing draft"));
    let mut dirty = false;
    for event in [
        key(KeyCode::Up),
        key(KeyCode::Down),
        key(KeyCode::PageUp),
        key(KeyCode::PageDown),
        function_key('p'),
        function_key('q'),
        function_key('r'),
        control('o'),
        control('a'),
        control('k'),
        key(KeyCode::F(1)),
        key(KeyCode::BackTab),
        Event::Paste("must not edit".into()),
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        }),
        Event::Resize(50, 12),
    ] {
        app.handle(event, &mut dirty);
    }
    draw(&mut app, 50, 12);
    draw(&mut app, 100, 30);
    assert!(requests.try_recv().is_err());
    assert!(app.model_picker_draft.is_none());
    assert!(!app.queue_menu);
    assert_eq!(app.input.lines(), &["existing draft", "second line"]);
    assert_eq!(app.input.cursor(), cursor);
    assert_eq!(app.input_history, ["old prompt"]);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert_eq!(draw(&mut app, 100, 30), before);
}

#[test]
fn changes_pending_followup_stays_queued_through_loading_view_and_confirmation() {
    let (mut app, requests, events) = plan_app();
    app.input_queue
        .enqueue(PendingKind::FollowUp, "next task".into(), None, false)
        .unwrap();
    app.input.insert_str("/changes");
    app.submit(PendingKind::Steering);
    assert!(app.input.is_empty());
    assert_eq!(app.input_history, ["/changes"]);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::ReadChanges)
    ));
    assert!(!app.queue_paused);
    assert!(!app.dispatch_next(true));
    events
        .send(WorkerEvent::ChangesRead(changes_view(false)))
        .unwrap();
    app.receive();
    assert!(app.has_pending());
    assert!(!app.dispatch_next(false));
    let mut dirty = false;
    app.handle(key(KeyCode::Char('u')), &mut dirty);
    assert!(draw(&mut app, 100, 30).contains("Undo file"));
    app.handle(function_key('r'), &mut dirty);
    app.handle(function_key('q'), &mut dirty);
    assert!(!app.dispatch_next(true));
    assert!(requests.try_recv().is_err());
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.changes_panel.is_some());
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.changes_panel.is_none());
    app.receive();
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt, .. }) if prompt == "next task")
    );
    assert!(!app.has_pending());
}

#[test]
fn changes_running_command_is_rejected_without_steering_or_history() {
    for state in [State::Running, State::Cancelling] {
        let (mut app, requests, _events) = plan_app();
        app.state = state;
        app.input_queue.begin_run().unwrap();
        app.input.insert_str("/changes");
        let mut dirty = false;
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert_eq!(app.input.lines(), &["/changes"]);
        assert!(app.input_history.is_empty());
        assert!(!app.has_pending());
        assert!(requests.try_recv().is_err());
        let screen = draw(&mut app, 110, 30);
        assert!(screen.contains("Finish or cancel the current task before opening"));
        assert!(screen.contains("/changes."));
    }
}

#[test]
fn changes_plan_mode_never_sends_undo_and_mode_or_tree_cannot_bypass_panel() {
    let (mut app, requests, events) = plan_app();
    app.workflow.mode = WorkMode::Plan;
    app.open_changes();
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::ReadChanges)
    ));
    events
        .send(WorkerEvent::ChangesRead(changes_view(false)))
        .unwrap();
    app.receive();
    let mut dirty = false;
    app.handle(key(KeyCode::Char('u')), &mut dirty);
    app.handle(key(KeyCode::Tab), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(draw(&mut app, 100, 30).contains("Undo is unavailable in Plan mode."));
    assert!(!app.plan_transition_ready("pending input"));
    app.open_tree();
    app.open_model_picker();
    app.request_plan_read();
    assert!(app.tree_panel.is_none());
    assert!(app.model_picker_draft.is_none());
    assert!(requests.try_recv().is_err());
}

#[test]
fn changes_undo_error_refresh_keeps_reason_and_success_removes_file() {
    let (mut app, requests, events) = plan_app();
    app.open_changes();
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::ReadChanges)
    ));
    events
        .send(WorkerEvent::ChangesRead(changes_view(false)))
        .unwrap();
    app.receive();
    let mut dirty = false;
    app.handle(key(KeyCode::Char('u')), &mut dirty);
    // Confirmation starts on Cancel: Enter never dispatches an undo.
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(requests.try_recv().is_err());
    app.handle(key(KeyCode::Char('u')), &mut dirty);
    app.handle(key(KeyCode::Tab), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::UndoFile(token)) if token.path == std::path::Path::new("demo.txt"))
    );
    assert!(matches!(app.state, State::UndoingFile));
    app.handle(key(KeyCode::Enter), &mut dirty);
    app.handle(key(KeyCode::Char('r')), &mut dirty);
    assert!(requests.try_recv().is_err());
    assert!(draw(&mut app, 100, 30).contains("Undoing file"));
    events
        .send(WorkerEvent::FileUndone(Err(anyhow::anyhow!(
            "specific refusal reason"
        ))))
        .unwrap();
    app.receive();
    assert!(matches!(app.state, State::UndoingFile));
    events
        .send(WorkerEvent::ChangesRead(changes_view(true)))
        .unwrap();
    app.receive();
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("specific refusal reason"));
    assert!(screen.contains("Blocked"));
    assert!(screen.contains("demo.txt"));
    assert!(matches!(app.state, State::Ready));
    app.handle(key(KeyCode::Char('r')), &mut dirty);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::ReadChanges)
    ));
    events
        .send(WorkerEvent::ChangesRead(changes_view(false)))
        .unwrap();
    app.receive();
    assert!(!draw(&mut app, 100, 30).contains("specific refusal reason"));
    app.handle(key(KeyCode::Char('u')), &mut dirty);
    app.handle(key(KeyCode::Tab), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::UndoFile(_))
    ));
    events
        .send(WorkerEvent::FileUndone(Ok(UndoResult {
            view: changes_empty_view(),
            warning: None,
        })))
        .unwrap();
    app.receive();
    assert!(app.changes_panel.is_some());
    assert!(draw(&mut app, 100, 30).contains("No edit/write changes in the latest task."));
    assert!(matches!(app.state, State::Ready));
}

#[test]
fn changes_worker_disconnect_clears_panel_and_preserves_draft() {
    let (mut app, requests, events) = plan_app();
    app.input.insert_str("preserved draft");
    app.open_changes();
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::ReadChanges)
    ));
    drop(events);
    app.receive();
    assert!(matches!(app.state, State::Unavailable));
    assert!(app.changes_panel.is_none());
    assert_eq!(app.input.lines(), &["preserved draft"]);
}
