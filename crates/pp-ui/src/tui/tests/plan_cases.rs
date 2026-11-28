use super::*;

#[test]
fn plan_transitions_execution_ack_and_progress_cache() {
    {
        let (mut app, requests, events) = plan_app();
        app.workflow = ready_plan_workflow();
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Ready,
        ))));
        for _ in 0..4 {
            app.handle(key(KeyCode::Down), &mut false);
        }
        app.handle(key(KeyCode::Enter), &mut false);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::SavePlanAndLeave { .. })
        ));
        assert!(matches!(app.state, State::ChangingPlan));
        assert_eq!(app.secondary_status.as_deref(), Some("Saving plan..."));
        let (reply, _ack) = mpsc::sync_channel(1);
        let workflow = WorkflowState {
            mode: WorkMode::Execute,
            plan: Some(pp_core::plan::PlanDocument {
                status: PlanStatus::Executed,
                ..ready_plan_workflow().plan.expect("plan")
            }),
            ..Default::default()
        };
        events
            .send(WorkerEvent::ExecutionPrepared {
                workflow,
                session_id: Some("next".into()),
                usage: None,
                replay: None,
                todo_snapshot: None,
                reply,
            })
            .unwrap();
        app.receive();
        assert!(matches!(app.state, State::Ready));
        assert_eq!(app.secondary_status, None);

        let (mut enter, req_enter, _) = plan_app();
        enter.workflow = WorkflowState::default();
        enter.input.insert_str("/plan");
        enter.submit(PendingKind::Steering);
        assert!(matches!(
            req_enter.try_recv(),
            Ok(WorkerCommand::EnterPlan { .. })
        ));
        assert!(matches!(enter.state, State::ChangingPlan));
        assert_eq!(
            enter.secondary_status.as_deref(),
            Some("Entering Plan mode...")
        );
        enter.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(WorkflowState {
            mode: WorkMode::Plan,
            plan: None,
            ..Default::default()
        })));
        assert!(matches!(enter.state, State::Ready));
        assert_eq!(enter.secondary_status, None);

        let (mut leave, req_leave, _) = plan_app();
        leave.workflow = WorkflowState {
            mode: WorkMode::Plan,
            plan: None,
            ..Default::default()
        };
        leave.input.insert_str("/plan");
        leave.submit(PendingKind::Steering);
        assert!(matches!(
            req_leave.try_recv(),
            Ok(WorkerCommand::LeavePlan { .. })
        ));
        assert!(matches!(leave.state, State::ChangingPlan));
        assert_eq!(
            leave.secondary_status.as_deref(),
            Some("Leaving Plan mode...")
        );
        leave.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(WorkflowState::default())));
        assert!(matches!(leave.state, State::Ready));
        assert_eq!(leave.secondary_status, None);
    }

    {
        let (mut app, _requests, events) = plan_app();
        app.state = State::PreparingExecution;
        let (reply, ack) = mpsc::sync_channel(1);
        let workflow = WorkflowState {
            mode: WorkMode::Execute,
            plan: Some(pp_core::plan::PlanDocument {
                status: PlanStatus::Executed,
                ..ready_plan_workflow().plan.expect("plan")
            }),
            ..Default::default()
        };
        events
            .send(WorkerEvent::ExecutionPrepared {
                workflow: workflow.clone(),
                session_id: Some("next".into()),
                usage: None,
                replay: None,
                todo_snapshot: None,
                reply,
            })
            .unwrap();
        app.receive();
        assert!(ack.try_recv().is_ok());
        assert_eq!(app.workflow, workflow);
        assert_eq!(app.session_id.as_deref(), Some("next"));
        assert!(matches!(app.state, State::Running) && !app.show_welcome);

        let (mut awaiting, requests, events) = plan_app();
        events
            .send(WorkerEvent::completed(
                Instant::now(),
                Ok(RunOutcome::AwaitingPlanReview),
            ))
            .unwrap();
        awaiting.receive();
        assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadPlan)));
        assert!(matches!(awaiting.state, State::ReadingPlan));
    }

    {
        let (mut app, _requests, _events) = plan_app();
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Executed,
        ))));
        app.receive_event_for_test(WorkerEvent::Progress(AgentEvent::PlanProgressUpdated {
            plan_id: "0123456789abcdef0123456789abcdef".into(),
            revision: 3,
            content: "- [x] step one\n- [ ] step two\n".into(),
            file_name: None,
            location: None,
            status: None,
        }));
        assert_eq!(app.plan_progress, (1, 2));
        assert_eq!(app.workflow.plan.as_ref().expect("plan").revision, 3);
    }
}

#[test]
fn plan_prompt_standby_toggle_and_readonly_progress_guards() {
    for workflow in [WorkflowState::default(), ready_plan_workflow()] {
        let (mut app, requests, _events) = plan_app();
        app.workflow = workflow;
        app.input.insert_str("/plan research login flow");
        app.submit(PendingKind::Steering);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::EnterPlan { .. })
        ));
        assert!(requests.try_recv().is_err());
        assert!(matches!(app.state, State::ChangingPlan));
        assert!(app.input.is_empty());
        assert_eq!(
            app.input_history.last().unwrap(),
            "/plan research login flow"
        );
        app.input.insert_str("new draft");
        app.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(ready_plan_workflow())));
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt, .. })
                if prompt == "research login flow")
        );
        assert!(requests.try_recv().is_err());
        assert!(matches!(app.state, State::Running));
        assert!(app.plan_panel.is_none() && app.plan_after_enter.is_none());
        assert_eq!(app.input.lines(), &["new draft"]);
    }

    for draft in ["", "new draft"] {
        let (mut app, requests, _events) = plan_app();
        app.input.insert_str("/plan research");
        app.submit(PendingKind::Steering);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::EnterPlan { .. })
        ));
        app.input.insert_str(draft);
        app.receive_event_for_test(WorkerEvent::WorkflowChanged(Err(anyhow::anyhow!(
            "cleanup failed"
        ))));
        assert!(requests.try_recv().is_err());
        assert!(app.plan_retry_enter && app.plan_after_enter.is_none());
        assert_eq!(
            app.input.lines(),
            &[if draft.is_empty() {
                "/plan research"
            } else {
                draft
            }]
        );
        app.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(ready_plan_workflow())));
        assert!(requests.try_recv().is_err());
    }

    let (mut app, requests, _events) = plan_app();
    app.input_queue
        .enqueue(PendingKind::FollowUp, "pending".into(), None, false)
        .unwrap();
    app.input.insert_str("/plan research");
    app.submit(PendingKind::Steering);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.input.lines(), &["/plan research"]);
    assert!(app.plan_after_enter.is_none());

    let (mut busy, requests, _events) = plan_app();
    busy.state = State::Running;
    busy.workflow = WorkflowState {
        mode: WorkMode::Plan,
        plan: None,
        ..Default::default()
    };
    for command in ["/plan", "/plan research"] {
        busy.input.insert_str(command);
        busy.submit(PendingKind::Steering);
        assert_eq!(busy.input.lines(), &[command]);
        busy.input = editor();
    }
    assert!(requests.try_recv().is_err());
    assert_notification(&mut busy, Color::Rgb(224, 108, 117));

    {
        let (mut app, requests, _events) = plan_app();
        app.workflow = ready_plan_workflow();
        app.state = State::Running;
        app.input.insert_str("/plan view");
        app.submit(PendingKind::Steering);
        assert!(requests.try_recv().is_err());
        let panel = app.plan_panel.as_ref().expect("progress view");
        assert!(panel.token().is_none());
        for index in 0..4 {
            assert!(!panel.action_enabled(index));
        }
        // Esc only closes the panel and does not cancel the task.
        app.handle(key(KeyCode::Esc), &mut false);
        assert!(app.plan_panel.is_none());
        assert!(matches!(app.state, State::Running));
    }

    {
        let (mut app, requests, _events) = plan_app();
        // Execute: a bare /plan enters Plan standby (Plan+None, no review opened).
        app.input.insert_str("/plan");
        app.submit(PendingKind::Steering);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::EnterPlan { .. })
        ));
        assert!(matches!(
            app.plan_after_enter,
            Some(PlanEnterAction::Standby)
        ));
        app.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(WorkflowState {
            mode: WorkMode::Plan,
            plan: None,
            ..Default::default()
        })));
        assert!(app.plan_after_enter.is_none());
        assert!(!app.plan_retry_enter);
        assert!(app.plan_panel.is_none());

        // Empty standby can leave directly: a bare /plan sends LeavePlan with no confirmation dialog.
        app.input.insert_str("/plan");
        app.submit(PendingKind::Steering);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::LeavePlan { .. })
        ));
        app.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(WorkflowState::default())));
        assert_eq!(app.workflow.mode, WorkMode::Execute);
        assert!(requests.try_recv().is_err());
    }
}

#[test]
fn plan_ready_execute_keep_focus_discussion_and_context_usage() {
    {
        let (mut queued, requests, _events) = plan_app();
        queued.workflow = ready_plan_workflow();
        queued.queue_paused = false;
        queued
            .input_queue
            .enqueue(PendingKind::Steering, "follow up".to_string(), None, false)
            .unwrap();
        assert!(!queued.dispatch_next(false));
        assert!(requests.try_recv().is_err());
        queued.resume_queue();
        assert!(matches!(requests.try_recv(), Ok(WorkerCommand::Run { .. })));

        let (mut approve, requests, _events) = plan_app();
        approve.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Ready,
        ))));
        assert!(approve.plan_panel.is_some());
        assert_eq!(approve.plan_progress, (0, 2));
        approve.handle(key(KeyCode::Down), &mut false);
        approve.handle(key(KeyCode::Enter), &mut false);
        let WorkerCommand::ApprovePlan { token, context, .. } = requests.try_recv().unwrap() else {
            panic!("expected ApprovePlan command");
        };
        assert_eq!(token.revision, 2);
        assert_eq!(context, ExecutionContext::Keep);
        assert!(matches!(approve.state, State::PreparingExecution));
        assert_eq!(
            approve.secondary_status.as_deref(),
            Some("Preparing execution...")
        );

        let (mut focus, requests, _events) = plan_app();
        focus.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Ready,
        ))));
        let muted = transcript_style::muted();
        let accent = transcript_style::accent();
        for colors in [
            [muted, accent, muted],
            [muted, muted, statusline::accent()],
            [accent, muted, muted],
            [muted, accent, muted],
        ] {
            let buffer = draw_buffer(&mut focus, 100, 30);
            let actions_y = (0..30).find(|&y| buffer[(0, y)].symbol() == "╭").unwrap();
            for (y, color) in [0, actions_y, 26].into_iter().zip(colors) {
                assert_eq!(buffer[(99, y)].fg, color, "border row: {y}");
            }
            let rows: Vec<String> = (actions_y + 1..actions_y + 6)
                .map(|y| (1..99).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            assert!(rows[0].starts_with("❯ Execute in a new session"));
            assert_eq!(rows.iter().filter(|row| row.contains('❯')).count(), 1);
            for (index, row) in rows.iter().enumerate() {
                assert!(!row.contains(&format!("{}. ", index + 1)), "{row}");
            }
            let hint: String = (1..99)
                .map(|x| buffer[(x, actions_y + 6)].symbol())
                .collect();
            assert!(!hint.contains("1–6 select"), "{hint}");
            focus.handle(key(KeyCode::Tab), &mut false);
        }
        assert!(requests.try_recv().is_err());
    }

    {
        let (mut app, requests, _events) = plan_app();
        app.input.insert_str("existing ");
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Ready,
        ))));
        app.handle(Event::Paste("question".into()), &mut false);
        assert_eq!(app.input.lines(), &["existing question"]);
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("step one"), "{screen}");
        assert!(
            screen.contains("Save draft and leave Plan mode"),
            "{screen}"
        );
        assert!(screen.contains("existing question"), "{screen}");
        assert!(
            !screen.contains("Plan ready · /plan view to review"),
            "{screen}"
        );
        app.handle(key(KeyCode::Enter), &mut false);
        let WorkerCommand::Run { prompt, .. } = requests.try_recv().unwrap() else {
            panic!("expected discussion run");
        };
        assert_eq!(prompt, "existing question");
        assert!(app.plan_panel.is_none() && app.input.is_empty());

        let (mut usage_app, requests, _events) = plan_app();
        usage_app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Ready,
        ))));
        usage_app.usage = Some(pp_core::context::ContextUsage {
            estimated_tokens: 120_000,
            context_window: Some(500_000),
            auto_compact: true,
            threshold_percent: 80,
            last_request: None,
            session_output_tokens: 0,
        });
        let screen = draw(&mut usage_app, 100, 30);
        assert!(
            screen.contains("Execute with context (120K/500K)"),
            "{screen}"
        );
        assert!(!screen.contains("Continue discussing Plan"), "{screen}");
        usage_app.usage.as_mut().unwrap().estimated_tokens = 125_500;
        assert!(draw(&mut usage_app, 100, 30).contains("Execute with context (125.5K/500K)"));
        usage_app.usage.as_mut().unwrap().context_window = None;
        assert!(draw(&mut usage_app, 100, 30).contains("Execute with context (125.5K/?)"));
        usage_app.usage = None;
        assert!(draw(&mut usage_app, 100, 30).contains("Execute with context (?/?)"));

        usage_app.handle(key(KeyCode::Char('q')), &mut false);
        assert!(usage_app.plan_panel.as_ref().unwrap().input_focused());
        usage_app.handle(
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            &mut false,
        );
        usage_app.handle(Event::Paste("continue discussion".into()), &mut false);
        assert_eq!(usage_app.input.lines(), &["q", "continue discussion"]);
        usage_app.handle(key(KeyCode::Enter), &mut false);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::Run { prompt, .. }) if prompt == "q\ncontinue discussion"
        ));
        assert!(requests.try_recv().is_err());
        assert!(usage_app.plan_panel.is_none() && usage_app.input.is_empty());
    }
}

#[test]
fn plan_leave_discard_confirmation_and_subcommand_guards() {
    let (mut app, requests, _events) = plan_app();
    let mut plan = pp_core::plan::PlanDocument::new().unwrap();
    plan.origin_prompt = "first task".into();
    let id = plan.id.clone();
    app.workflow = WorkflowState {
        mode: WorkMode::Plan,
        plan: Some(plan),
        ..Default::default()
    };
    app.input.insert_str("/plan");
    app.submit(PendingKind::Steering);
    assert!(requests.try_recv().is_err());
    assert!(matches!(app.state, State::Ready) && app.workflow.mode == WorkMode::Plan);
    assert!(app.workflow.plan.is_some());
    let screen = draw(&mut app, 100, 30).replace(' ', "");
    assert!(screen.contains("Discardunsavedchanges?"), "{screen}");
    assert!(screen.contains("Cancel,keepplanning"));
    assert!(screen.contains("Discardchangesandexit"));
    assert!(app.needs_alternate());
    app.input.insert_str("keep input");
    app.handle(key(KeyCode::Enter), &mut false);
    assert!(app.plan_leave_confirmation.is_none() && requests.try_recv().is_err());
    assert_eq!(app.input.lines(), &["keep input"]);
    assert_eq!(app.workflow.plan.as_ref().unwrap().id, id);

    app.apply_plan_command(PlanCommand::Toggle);
    app.handle(Event::Paste("cannot insert".into()), &mut false);
    app.handle(function_key('p'), &mut false);
    app.handle(key(KeyCode::F(1)), &mut false);
    assert_eq!(app.input.lines(), &["keep input"]);
    assert!(requests.try_recv().is_err());
    app.handle(key(KeyCode::Esc), &mut false);
    assert!(app.plan_leave_confirmation.is_none() && app.workflow.mode == WorkMode::Plan);

    app.apply_plan_command(PlanCommand::Toggle);
    app.handle(key(KeyCode::Down), &mut false);
    app.handle(key(KeyCode::Tab), &mut false);
    let mut repeat = crossterm::event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    repeat.kind = KeyEventKind::Repeat;
    app.handle(Event::Key(repeat), &mut false);
    assert!(requests.try_recv().is_err());
    app.handle(Event::Resize(4, 20), &mut false);
    draw(&mut app, 20, 4);
    app.handle(key(KeyCode::Enter), &mut false);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::LeavePlan { discard_plan_id: Some(token), .. }) if token == id)
    );
    assert!(requests.try_recv().is_err());
    assert_eq!(app.input.lines(), &["keep input"]);

    let (mut stale, req_stale, _) = plan_app();
    let mut stale_plan = pp_core::plan::PlanDocument::new().unwrap();
    stale_plan.origin_prompt = "first task".into();
    stale.workflow = WorkflowState {
        mode: WorkMode::Plan,
        plan: Some(stale_plan),
        ..Default::default()
    };
    stale.apply_plan_command(PlanCommand::Toggle);
    stale.workflow = ready_plan_workflow();
    stale.handle(key(KeyCode::Down), &mut false);
    stale.handle(key(KeyCode::Enter), &mut false);
    assert!(req_stale.try_recv().is_err() && stale.plan_leave_confirmation.is_none());
    assert_eq!(stale.workflow, ready_plan_workflow());

    let (mut new_plan, req_new, _) = plan_app();
    let mut doc = pp_core::plan::PlanDocument::new().unwrap();
    doc.origin_prompt = "first task".into();
    new_plan.workflow = WorkflowState {
        mode: WorkMode::Plan,
        plan: Some(doc),
        ..Default::default()
    };
    new_plan.plan_list_panel = Some(plan_list::Panel::new());
    new_plan.plan_list_new_plan();
    assert!(req_new.try_recv().is_err());
    new_plan.handle(key(KeyCode::Esc), &mut false);
    assert!(new_plan.plan_list_panel.is_some());
    new_plan.plan_list_new_plan();
    new_plan.handle(key(KeyCode::Down), &mut false);
    new_plan.handle(key(KeyCode::Enter), &mut false);
    assert!(matches!(
        req_new.try_recv(),
        Ok(WorkerCommand::LeavePlan {
            discard_plan_id: Some(_),
            ..
        })
    ));
    assert!(new_plan.plan_list_panel.is_none());
    new_plan.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(WorkflowState::default())));
    assert!(matches!(
        req_new.try_recv(),
        Ok(WorkerCommand::EnterPlan { .. })
    ));
    assert!(req_new.try_recv().is_err());

    let (mut retry_leave, req_retry, _) = plan_app();
    retry_leave.workflow = ready_plan_workflow();
    retry_leave.input.insert_str("/plan");
    retry_leave.submit(PendingKind::Steering);
    assert!(matches!(
        req_retry.try_recv(),
        Ok(WorkerCommand::LeavePlan { .. })
    ));
    retry_leave.receive_event_for_test(WorkerEvent::WorkflowChanged(Err(anyhow::anyhow!(
        "exit failed"
    ))));
    assert!(!retry_leave.plan_retry_enter);
    retry_leave.input.insert_str("/plan");
    retry_leave.submit(PendingKind::Steering);
    assert!(matches!(
        req_retry.try_recv(),
        Ok(WorkerCommand::LeavePlan { .. })
    ));

    let (mut old_cmd, req_old, _) = plan_app();
    for command in ["/plan new", "/plan off"] {
        old_cmd.input.insert_str(command);
        old_cmd.submit(PendingKind::Steering);
        assert!(req_old.try_recv().is_err());
        assert_eq!(old_cmd.input.lines(), &[command]);
        assert_eq!(old_cmd.workflow.mode, WorkMode::Execute);
        assert!(old_cmd.workflow.plan.is_none());
        assert_notification(&mut old_cmd, Color::Rgb(224, 108, 117));
        assert!(
            draw(&mut old_cmd, 120, 30)
                .contains("Use /plan to toggle; manage plans with /plan-list.")
        );
        old_cmd.input = editor();
        old_cmd.conversation = old_cmd.conversation.fresh();
    }
}

fn long_review(app: &mut App, lines: usize, marker: &str) {
    let body = (0..lines)
        .map(|index| format!("review line {index:03}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.receive_event_for_test(WorkerEvent::ReviewProgress(AgentEvent::AssistantDelta {
        text: format!("{body}\n{marker}\n"),
        refusal: false,
    }));
}

fn review_wheel(kind: MouseEventKind) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: 50,
        row: 10,
        modifiers: KeyModifiers::NONE,
    })
}

#[test]
fn plan_review_routing_completion_cancel_and_scrolling() {
    {
        let (mut app, requests, _events) = plan_app();
        app.workflow = ready_plan_workflow();
        app.input_queue
            .enqueue(PendingKind::Steering, "pending".into(), None, false)
            .unwrap();
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Ready,
        ))));
        app.handle(key(KeyCode::Char('4')), &mut false);
        app.handle(key(KeyCode::Enter), &mut false);
        assert!(requests.try_recv().is_err() && app.plan_panel.is_some());
        assert!(matches!(app.state, State::Ready));

        let (mut review, req_rev, _) = plan_app();
        review.workflow = ready_plan_workflow();
        start_review(&mut review, &req_rev);
        review.usage = Some(pp_core::context::ContextUsage {
            estimated_tokens: 100,
            context_window: Some(40000),
            auto_compact: false,
            threshold_percent: 80,
            last_request: None,
            session_output_tokens: 0,
        });
        review.receive_event_for_test(WorkerEvent::ReviewProgress(AgentEvent::AssistantDelta {
            text: "review finding".into(),
            refusal: false,
        }));
        review.receive_event_for_test(WorkerEvent::ReviewProgress(AgentEvent::ContextUpdated {
            usage: pp_core::context::ContextUsage {
                estimated_tokens: 999,
                context_window: Some(40000),
                auto_compact: false,
                threshold_percent: 80,
                last_request: None,
                session_output_tokens: 0,
            },
        }));
        assert_eq!(review.usage.as_ref().unwrap().estimated_tokens, 100);
        assert!(draw(&mut review, 100, 30).contains("review finding"));

        review.input.insert_str("draft during review");
        review.handle(key(KeyCode::Enter), &mut false);
        assert_eq!(review.input.lines(), &["draft during review"]);
        assert!(req_rev.try_recv().is_err());
        review.handle(
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
            &mut false,
        );
        assert_eq!(review.input.lines(), &["draft during review"]);
        assert!(req_rev.try_recv().is_err());
        review.input = editor();
        review.input.insert_str("/plan");
        review.handle(key(KeyCode::Enter), &mut false);
        assert_eq!(review.input.lines(), &["/plan"]);
        assert!(req_rev.try_recv().is_err() && matches!(review.state, State::ReviewingPlan));
    }

    {
        let (mut app, requests, _events) = plan_app();
        app.workflow = ready_plan_workflow();
        app.workflow.review = Some(PlanReviewReport {
            plan_id: "0123456789abcdef0123456789abcdef".into(),
            revision: 2,
            model_reference: "provider/model".into(),
            content: "old findings".into(),
        });
        let WorkerCommand::ReviewPlan { cancellation, .. } = start_review(&mut app, &requests)
        else {
            panic!("expected ReviewPlan command");
        };
        app.handle(key(KeyCode::Esc), &mut false);
        assert!(cancellation.is_cancelled() && matches!(app.state, State::ReviewingPlan));
        app.input_queue
            .enqueue(PendingKind::Steering, "late".into(), None, false)
            .unwrap();
        app.resume_queue();
        assert!(requests.try_recv().is_err());

        app.receive_event_for_test(WorkerEvent::PlanReviewed(Err(anyhow::anyhow!(
            "review cancelled"
        ))));
        assert!(app.review_transcript.is_none() && matches!(app.state, State::ReadingPlan));
        assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadPlan)));
        assert_eq!(app.workflow.mode, WorkMode::Plan);
        assert_eq!(
            app.workflow
                .review
                .as_ref()
                .map(|report| report.content.as_str()),
            Some("old findings")
        );
        app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
            PlanStatus::Ready,
        ))));
        assert!(
            app.plan_panel
                .as_ref()
                .expect("menu panel")
                .action_enabled(1)
        );
        assert!(requests.try_recv().is_err());

        let (mut ok_app, ok_req, _) = plan_app();
        ok_app.workflow = ready_plan_workflow();
        start_review(&mut ok_app, &ok_req);
        ok_app.receive_event_for_test(WorkerEvent::PlanReviewed(Ok(PlanReviewReport {
            plan_id: "0123456789abcdef0123456789abcdef".into(),
            revision: 2,
            model_reference: "provider/model-x".into(),
            content: "final findings".into(),
        })));
        assert!(matches!(ok_req.try_recv(), Ok(WorkerCommand::ReadPlan)));
        assert_eq!(
            ok_app
                .workflow
                .review
                .as_ref()
                .map(|report| report.content.as_str()),
            Some("final findings")
        );
        let mut review = plan_review_fixture(PlanStatus::Ready);
        review.document.revision = 3;
        review.document.content = "- [ ] step one\n- [ ] step two\n- [ ] step three\n".into();
        review.token.revision = 3;
        review.report = Some(PlanReviewReport {
            plan_id: review.document.id.clone(),
            revision: 2,
            model_reference: "provider/model-x".into(),
            content: "stale findings".into(),
        });
        ok_app.receive_event_for_test(WorkerEvent::PlanRead(Ok(review)));
        assert!(
            ok_app
                .plan_panel
                .as_ref()
                .expect("report panel")
                .on_review_body()
        );
        let screen = draw(&mut ok_app, 100, 30);
        assert!(
            screen.contains("outdated") && screen.contains("stale findings"),
            "{screen}"
        );
        assert!(ok_req.try_recv().is_err());

        let (mut fail_app, fail_req, _) = plan_app();
        fail_app.workflow = ready_plan_workflow();
        start_review(&mut fail_app, &fail_req);
        fail_app.receive_event_for_test(WorkerEvent::PlanReviewed(Ok(PlanReviewReport {
            plan_id: "0123456789abcdef0123456789abcdef".into(),
            revision: 2,
            model_reference: "provider/model-x".into(),
            content: "final findings".into(),
        })));
        assert!(matches!(fail_req.try_recv(), Ok(WorkerCommand::ReadPlan)));
        fail_app.receive_event_for_test(WorkerEvent::PlanRead(Err(anyhow::anyhow!(
            "plan file vanished"
        ))));
        let panel = fail_app.plan_panel.as_ref().expect("degraded panel");
        assert!(panel.token().is_none());
        for index in 0..4 {
            assert!(!panel.action_enabled(index));
        }
        let screen = draw(&mut fail_app, 100, 30);
        assert!(screen.contains("final findings") && screen.contains("plan file vanished"));
        assert!(fail_req.try_recv().is_err());
    }

    {
        let (mut app, requests, _events) = plan_app();
        app.workflow = ready_plan_workflow();
        let WorkerCommand::ReviewPlan { cancellation, .. } = start_review(&mut app, &requests)
        else {
            panic!("expected ReviewPlan command");
        };
        app.conversation.begin_browse();
        app.conversation.scroll_up(5);
        assert!(!app.conversation.following());
        long_review(&mut app, 80, "review-tail-before-browse");
        draw(&mut app, 100, 30);
        app.input.insert_str("preserved reviewer draft");

        app.handle(key(KeyCode::PageUp), &mut false);
        assert!(app.history_browser && !app.active_transcript().following());
        let screen = draw(&mut app, 100, 30);
        assert!(!screen.contains("review-tail-before-browse"), "{screen}");

        app.handle(key(KeyCode::Esc), &mut false);
        assert!(!app.history_browser && app.active_transcript().following());
        assert!(!app.conversation.following() && !cancellation.is_cancelled());
        assert!(matches!(app.state, State::ReviewingPlan));
        app.receive_event_for_test(WorkerEvent::ReviewProgress(AgentEvent::AssistantDelta {
            text: "review-tail-after-esc\n".into(),
            refusal: false,
        }));
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("review-tail-after-esc"), "{screen}");
        assert_eq!(app.input.lines(), &["preserved reviewer draft"]);

        app.handle(key(KeyCode::Esc), &mut false);
        assert!(cancellation.is_cancelled() && matches!(app.state, State::ReviewingPlan));
        assert_eq!(
            app.secondary_status.as_deref(),
            Some("Cancelling review...")
        );

        let (mut wheel, req_wheel, _) = plan_app();
        wheel.workflow = ready_plan_workflow();
        start_review(&mut wheel, &req_wheel);
        wheel.conversation.begin_browse();
        wheel.conversation.scroll_up(5);
        long_review(&mut wheel, 80, "review-tail-before-wheel");
        draw(&mut wheel, 100, 30);
        for _ in 0..3 {
            wheel.handle(review_wheel(MouseEventKind::ScrollUp), &mut false);
        }
        assert!(wheel.history_browser && !wheel.active_transcript().following());
        wheel.handle(review_wheel(MouseEventKind::ScrollDown), &mut false);
        assert!(wheel.history_browser);
        for _ in 0..40 {
            wheel.handle(review_wheel(MouseEventKind::ScrollDown), &mut false);
        }
        assert!(wheel.active_transcript().following() && !wheel.history_browser);
        assert!(!wheel.conversation.following());
    }
}
