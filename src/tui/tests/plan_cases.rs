use super::*;

#[test]
fn plan_prompt_waits_for_success_then_sends_once_without_opening_review() {
    for workflow in [WorkflowState::default(), ready_plan_workflow()] {
        let (mut app, requests, _events) = plan_app();
        app.workflow = workflow;
        app.input.insert_str("/plan 调研登录流程");
        app.submit(PendingKind::Steering);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::EnterPlan { .. })
        ));
        assert!(requests.try_recv().is_err());
        assert!(matches!(app.state, State::ChangingPlan));
        assert!(app.input.is_empty());
        assert_eq!(app.input_history.last().unwrap(), "/plan 调研登录流程");
        // 切换期间新输入的草稿不受发送影响。
        app.input.insert_str("新草稿");
        app.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(ready_plan_workflow())));
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::Run { prompt, .. })
                if prompt == "调研登录流程")
        );
        assert!(requests.try_recv().is_err());
        assert!(matches!(app.state, State::Running));
        assert!(app.plan_panel.is_none());
        assert!(app.plan_after_enter.is_none());
        assert_eq!(app.input.lines(), &["新草稿"]);
    }
}

#[test]
fn plan_prompt_failure_never_sends_or_leaks_into_later_transition() {
    for draft in ["", "新草稿"] {
        let (mut app, requests, _events) = plan_app();
        app.input.insert_str("/plan research");
        app.submit(PendingKind::Steering);
        assert!(matches!(
            requests.try_recv(),
            Ok(WorkerCommand::EnterPlan { .. })
        ));
        app.input.insert_str(draft);
        app.receive_event_for_test(WorkerEvent::WorkflowChanged(Err(anyhow::anyhow!(
            "清理失败"
        ))));
        assert!(requests.try_recv().is_err());
        assert!(app.plan_retry_enter);
        assert!(app.plan_after_enter.is_none());
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
}

#[test]
fn plan_prompt_with_pending_input_preserves_draft() {
    let (mut app, requests, _events) = plan_app();
    app.input_queue
        .enqueue(PendingKind::FollowUp, "pending".into(), None, false)
        .unwrap();
    app.input.insert_str("/plan research");
    app.submit(PendingKind::Steering);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.input.lines(), &["/plan research"]);
    assert!(app.plan_after_enter.is_none());
}

#[test]
fn plan_commands_are_busy_refused_while_running() {
    let (mut app, requests, _events) = plan_app();
    app.state = State::Running;
    // Plan 模式下切换与任务提交都必须被忙碌拒绝；旧子命令由解析层拒绝。
    app.workflow = WorkflowState {
        mode: WorkMode::Plan,
        plan: None,
        ..Default::default()
    };
    for command in ["/plan", "/plan research"] {
        app.input.insert_str(command);
        app.submit(PendingKind::Steering);
        assert_eq!(app.input.lines(), &[command]);
        app.input = editor();
    }
    assert!(requests.try_recv().is_err());
    assert_notification(&mut app, Color::Rgb(224, 108, 117));
}

#[test]
fn plan_ready_refuses_auto_dispatch_but_ctrl_r_explicitly_resumes() {
    let (mut app, requests, _events) = plan_app();
    app.workflow = ready_plan_workflow();
    app.queue_paused = false;
    // 模拟 pending：直接通过输入队列入队。
    app.input_queue
        .enqueue(PendingKind::Steering, "follow up".to_string(), None, false)
        .unwrap();
    assert!(!app.dispatch_next(false));
    assert!(requests.try_recv().is_err());
    app.resume_queue();
    assert!(matches!(requests.try_recv(), Ok(WorkerCommand::Run { .. })));
}

#[test]
fn read_plan_opens_panel_and_execute_keep_sends_token() {
    let (mut app, requests, _events) = plan_app();
    app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
        PlanStatus::Ready,
    ))));
    assert!(app.plan_panel.is_some());
    assert_eq!(app.plan_progress, (0, 2));
    // 默认选中第一项；向下选中保留上下文执行并携带 token。
    app.handle(key(KeyCode::Down), &mut false);
    app.handle(key(KeyCode::Enter), &mut false);
    let command = requests.try_recv().unwrap();
    let WorkerCommand::ApprovePlan { token, context, .. } = command else {
        panic!("expected ApprovePlan command");
    };
    assert_eq!(token.revision, 2);
    assert_eq!(context, ExecutionContext::Keep);
    assert!(matches!(app.state, State::PreparingExecution));
    assert_eq!(
        app.secondary_status.as_deref(),
        Some("Preparing execution...")
    );
}

#[test]
fn plan_focus_highlights_only_the_active_frame_and_preserves_selection() {
    let (mut app, requests, _events) = plan_app();
    app.receive_event_for_test(WorkerEvent::PlanRead(Ok(plan_review_fixture(
        PlanStatus::Ready,
    ))));
    let muted = transcript_style::MUTED;
    let accent = transcript_style::ACCENT;
    for colors in [
        [muted, accent, muted],
        [muted, muted, statusline::ACCENT],
        [accent, muted, muted],
        [muted, accent, muted],
    ] {
        let buffer = draw_buffer(&mut app, 100, 30);
        let actions_y = (0..30).find(|&y| buffer[(0, y)].symbol() == "╭").unwrap();
        for (y, color) in [0, actions_y, 26].into_iter().zip(colors) {
            assert_eq!(buffer[(99, y)].fg, color, "边框所在行：{y}");
        }
        let rows: Vec<String> = (actions_y + 1..actions_y + 7)
            .map(|y| (1..99).map(|x| buffer[(x, y)].symbol()).collect())
            .collect();
        assert!(rows[0].starts_with("❯ Execute in a new session"));
        assert_eq!(rows.iter().filter(|row| row.contains('❯')).count(), 1);
        for (index, row) in rows.iter().enumerate() {
            assert!(!row.contains(&format!("{}. ", index + 1)), "{row}");
        }
        let hint: String = (1..99)
            .map(|x| buffer[(x, actions_y + 7)].symbol())
            .collect();
        assert!(!hint.contains("1–6 select"), "{hint}");
        app.handle(key(KeyCode::Tab), &mut false);
    }
    assert!(requests.try_recv().is_err());
}

#[test]
fn plan_ready_paste_keeps_output_and_discusses_from_the_input() {
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
    assert!(app.plan_panel.is_none());
    assert!(app.input.is_empty());
}

#[test]
fn execution_prepared_installs_cleanup_and_acks_before_running() {
    let (mut app, _requests, events) = plan_app();
    app.state = State::PreparingExecution;
    let (reply, ack) = mpsc::sync_channel(1);
    let registry = crate::mcp::Registry::connect(
        &Default::default(),
        Path::new("."),
        &CancellationToken::new(),
    )
    .unwrap();
    let cleanup = registry.cleanup().unwrap();
    let workflow = WorkflowState {
        mode: WorkMode::Execute,
        plan: Some(crate::plan::PlanDocument {
            status: PlanStatus::Executed,
            ..ready_plan_workflow().plan.expect("plan")
        }),
        ..Default::default()
    };
    events
        .send(WorkerEvent::ExecutionPrepared {
            workflow: workflow.clone(),
            session_id: Some("next".into()),
            replay: None,
            cleanup,
            todo_snapshot: None,
            reply,
        })
        .unwrap();
    app.receive();
    assert!(ack.try_recv().is_ok());
    assert!(app.mcp_cleanup.is_some());
    assert_eq!(app.workflow, workflow);
    assert_eq!(app.session_id.as_deref(), Some("next"));
    assert!(matches!(app.state, State::Running));
    assert!(!app.show_welcome);
}

#[test]
fn awaiting_review_completion_requests_plan_read() {
    let (mut app, requests, events) = plan_app();
    events
        .send(WorkerEvent::completed(
            Instant::now(),
            Ok(RunOutcome::AwaitingPlanReview),
        ))
        .unwrap();
    app.receive();
    assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadPlan)));
    assert!(matches!(app.state, State::ReadingPlan));
}

#[test]
fn progress_view_during_running_is_readonly_and_cached() {
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
    // Esc 只关闭面板，不取消任务。
    app.handle(key(KeyCode::Esc), &mut false);
    assert!(app.plan_panel.is_none());
    assert!(matches!(app.state, State::Running));
}

#[test]
fn plan_progress_event_updates_cache_and_panel_without_transcript() {
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

#[test]
fn plan_toggle_enters_standby_and_leaves_without_confirm() {
    let (mut app, requests, _events) = plan_app();
    // Execute：裸 /plan 进入 Plan 待命（Plan+None，不打开审阅）。
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

    // 空待命可直接退出：裸 /plan 直接发送 LeavePlan，无确认框。
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

#[test]
fn plan_toggle_confirms_discarding_unsaved_revision_zero_draft() {
    let (mut app, requests, _events) = plan_app();
    let mut plan = crate::plan::PlanDocument::new().unwrap();
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
    assert!(matches!(app.state, State::Ready));
    assert_eq!(app.workflow.mode, WorkMode::Plan);
    assert!(app.workflow.plan.is_some());
    let screen = draw(&mut app, 100, 30).replace(' ', "");
    assert!(screen.contains("Discardunsavedchanges?"), "{screen}");
    assert!(screen.contains("Cancel,keepplanning"));
    assert!(screen.contains("Discardchangesandexit"));
    assert!(app.needs_alternate());
    app.input.insert_str("保留输入");
    app.handle(key(KeyCode::Enter), &mut false);
    assert!(app.plan_leave_confirmation.is_none());
    assert!(requests.try_recv().is_err());
    assert_eq!(app.input.lines(), &["保留输入"]);
    assert_eq!(app.workflow.plan.as_ref().unwrap().id, id);

    app.apply_plan_command(PlanCommand::Toggle);
    app.handle(Event::Paste("不能插入".into()), &mut false);
    app.handle(function_key('p'), &mut false);
    app.handle(key(KeyCode::F(1)), &mut false);
    assert_eq!(app.input.lines(), &["保留输入"]);
    assert!(requests.try_recv().is_err());
    app.handle(key(KeyCode::Esc), &mut false);
    assert!(app.plan_leave_confirmation.is_none());
    assert_eq!(app.workflow.mode, WorkMode::Plan);

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
    assert_eq!(app.input.lines(), &["保留输入"]);
}

#[test]
fn plan_discard_confirmation_rejects_stale_binding() {
    let (mut app, requests, _events) = plan_app();
    let mut plan = crate::plan::PlanDocument::new().unwrap();
    plan.origin_prompt = "first task".into();
    app.workflow = WorkflowState {
        mode: WorkMode::Plan,
        plan: Some(plan),
        ..Default::default()
    };
    app.apply_plan_command(PlanCommand::Toggle);
    app.workflow = ready_plan_workflow();
    app.handle(key(KeyCode::Down), &mut false);
    app.handle(key(KeyCode::Enter), &mut false);
    assert!(requests.try_recv().is_err());
    assert!(app.plan_leave_confirmation.is_none());
    assert_eq!(app.workflow, ready_plan_workflow());
}

#[test]
fn plan_list_new_plan_confirms_discard_and_reenters_standby() {
    let (mut app, requests, _events) = plan_app();
    let mut plan = crate::plan::PlanDocument::new().unwrap();
    plan.origin_prompt = "first task".into();
    app.workflow = WorkflowState {
        mode: WorkMode::Plan,
        plan: Some(plan),
        ..Default::default()
    };
    app.plan_list_panel = Some(plan_list::Panel::new());
    app.plan_list_new_plan();
    assert!(requests.try_recv().is_err());
    app.handle(key(KeyCode::Esc), &mut false);
    assert!(app.plan_list_panel.is_some());
    app.plan_list_new_plan();
    app.handle(key(KeyCode::Down), &mut false);
    app.handle(key(KeyCode::Enter), &mut false);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::LeavePlan {
            discard_plan_id: Some(_),
            ..
        })
    ));
    assert!(app.plan_list_panel.is_none());
    app.receive_event_for_test(WorkerEvent::WorkflowChanged(Ok(WorkflowState::default())));
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::EnterPlan { .. })
    ));
    assert!(requests.try_recv().is_err());
}

#[test]
fn plan_leave_failure_keeps_leave_direction_on_retry() {
    let (mut app, requests, _events) = plan_app();
    app.workflow = ready_plan_workflow();
    app.input.insert_str("/plan");
    app.submit(PendingKind::Steering);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::LeavePlan { .. })
    ));
    app.receive_event_for_test(WorkerEvent::WorkflowChanged(Err(anyhow::anyhow!(
        "退出失败"
    ))));
    // 退出失败不设置进入重试标记；下一次裸 /plan 仍然离开。
    assert!(!app.plan_retry_enter);
    app.input.insert_str("/plan");
    app.submit(PendingKind::Steering);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::LeavePlan { .. })
    ));
}

#[test]
fn plan_old_subcommands_are_rejected_locally_in_tui() {
    let (mut app, requests, _events) = plan_app();
    for command in ["/plan new", "/plan off"] {
        app.input.insert_str(command);
        app.submit(PendingKind::Steering);
        // 只报本地错误：不切换模式、不发送模型请求、输入原样保留。
        assert!(requests.try_recv().is_err());
        assert_eq!(app.input.lines(), &[command]);
        assert_eq!(app.workflow.mode, WorkMode::Execute);
        assert!(app.workflow.plan.is_none());
        assert_notification(&mut app, Color::Rgb(224, 108, 117));
        let screen = draw(&mut app, 120, 30);
        assert!(screen.contains("Use /plan to toggle; manage plans with /plan-list."));
        app.input = editor();
        app.conversation = app.conversation.fresh();
    }
}

#[test]
fn review_action_refuses_pending_input() {
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
    assert!(requests.try_recv().is_err());
    assert!(app.plan_panel.is_some());
    assert!(matches!(app.state, State::Ready));
}

#[test]
fn review_page_routes_progress_only_to_review_transcript_and_keeps_draft() {
    let (mut app, requests, _events) = plan_app();
    app.workflow = ready_plan_workflow();
    start_review(&mut app, &requests);
    app.usage = Some(crate::context::ContextUsage {
        estimated_tokens: 100,
        context_window: Some(40000),
        auto_compact: false,
        threshold_percent: 80,
        last_request: None,
    });
    app.receive_event_for_test(WorkerEvent::ReviewProgress(AgentEvent::AssistantDelta {
        text: "review finding".into(),
        refusal: false,
    }));
    app.receive_event_for_test(WorkerEvent::ReviewProgress(AgentEvent::ContextUpdated {
        usage: crate::context::ContextUsage {
            estimated_tokens: 999,
            context_window: Some(40000),
            auto_compact: false,
            threshold_percent: 80,
            last_request: None,
        },
    }));
    // 主用量与派发不受审查影响；屏幕只显示审查流。
    assert_eq!(app.usage.as_ref().unwrap().estimated_tokens, 100);
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("review finding"));

    // steering/followup/命令提交全部被拒，草稿不丢失。
    app.input.insert_str("draft during review");
    app.handle(key(KeyCode::Enter), &mut false);
    assert_eq!(app.input.lines(), &["draft during review"]);
    assert!(requests.try_recv().is_err());
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)),
        &mut false,
    );
    assert_eq!(app.input.lines(), &["draft during review"]);
    assert!(requests.try_recv().is_err());
    app.input = editor();
    app.input.insert_str("/plan");
    app.handle(key(KeyCode::Enter), &mut false);
    assert_eq!(app.input.lines(), &["/plan"]);
    assert!(requests.try_recv().is_err());
    assert!(matches!(app.state, State::ReviewingPlan));
}

#[test]
fn esc_cancels_reviewer_and_returns_to_menu_on_controlled_finish() {
    let (mut app, requests, _events) = plan_app();
    app.workflow = ready_plan_workflow();
    app.workflow.review = Some(PlanReviewReport {
        plan_id: "0123456789abcdef0123456789abcdef".into(),
        revision: 2,
        model_reference: "provider/model".into(),
        content: "old findings".into(),
    });
    let WorkerCommand::ReviewPlan { cancellation, .. } = start_review(&mut app, &requests) else {
        panic!("expected ReviewPlan command");
    };
    app.handle(key(KeyCode::Esc), &mut false);
    // 收尾前保持审查态；迟到的 pending 不派发。
    assert!(cancellation.is_cancelled());
    assert!(matches!(app.state, State::ReviewingPlan));
    app.input_queue
        .enqueue(PendingKind::Steering, "late".into(), None, false)
        .unwrap();
    app.resume_queue();
    assert!(requests.try_recv().is_err());

    app.receive_event_for_test(WorkerEvent::PlanReviewed(Err(anyhow::anyhow!(
        "review cancelled"
    ))));
    // 回六项菜单的读取中；旧完整报告与 Plan 模式保留。
    assert!(app.review_transcript.is_none());
    assert!(matches!(app.state, State::ReadingPlan));
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
    let panel = app.plan_panel.as_ref().expect("menu panel");
    assert!(panel.action_enabled(1));
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn review_success_focuses_report_and_never_autostarts_execution() {
    let (mut app, requests, _events) = plan_app();
    app.workflow = ready_plan_workflow();
    start_review(&mut app, &requests);
    app.receive_event_for_test(WorkerEvent::PlanReviewed(Ok(PlanReviewReport {
        plan_id: "0123456789abcdef0123456789abcdef".into(),
        revision: 2,
        model_reference: "provider/model-x".into(),
        content: "final findings".into(),
    })));
    assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadPlan)));
    assert_eq!(
        app.workflow
            .review
            .as_ref()
            .map(|report| report.content.as_str()),
        Some("final findings")
    );

    // 外部编辑期间产生新修订：刷新返回 r3 与针对 r2 的旧报告。
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
    app.receive_event_for_test(WorkerEvent::PlanRead(Ok(review)));
    let panel = app.plan_panel.as_ref().expect("report panel");
    assert!(panel.on_review_body());
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("outdated"));
    assert!(screen.contains("stale findings"), "{screen}");
    // 报告只是辅助材料：不产生实施请求，也不重新进入审查。
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn failed_read_after_review_shows_report_without_execution_token() {
    let (mut app, requests, _events) = plan_app();
    app.workflow = ready_plan_workflow();
    start_review(&mut app, &requests);
    app.receive_event_for_test(WorkerEvent::PlanReviewed(Ok(PlanReviewReport {
        plan_id: "0123456789abcdef0123456789abcdef".into(),
        revision: 2,
        model_reference: "provider/model-x".into(),
        content: "final findings".into(),
    })));
    assert!(matches!(requests.try_recv(), Ok(WorkerCommand::ReadPlan)));
    app.receive_event_for_test(WorkerEvent::PlanRead(Err(anyhow::anyhow!(
        "plan file vanished"
    ))));
    // 报告展示但旧执行 token 失效：只读面板禁用全部动作。
    let panel = app.plan_panel.as_ref().expect("degraded panel");
    assert!(panel.token().is_none());
    for index in 0..4 {
        assert!(!panel.action_enabled(index));
    }
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("final findings"));
    assert!(screen.contains("plan file vanished"));
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
}
