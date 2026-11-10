use super::*;

fn completion_states() -> impl Iterator<Item = State> {
    [
        State::ManagingMcp,
        State::Ready,
        State::Running,
        State::RunningShell,
        State::RunningTools,
        State::Cancelling,
        State::Compacting,
        State::Resetting,
        State::SwitchingModel,
        State::SavingModelSettings,
        State::SavingThinkDefault,
        State::Resuming,
        State::ReadingTree,
        State::NavigatingTree,
        State::ReadingChanges,
        State::UndoingFile,
        State::ListingPlans,
        State::ReadingListedPlan,
        State::OpeningListedPlan,
        State::RenamingPlan,
        State::DeletingPlan,
        State::Unavailable,
        State::ChangingPlan,
        State::ReadingPlan,
        State::PreparingExecution,
        State::ReviewingPlan,
    ]
    .into_iter()
}

#[test]
fn command_menu_lists_debug_only_commands_in_debug_builds() {
    let mut app = app();
    let mut dirty = false;
    app.handle(Event::Paste("/test-ui".into()), &mut dirty);
    if cfg!(debug_assertions) {
        // 补全直接枚举已裁剪的 COMMANDS：MenuItem 指向真实下标。
        let values = menu_values(&app);
        assert_eq!(values.len(), 1, "{values:?}");
        let MenuAction::Command(index) = values[0] else {
            panic!("unexpected menu action: {values:?}");
        };
        assert_eq!(COMMANDS[index].name, "/test-ui");
        assert_eq!(app.menu.as_ref().unwrap().items()[0].label, "/test-ui");
    } else {
        assert!(app.menu.is_none());
        assert_eq!(app.input.lines(), &["/test-ui"]);
    }
}

#[cfg(debug_assertions)]
#[test]
fn command_menu_hides_fuzzy_matches_when_a_prefix_matches() {
    let mut app = app();
    let mut dirty = false;
    app.handle(Event::Paste("/test-too".into()), &mut dirty);
    let menu = app.menu.as_ref().unwrap();
    assert_eq!(menu.items().len(), 1);
    assert_eq!(menu.items()[0].label, "/test-tools");
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &["/test-tools"]);
}

#[test]
fn command_menu_matches_fuzzy_case_insensitively_and_prioritizes_prefixes() {
    let mut app = app();
    let mut dirty = false;
    for (query, first, completed) in [
        ("/roles", "/Roles", "/Roles"),
        ("/MDL", "/model", "/model"),
        ("/PLS", "/plan-list", "/plan-list"),
        ("/LIST", "/plan-list", "/plan-list"),
        ("/PLAN", "/plan", "/PLAN"),
        ("/RE", "/resume", "/resume"),
    ] {
        app.input = editor();
        app.handle(Event::Paste(query.into()), &mut dirty);
        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.items()[0].label, first, "{query}");
        assert_eq!(menu.selected_index(), Some(0));
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &[completed]);
    }
    for query in ["/LDM", "/zzzz", "/MDL argument"] {
        app.input = editor();
        app.handle(Event::Paste(query.into()), &mut dirty);
        assert!(app.menu.is_none(), "{query}");
        assert_eq!(app.input.lines(), &[query]);
    }
}

#[test]
fn model_space_menu_renders_above_input_and_preserves_selection_on_resize() {
    let mut app = app();
    app.state = State::Ready;
    app.choices = (0..25)
        .map(|index| ModelChoice {
            reference: format!("fixture/model-{index}"),
            model: format!("vendor-{index}"),
            protocol: pp_config::models::Protocol::Response,
            thinking: pp_config::models::ModelThinking::default(),
            context_window: None,
        })
        .collect();
    app.current_model = Some(app.choices[2].clone());
    let (sender, requests) = mpsc::channel();
    app.commands = Some(sender);
    let (events, receiver) = mpsc::channel();
    app.events = Some(receiver);
    let mut dirty = false;
    app.input.insert_str("/model");
    app.refresh_candidates(true);
    let model_command = commands::COMMANDS
        .iter()
        .position(|spec| spec.name == "/model")
        .unwrap();
    assert_eq!(menu_values(&app), vec![MenuAction::Command(model_command)]);
    app.handle(key(KeyCode::Char(' ')), &mut dirty);
    let screen = draw(&mut app, 100, 30);
    assert!(screen.find(" ").unwrap() < screen.find("fixture/model-0").unwrap());
    assert!(!screen.contains("❯ /model"));
    assert!(screen.contains("❯ fixture/model-2"));
    assert!(
        screen.replace(' ', "").contains("fixture/model-2"),
        "{screen}"
    );
    assert!(screen.lines().last().unwrap().contains("fixture/model-2"));
    assert!(!screen.contains("fixture/vendor-2"), "{screen}");
    assert!(!screen.contains('┌'));
    for width in [100, 180, 60] {
        let buffer = draw_buffer(&mut app, width, 30);
        let y = (0..30)
            .find(|y| {
                (0..width)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains(" favorite ·  default ·  current")
            })
            .unwrap();
        for (icon, color) in [
            ("", Color::Rgb(254, 188, 56)),
            ("", Color::Rgb(187, 154, 247)),
            ("", Color::Rgb(158, 206, 106)),
        ] {
            let x = (0..width)
                .find(|x| buffer[(*x, y)].symbol() == icon)
                .unwrap();
            assert_eq!(buffer[(x, y)].fg, color);
        }
    }
    draw(&mut app, 100, 30);
    assert_eq!(app.model_panel.as_ref().unwrap().height(30), 15);
    app.handle(key(KeyCode::PageDown), &mut dirty);
    assert_eq!(active_menu(&app).selected_index(), Some(13));
    for (width, height) in [(20, 6), (1, 1), (100, 30)] {
        draw(&mut app, width, height);
        assert_eq!(app.input.lines(), &[""]);
        assert_eq!(active_menu(&app).selected_index(), Some(13));
    }
    app.handle(key(KeyCode::End), &mut dirty);
    assert!(draw(&mut app, 100, 30).contains("❯ fixture/model-24"));
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::SwitchModel(reference)) if reference == "fixture/model-24")
    );
    assert_eq!(
        app.current_model.as_ref().unwrap().reference,
        "fixture/model-2"
    );
    assert!(app.menu.is_none());
    app.input.insert_str("后续草稿");
    events
        .send(WorkerEvent::ModelSwitched(Ok((
            app.choices[24].clone(),
            app.think_level,
        ))))
        .unwrap();
    app.receive();
    assert_eq!(
        app.current_model.as_ref().unwrap().reference,
        "fixture/model-24"
    );
    assert_eq!(app.input.lines(), &["后续草稿"]);
}

#[test]
fn model_panel_search_is_independent_single_line_and_never_submits_tasks() {
    let (mut app, requests, _events) = plan_app();
    app.choices = favorite_model_choices();
    let mut dirty = false;
    app.handle(Event::Paste("/model".into()), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert_eq!(app.input.lines(), &[""]);
    assert_eq!(model_query(&app), "");
    app.handle(
        Event::Paste("模型🙂\nsecond\r\nthird\rfourth".into()),
        &mut dirty,
    );
    assert_eq!(model_query(&app), "模型🙂 second third fourth");
    app.handle(control('a'), &mut dirty);
    app.handle(control('k'), &mut dirty);
    app.handle(control('j'), &mut dirty);
    app.handle(key(KeyCode::BackTab), &mut dirty);
    assert_eq!(model_query(&app), "");
    let query = "模型🙂".repeat(30);
    app.handle(Event::Paste(query.clone()), &mut dirty);
    let mut terminal = Terminal::new(TestBackend::new(20, 12)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, &StatusContext::default()))
        .unwrap();
    let cursor = terminal.backend().cursor_position();
    assert!(cursor.x < 20 && cursor.y < 8);
    assert_eq!(model_query(&app), query);
    app.handle(control('a'), &mut dirty);
    app.handle(control('k'), &mut dirty);
    app.handle(Event::Paste("aMm".into()), &mut dirty);
    assert_eq!(menu_values(&app), [MenuAction::Model(2)]);
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(model_query(&app), "gateway/team/Gamma");
    assert_eq!(app.input.lines(), &[""]);
    assert!(requests.try_recv().is_err());
    assert!(app.input_history.is_empty());
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.model_panel.is_none());
    assert_eq!(app.input.lines(), &[""]);
}

#[test]
fn model_panel_caps_height_and_routes_wheel_without_scrolling_conversation() {
    let (mut app, requests, _events) = plan_app();
    app.show_welcome = false;
    app.choices = (0..25)
        .map(|index| ModelChoice {
            reference: format!("fixture/model-{index:02}"),
            model: format!("vendor-{index:02}"),
            protocol: pp_config::models::Protocol::Response,
            thinking: pp_config::models::ModelThinking::default(),
            context_window: None,
        })
        .collect();
    for index in 0..40 {
        app.append("Assistant", &format!("history-{index:02}"));
    }
    let mut dirty = false;
    app.handle(Event::Paste("/model ".into()), &mut dirty);
    let before = draw_buffer(&mut app, 100, 30);
    assert_eq!(app.model_panel.as_ref().unwrap().height(30), 15);
    let model_rows = |buffer: &ratatui::buffer::Buffer| -> Vec<u16> {
        (0..30)
            .filter(|y| {
                (0..99)
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("fixture/model-")
            })
            .collect()
    };
    assert_eq!(model_rows(&before), (14..25).collect::<Vec<_>>());
    assert_eq!(before[(99, 14)].symbol(), "█");
    let wheel = |kind, row| {
        Event::Mouse(MouseEvent {
            kind,
            column: 50,
            row,
            modifiers: KeyModifiers::NONE,
        })
    };
    app.handle(wheel(MouseEventKind::ScrollDown, 15), &mut dirty);
    assert_eq!(active_menu(&app).selected_index(), Some(3));
    app.handle(key(KeyCode::End), &mut dirty);
    let end = draw_buffer(&mut app, 100, 30);
    assert_eq!(model_rows(&end).len(), 11);
    assert_eq!(end[(99, 24)].symbol(), "█");
    for y in (0..11).chain(26..30) {
        for x in 0..100 {
            assert_eq!(before[(x, y)], end[(x, y)]);
        }
    }
    app.handle(wheel(MouseEventKind::ScrollDown, 15), &mut dirty);
    assert_eq!(active_menu(&app).selected_index(), Some(24));
    app.handle(wheel(MouseEventKind::ScrollUp, 15), &mut dirty);
    assert_eq!(active_menu(&app).selected_index(), Some(21));
    app.handle(wheel(MouseEventKind::ScrollUp, 0), &mut dirty);
    let scrolled = draw_buffer(&mut app, 100, 30);
    assert_eq!(active_menu(&app).selected_index(), Some(21));
    assert!((0..11).any(|y| (0..100).any(|x| before[(x, y)] != scrolled[(x, y)])));
    app.handle(key(KeyCode::Home), &mut dirty);
    app.handle(wheel(MouseEventKind::ScrollUp, 15), &mut dirty);
    assert_eq!(active_menu(&app).selected_index(), Some(0));
    app.handle(Event::Paste("model-24".into()), &mut dirty);
    assert_eq!(app.model_panel.as_ref().unwrap().height(30), 15);
    let filtered = draw_buffer(&mut app, 100, 30);
    assert_eq!(filtered[(0, 12)].symbol(), before[(0, 12)].symbol());
    assert_eq!(model_rows(&filtered), [14]);
    app.handle(control('a'), &mut dirty);
    app.handle(control('k'), &mut dirty);
    app.handle(Event::Paste("missing".into()), &mut dirty);
    assert_eq!(app.model_panel.as_ref().unwrap().height(30), 15);
    assert!(
        draw(&mut app, 100, 30)
            .lines()
            .nth(14)
            .unwrap()
            .contains("No matching models")
    );
    app.handle(control('a'), &mut dirty);
    app.handle(control('k'), &mut dirty);
    assert_eq!(model_rows(&draw_buffer(&mut app, 100, 30)).len(), 11);
    app.handle(Event::Paste("model-24".into()), &mut dirty);
    for (width, height) in [(20, 6), (1, 1), (1, 8), (100, 30)] {
        draw(&mut app, width, height);
        assert_eq!(model_query(&app), "model-24");
        assert_eq!(active_menu(&app).items()[0].value, MenuAction::Model(24));
    }
    assert!(draw(&mut app, 100, 30).contains(" model-24"));
    assert!(requests.try_recv().is_err());
}

#[test]
fn model_favorites_commit_before_reorder_preserve_focus_and_reject_repeats() {
    let (mut app, requests, events) = plan_app();
    app.choices = favorite_model_choices();
    app.current_model = Some(app.choices[0].clone());
    app.default_model_reference = Some(app.choices[0].reference.clone());
    app.favorite_models = vec![
        app.choices[3].reference.clone(),
        app.choices[1].reference.clone(),
    ];
    let mut dirty = false;
    app.input.insert_str("/model ");
    app.refresh_candidates(true);
    assert_eq!(
        menu_values(&app),
        vec![
            MenuAction::Model(1),
            MenuAction::Model(3),
            MenuAction::Model(0),
            MenuAction::Model(2)
        ]
    );
    app.handle(key(KeyCode::End), &mut dirty);
    for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
        app.handle(
            Event::Key(KeyEvent {
                kind,
                ..KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)
            }),
            &mut dirty,
        );
    }
    assert!(requests.try_recv().is_err());
    app.handle(control('f'), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::ToggleFavoriteModel(reference)) if reference == app.choices[2].reference)
    );
    assert_eq!(app.favorite_models.len(), 2);
    assert_eq!(menu_values(&app).last(), Some(&MenuAction::Model(2)));
    for event in [control('f'), control('s'), key(KeyCode::Enter)] {
        app.handle(event, &mut dirty);
    }
    assert!(requests.try_recv().is_err());
    events
        .send(WorkerEvent::FavoriteModelsSaved(Ok(vec![
            app.choices[1].reference.clone(),
            app.choices[2].reference.clone(),
            app.choices[3].reference.clone(),
        ])))
        .unwrap();
    app.receive();
    assert_eq!(
        menu_values(&app),
        vec![
            MenuAction::Model(1),
            MenuAction::Model(2),
            MenuAction::Model(3),
            MenuAction::Model(0)
        ]
    );
    assert_eq!(active_menu(&app).selected_index(), Some(1));
    assert!(draw(&mut app, 130, 30).contains("❯ gateway/team/Gamma "));
    app.handle(control('f'), &mut dirty);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::ToggleFavoriteModel(_))
    ));
    events
        .send(WorkerEvent::FavoriteModelsSaved(Err(anyhow::anyhow!(
            "fixture save failure"
        ))))
        .unwrap();
    app.receive();
    assert_eq!(app.favorite_models.len(), 3);
    assert_eq!(active_menu(&app).selected_index(), Some(1));
    assert!(draw(&mut app, 130, 30).contains("fixture save failure"));
    app.handle(control('f'), &mut dirty);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::ToggleFavoriteModel(_))
    ));
    events
        .send(WorkerEvent::FavoriteModelsSaved(Ok(vec![
            app.choices[1].reference.clone(),
            app.choices[3].reference.clone(),
        ])))
        .unwrap();
    app.receive();
    assert_eq!(active_menu(&app).selected_index(), Some(3));
    assert_eq!(
        app.current_model.as_ref().unwrap().reference,
        app.choices[0].reference
    );
    assert_eq!(
        app.default_model_reference.as_deref(),
        Some(app.choices[0].reference.as_str())
    );
}

#[test]
fn model_search_matches_substrings_case_insensitively_and_handles_no_results() {
    let (mut app, requests, _events) = plan_app();
    app.choices = favorite_model_choices();
    app.favorite_models = vec![app.choices[2].reference.clone()];
    let mut dirty = false;
    for (query, expected) in [
        ("  WaY/TeAm  ", vec![2, 0, 1, 3]),
        ("ENDOR-gA", vec![2]),
        ("eTa", vec![1]),
        ("", vec![2, 0, 1, 3]),
        ("missing", vec![]),
    ] {
        app.close_model_panel();
        app.input = editor();
        app.handle(Event::Paste(format!("/model {query}")), &mut dirty);
        assert_eq!(app.model_panel.as_ref().unwrap().height(30), 8);
        assert_eq!(
            menu_values(&app),
            expected
                .into_iter()
                .map(MenuAction::Model)
                .collect::<Vec<_>>()
        );
    }
    assert!(draw(&mut app, 100, 30).contains("No matching models"));
    for event in [
        key(KeyCode::Enter),
        key(KeyCode::Tab),
        control('f'),
        control('s'),
    ] {
        app.handle(event, &mut dirty);
    }
    assert!(requests.try_recv().is_err());
    assert_eq!(app.input.lines(), &[""]);
    assert_eq!(model_query(&app), "missing");
    for _ in 0..7 {
        app.handle(key(KeyCode::Backspace), &mut dirty);
    }
    assert_eq!(menu_values(&app).len(), 4);
    app.handle(Event::Paste("aMm".into()), &mut dirty);
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &[""]);
    assert_eq!(model_query(&app), "gateway/team/Gamma");
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::SwitchModel(reference)) if reference == "gateway/team/Gamma")
    );
}

#[test]
fn model_menu_filters_completes_and_cancels_without_requests() {
    let mut app = app();
    app.state = State::Ready;
    app.choices = vec![ModelChoice {
        reference: "gateway/team/model with spaces".into(),
        model: "vendor-one".into(),
        protocol: pp_config::models::Protocol::Completion,
        thinking: pp_config::models::ModelThinking::default(),
        context_window: None,
    }];
    let mut dirty = false;
    app.handle(Event::Paste("/model vendor".into()), &mut dirty);
    assert_eq!(menu_values(&app), vec![MenuAction::Model(0)]);
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &[""]);
    assert_eq!(model_query(&app), "gateway/team/model with spaces");
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.model_panel.is_none());
    app.refresh_candidates(false);
    assert!(app.menu.is_none());
    app.handle(key(KeyCode::Backspace), &mut dirty);
    assert!(app.model_panel.is_none());
    app.input = editor();
    app.input.insert_str("/model missing");
    app.refresh_candidates(true);
    assert!(active_menu(&app).items().is_empty());
    assert!(draw(&mut app, 100, 30).contains("No matching models"));
    app.close_model_panel();
}

#[test]
fn completion_commands_stay_usable_in_busy_states() {
    for state in completion_states().filter(|state| !matches!(state, State::Ready)) {
        let (mut app, requests, _events) = plan_app();
        app.state = state;
        app.file_commands = review_files();
        let cancellation = CancellationToken::new();
        app.active_cancellation = Some(cancellation.clone());
        let mut dirty = false;
        for (query, expected) in [("/mo", "/model"), ("/rev", "/review")] {
            app.clear_input();
            app.handle(Event::Paste(query.into()), &mut dirty);
            assert_eq!(app.menu.as_ref().unwrap().items()[0].label, expected);
            app.handle(key(KeyCode::Tab), &mut dirty);
            assert_eq!(app.input.lines(), &[expected]);
            app.handle(key(KeyCode::Esc), &mut dirty);
            assert!(!cancellation.is_cancelled());
        }
        assert!(requests.try_recv().is_err());
        assert!(app.input_history.is_empty());
        assert!(!cancellation.is_cancelled());
        assert!(std::mem::discriminant(&app.state) == std::mem::discriminant(&state));
    }
}

#[test]
fn effort_completion_stays_usable_while_running_and_submits_without_state_change() {
    let (control, _binding) = model_control();
    let (mut app, requests, _events) = plan_app();
    app.state = State::Running;
    app.model_control = Some(control);
    // 夹具模型未写 thinking，允许全部档位；目录只用于本档位校验。
    app.choices = vec![all_levels_choice()];
    app.current_model = Some(app.choices[0].clone());
    let mut dirty = false;
    app.handle(Event::Paste("/effort h".into()), &mut dirty);
    assert_eq!(menu_values(&app), [MenuAction::Effort(ThinkLevel::High)]);
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &["/effort high"]);
    app.handle(key(KeyCode::Enter), &mut dirty);
    // 提交后只登记期望值：清空草稿但不改 State，也不发 worker 命令。
    assert_eq!(app.input.lines(), &[""]);
    assert!(matches!(app.state, State::Running));
    assert_eq!(app.pending_think, Some((0, ThinkLevel::High)));
    assert_eq!(app.input_history, ["/effort high"]);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));

    // 控制通道缺失时保留草稿并提示，不登记新目标。
    app.model_control = None;
    app.pending_think = None;
    app.input_history.clear();
    app.handle(Event::Paste("/effort high".into()), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert_eq!(app.input.lines(), &["/effort high"]);
    assert_eq!(app.pending_think, None);
    assert!(app.input_history.is_empty());
    assert!(matches!(app.state, State::Running));
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    assert!(draw(&mut app, 120, 30).contains("Model control unavailable"));
}

#[test]
fn unsupported_effort_level_is_rejected_locally_while_running() {
    // 模型只允许 low/high：拒绝在本地完成，不产生目标、不写日志。
    let (control, _binding, choices) = model_control_with(
        r#"{"providers":{"fixture":{"api_key":"fixture","models":{"one":{"model":"one","thinking":{"efforts":["low","high"]}}}}}}"#,
    );
    let (mut app, requests, _events) = plan_app();
    app.state = State::Running;
    app.choices = choices.clone();
    app.current_model = Some(choices[0].clone());
    app.model_control = Some(control);
    let mut dirty = false;
    app.handle(Event::Paste("/effort max".into()), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert_eq!(app.input.lines(), &["/effort max"]);
    assert_eq!(app.pending_think, None);
    assert!(app.input_history.is_empty());
    assert!(matches!(app.state, State::Running));
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    let screen = draw(&mut app, 160, 30);
    assert!(screen.contains("thinking level is not supported by the current model"));
    assert!(screen.contains("low, high"));
    // 不产生任何目标：服务端报告队列为空，失败只停在本地。
    assert!(
        app.model_control
            .as_ref()
            .unwrap()
            .registry
            .reports()
            .is_empty()
    );
}

#[test]
fn think_failed_report_keeps_applied_level_and_notifies() {
    let (control, binding) = model_control();
    let (mut app, _requests, _events) = plan_app();
    app.state = State::Running;
    app.model_control = Some(control);
    app.think_level = ThinkLevel::Low;
    app.pending_think = Some((3, ThinkLevel::High));
    binding.report(pp_runtime::model_control::Report::ThinkFailed {
        revision: 3,
        effort: ThinkLevel::High,
        message: "persisting session failed".into(),
    });
    app.receive();
    assert_eq!(app.think_level, ThinkLevel::Low);
    assert_eq!(app.pending_think, None);
    assert!(app.secondary_status.is_none());
    let screen = draw(&mut app, 160, 30);
    assert!(screen.contains("Thinking effort change failed: persisting session failed"));
    assert!(screen.lines().last().unwrap().contains(" low ·"));

    // 迟到的失败（revision 更小）不得清掉更新的期望档位。
    app.pending_think = Some((5, ThinkLevel::High));
    binding.report(pp_runtime::model_control::Report::ThinkFailed {
        revision: 2,
        effort: ThinkLevel::High,
        message: "stale failure".into(),
    });
    app.receive();
    assert_eq!(app.pending_think, Some((5, ThinkLevel::High)));
    assert_eq!(app.think_level, ThinkLevel::Low);
}

#[test]
fn effort_stays_rejected_while_reviewing_a_plan() {
    let (control, _binding) = model_control();
    let (mut app, requests, _events) = plan_app();
    app.state = State::ReviewingPlan;
    app.model_control = Some(control);
    app.choices = vec![all_levels_choice()];
    app.current_model = Some(app.choices[0].clone());
    let mut dirty = false;
    app.handle(Event::Paste("/effort high".into()), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    // 审阅期间整体拒绝：保留草稿、不登记目标、不进入 worker。
    assert_eq!(app.input.lines(), &["/effort high"]);
    assert_eq!(app.pending_think, None);
    assert!(matches!(app.state, State::ReviewingPlan));
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    app.handle(key(KeyCode::BackTab), &mut dirty);
    assert_eq!(app.pending_think, None);
    assert!(matches!(app.state, State::ReviewingPlan));
    assert!(draw(&mut app, 160, 30).contains("Plan review is running"));
}

#[test]
fn completion_model_arguments_open_search_in_every_editable_state() {
    for state in completion_states() {
        for command in ["/model", "/models"] {
            let (mut app, requests, _events) = plan_app();
            app.state = state;
            app.choices = favorite_model_choices();
            let mut dirty = false;
            app.handle(Event::Paste(format!("{command} vendor-B")), &mut dirty);
            assert_eq!(model_query(&app), "vendor-B");
            assert_eq!(menu_values(&app), [MenuAction::Model(1)]);
            assert!(draw(&mut app, 100, 30).contains("gateway/team/Beta"));
            if !matches!(
                state,
                State::Ready | State::Running | State::Cancelling | State::ReviewingPlan
            ) {
                app.handle(key(KeyCode::Enter), &mut dirty);
                assert!(app.model_panel.is_some());
            }
            app.handle(key(KeyCode::Esc), &mut dirty);
            assert!(app.model_panel.is_none());
            assert!(requests.try_recv().is_err());
        }
    }
}

#[test]
fn completion_files_remain_available_in_every_editable_state() {
    for state in completion_states() {
        let (mut app, scans, replies, requests) = file_picker_fixture();
        app.state = state;
        app.handle(Event::Paste("查看 @".into()), &mut false);
        scans.try_recv().unwrap();
        replies.send(Ok(vec!["README.md".into()])).unwrap();
        app.receive();
        assert_eq!(menu_values(&app), [MenuAction::File(0)]);
        app.handle(key(KeyCode::Tab), &mut false);
        assert_eq!(app.input.lines(), &["查看 @README.md "]);
        assert!(requests.try_recv().is_err());
        assert!(std::mem::discriminant(&app.state) == std::mem::discriminant(&state));
    }
}

#[test]
fn completion_tab_reopens_dismissed_and_recalled_drafts() {
    let (mut app, requests, _events) = plan_app();
    app.state = State::Compacting;
    app.handle(Event::Paste("/mo".into()), &mut false);
    app.handle(key(KeyCode::Esc), &mut false);
    assert!(app.menu.is_none());
    app.handle(key(KeyCode::Tab), &mut false);
    assert_eq!(app.input.lines(), &["/model"]);
    app.clear_input();
    app.input_history.push("/mo".into());
    app.handle(key(KeyCode::Up), &mut false);
    assert!(app.menu.is_none());
    app.handle(key(KeyCode::Tab), &mut false);
    assert_eq!(app.input.lines(), &["/model"]);
    assert!(requests.try_recv().is_err());
}

#[test]
fn command_tab_extends_only_the_shared_prefix() {
    let (mut app, requests, _events) = plan_app();
    app.file_commands.entries = ["/local-test-abc", "/local-test-acc"]
        .into_iter()
        .map(|name| commands::FileCommand {
            name: name.into(),
            description: String::new(),
            content: String::new(),
        })
        .collect();
    app.handle(Event::Paste("  /local-test".into()), &mut false);
    assert_eq!(menu_values(&app).len(), 2);
    app.handle(key(KeyCode::Down), &mut false);
    app.handle(key(KeyCode::Tab), &mut false);
    assert_eq!(app.input.lines(), &["  /local-test-a"]);
    assert_eq!(menu_values(&app).len(), 2);
    app.handle(key(KeyCode::Tab), &mut false);
    assert_eq!(app.input.lines(), &["  /local-test-a"]);
    app.handle(key(KeyCode::Char('b')), &mut false);
    app.handle(key(KeyCode::Tab), &mut false);
    assert_eq!(app.input.lines(), &["/local-test-abc"]);
    assert!(requests.try_recv().is_err());
}

#[test]
fn model_menu_prefers_exact_reference_over_current_prefix_match() {
    let mut app = app();
    app.state = State::Ready;
    app.choices = ["m", "m2"]
        .into_iter()
        .map(|id| ModelChoice {
            reference: format!("a/{id}"),
            model: id.into(),
            protocol: pp_config::models::Protocol::Completion,
            thinking: pp_config::models::ModelThinking::default(),
            context_window: None,
        })
        .collect();
    app.current_model = Some(app.choices[1].clone());
    app.default_model_reference = Some(app.choices[0].reference.clone());
    let mut dirty = false;
    app.handle(Event::Paste("/model a/".into()), &mut dirty);
    assert_eq!(active_menu(&app).selected_index(), Some(1));
    app.handle(key(KeyCode::Char('m')), &mut dirty);
    assert_eq!(
        menu_values(&app),
        vec![MenuAction::Model(0), MenuAction::Model(1)]
    );
    assert_eq!(active_menu(&app).selected_index(), Some(0));
}

#[test]
fn menu_overlay_preserves_editor_and_history_when_candidates_change() {
    for (width, height) in [(100, 30), (40, 12), (20, 6)] {
        for draft in ["/", "first line\n@"] {
            for scrolled in [false, true] {
                let mut app = app();
                app.show_welcome = false;
                for index in 0..40 {
                    app.append("Assistant", &format!("history-{index:02}"));
                }
                app.input.insert_str(draft);
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                let mut render = |app: &mut App| {
                    terminal
                        .draw(|frame| app.draw(frame, &StatusContext::default()))
                        .unwrap();
                    (
                        terminal.backend().buffer().clone(),
                        terminal.backend().cursor_position(),
                    )
                };
                render(&mut app);
                if scrolled {
                    app.conversation.page_up();
                }
                let (baseline, cursor) = render(&mut app);
                let input_top = height - 1 - (app.input.lines().len() as u16 + 2);
                for count in [12, 1, 3, 12] {
                    app.menu = Some(
                        SelectionMenu::new(
                            (0..count)
                                .map(|index| MenuItem {
                                    value: MenuAction::Command(index),
                                    label: format!("/candidate-{index}"),
                                    description: "Candidate description".into(),
                                    icon: None,
                                    current: false,
                                })
                                .collect(),
                            count - 1,
                        )
                        .with_hint("Enter select · Esc close"),
                    );
                    let menu_top = input_top - app.menu.as_ref().unwrap().height(input_top);
                    let (buffer, position) = render(&mut app);
                    assert_eq!(position, cursor);
                    // 覆盖区之外的对话、输入正文和状态栏必须逐格保持原位。
                    for y in (0..menu_top).chain(input_top + 1..height) {
                        for x in 0..width {
                            if y == height - 2 {
                                assert_eq!(buffer[(x, y)].symbol(), baseline[(x, y)].symbol());
                            } else {
                                assert_eq!(buffer[(x, y)], baseline[(x, y)]);
                            }
                        }
                    }
                }
                app.menu = None;
                assert_eq!(render(&mut app), (baseline, cursor));
            }
        }
    }
}

#[test]
fn command_menu_renders_above_input_with_columns_and_blue_selection() {
    let mut app = app();
    app.input.insert_str("/");
    app.refresh_candidates(true);
    let mut dirty = false;
    app.handle(key(KeyCode::Down), &mut dirty);
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, &StatusContext::default()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let input_row = 27;
    assert_eq!(buffer[(0, input_row)].symbol(), "❯");
    assert_eq!(buffer[(2, input_row)].symbol(), "/");
    assert!(!buffer[(3, input_row)].modifier.contains(Modifier::REVERSED));
    assert_eq!(terminal.backend().cursor_position(), (3, input_row).into());
    let separator_row = input_row - 1;
    for x in 0..100 {
        assert_eq!(buffer[(x, separator_row)].symbol(), "─");
        assert_eq!(buffer[(x, separator_row)].fg, menu::SEPARATOR);
    }
    let menu_top = separator_row - 8;
    for (index, spec) in COMMANDS.iter().take(8).enumerate() {
        let y = menu_top + index as u16;
        let active = index == 1;
        assert_eq!(buffer[(0, y)].symbol(), if active { "❯" } else { " " });
        assert_eq!(buffer[(2, y)].symbol(), "/");
        assert_eq!(buffer[(3, y)].symbol(), &spec.name[1..2]);
        assert_eq!(
            buffer[(3, y)].fg,
            if active {
                menu::SELECTED
            } else {
                Color::DarkGray
            }
        );
        assert!(!buffer[(4, y)].modifier.contains(Modifier::REVERSED));
    }
    assert_eq!(app.input.lines(), &["/"]);
    app.handle(key(KeyCode::End), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    for (width, height) in [(20, 6), (1, 1), (2, 2), (100, 30)] {
        let screen = draw(&mut app, width, height);
        if height == 6 || height == 30 {
            assert!(screen.contains("quit"));
        }
        assert_eq!(app.input.lines(), &["/"]);
    }
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &["/"]);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.menu.is_none());
    app.refresh_candidates(false);
    assert!(app.menu.is_none());
}

#[test]
fn shortcuts_cycle_all_levels_after_ack_without_recording_or_moving_draft() {
    let (control, binding) = model_control();
    let mut app = app();
    // 运行中切档：不得落回阻塞态，也不得经 worker 排队。
    app.state = State::Running;
    app.model_control = Some(control);
    app.choices = vec![all_levels_choice()];
    app.current_model = Some(app.choices[0].clone());
    let (sender, requests) = mpsc::channel();
    app.commands = Some(sender);
    app.input.insert_str("草稿\n未发送🙂");
    app.input_history.push("已提交".into());
    app.navigate_history(true);
    let cursor = app.input.cursor();
    let mut dirty = false;
    let encodings = [
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
    ];
    for (index, (level, label)) in [
        (ThinkLevel::Low, "low"),
        (ThinkLevel::Medium, "med"),
        (ThinkLevel::High, "hig"),
        (ThinkLevel::Xhigh, "xhi"),
        (ThinkLevel::Max, "max"),
        (ThinkLevel::None, "off"),
    ]
    .into_iter()
    .enumerate()
    {
        let pressed = encodings[index % encodings.len()];
        let previous = app.think_level;
        app.handle(Event::Key(pressed), &mut dirty);
        // 只登记期望值：State 未变、已应用档位未变、未向 worker 发任何命令。
        assert!(matches!(app.state, State::Running));
        assert_eq!(app.pending_think, Some((0, level)));
        assert_eq!(app.think_level, previous);
        assert_eq!(app.input.lines(), &["已提交"]);
        assert_eq!(app.input.cursor(), cursor);
        assert_eq!(app.history_index, Some(0));
        assert_eq!(app.input_history, ["已提交"]);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        // 长按产生的 Repeat 不重复登记目标。
        let mut repeat = pressed;
        repeat.kind = KeyEventKind::Repeat;
        app.handle(Event::Key(repeat), &mut dirty);
        assert_eq!(app.pending_think, Some((0, level)));
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        // 控制线程先确认待生效目标，再发布边界应用结果。
        let revision = wait_for_queued_think(&mut app, level);
        assert_eq!(app.think_level, previous);
        assert!(
            app.secondary_status
                .as_deref()
                .is_some_and(|status| status.contains("pending next request"))
        );
        binding.report(pp_runtime::model_control::Report::ThinkApplied {
            revision,
            effort: level,
        });
        app.receive();
        assert_eq!(app.pending_think, None);
        assert_eq!(app.think_level, level);
        assert!(app.secondary_status.is_none());
        let screen = draw(&mut app, 100, 30);
        assert!(
            screen
                .lines()
                .last()
                .unwrap()
                .contains(&format!(" {label} ·"))
        );
        assert!(!screen.contains("Thinking level:"));
        assert!(!screen.contains("╭"));
        assert_eq!(app.input.lines(), &["已提交"]);
        assert_eq!(app.input.cursor(), cursor);
        assert_eq!(app.history_index, Some(0));
        assert_eq!(app.input_history, ["已提交"]);
        // 待生效确认后 UI 只补发 ApplyModelTargets；其余命令依旧不经 worker。
        while let Ok(command) = requests.try_recv() {
            assert!(matches!(command, WorkerCommand::ApplyModelTargets));
        }
    }
    app.navigate_history(false);
    assert_eq!(app.input.lines(), &["草稿", "未发送🙂"]);
}

#[test]
fn shortcuts_model_picker_restores_history_draft_on_cancel_failure_and_success() {
    let mut app = app();
    app.state = State::Ready;
    app.choices = ["first", "second"]
        .map(|id| ModelChoice {
            reference: format!("fixture/{id}"),
            model: id.into(),
            protocol: pp_config::models::Protocol::Completion,
            thinking: pp_config::models::ModelThinking::default(),
            context_window: None,
        })
        .to_vec();
    app.current_model = Some(app.choices[0].clone());
    let (sender, requests) = mpsc::channel();
    app.commands = Some(sender);
    let (events, receiver) = mpsc::channel();
    app.events = Some(receiver);
    app.input.insert_str("草稿\n🙂结尾");
    app.input.move_cursor(CursorMove::Back);
    let cursor = app.input.cursor();
    app.input_history.push("旧任务".into());
    app.navigate_history(true);
    let mut dirty = false;
    app.handle(function_key('p'), &mut dirty);
    assert_eq!(app.input.lines(), &[""]);
    app.handle(function_key('p'), &mut dirty);
    app.handle(key(KeyCode::Tab), &mut dirty);
    assert_eq!(app.input.lines(), &[""]);
    assert_eq!(model_query(&app), "fixture/first");
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert_eq!(app.input.lines(), &["旧任务"]);
    assert_eq!(app.history_index, Some(0));
    app.navigate_history(false);
    assert_eq!(app.input.lines(), &["草稿", "🙂结尾"]);
    assert_eq!(app.input.cursor(), cursor);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));

    app.handle(function_key('p'), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert_eq!(app.input.lines(), &["草稿", "🙂结尾"]);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    for succeeds in [false, true] {
        app.handle(function_key('p'), &mut dirty);
        app.handle(key(KeyCode::Down), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::SwitchModel(reference)) if reference == "fixture/second")
        );
        assert_eq!(app.input.lines(), &["草稿", "🙂结尾"]);
        assert_eq!(app.input.cursor(), cursor);
        assert!(app.model_picker_draft.is_none());
        let result = if succeeds {
            Ok((app.choices[1].clone(), app.think_level))
        } else {
            Err(anyhow::anyhow!("fixture switch failure"))
        };
        events.send(WorkerEvent::ModelSwitched(result)).unwrap();
        app.receive();
        assert_eq!(
            app.current_model.as_ref().unwrap().model,
            if succeeds { "second" } else { "first" }
        );
        assert_eq!(app.input.lines(), &["草稿", "🙂结尾"]);
        assert_eq!(app.input_history, ["旧任务"]);
        assert_eq!(app.input.cursor(), cursor);
        if !succeeds {
            assert_notification(&mut app, Color::Rgb(224, 108, 117));
            assert!(draw(&mut app, 100, 30).contains("fixture switch failure"));
        }
        let screen = draw(&mut app, 100, 30);
        assert!(!screen.contains("Current model:"));
        assert!(screen.contains("fixture switch failure"));
        assert!(
            screen
                .lines()
                .last()
                .unwrap()
                .contains(if succeeds { "second" } else { "first" })
        );
    }
}

#[test]
fn shortcuts_reject_busy_actions_and_ignore_repeats_and_shift_tab_in_menus() {
    let mut app = app();
    let (sender, requests) = mpsc::channel();
    app.commands = Some(sender);
    app.input.insert_str("草稿🙂");
    let mut dirty = false;
    for state in [
        State::Resetting,
        State::SwitchingModel,
        State::SavingThinkDefault,
        State::Unavailable,
    ] {
        app.state = state;
        app.handle(key(KeyCode::BackTab), &mut dirty);
        app.handle(function_key('p'), &mut dirty);
        assert_eq!(app.input.lines(), &["草稿🙂"]);
        assert!(app.model_picker_draft.is_none());
    }
    app.state = State::Ready;
    app.skills_pending = true;
    app.handle(key(KeyCode::BackTab), &mut dirty);
    app.handle(function_key('p'), &mut dirty);
    app.skills_pending = false;
    for mut key in [
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('p'), FUNCTION_MODIFIER),
        KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
    ] {
        key.kind = KeyEventKind::Repeat;
        app.handle(Event::Key(key), &mut dirty);
        key.kind = KeyEventKind::Release;
        app.handle(Event::Key(key), &mut dirty);
    }
    assert_eq!(app.input.lines(), &["草稿🙂"]);
    assert!(app.model_picker_draft.is_none());
    app.clear_input();
    app.handle(Event::Paste("/effort ".into()), &mut dirty);
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)),
        &mut dirty,
    );
    assert_eq!(app.input.lines(), &["/effort "]);
    assert!(app.menu.is_some());
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    assert!(app.input_history.is_empty());
}

#[test]
fn shortcuts_help_and_empty_model_search_keep_drafts_and_never_submit_tasks() {
    let mut app = app();
    app.state = State::Ready;
    let (sender, requests) = mpsc::channel();
    app.commands = Some(sender);
    app.input.insert_str("未发送🙂");
    let cursor = app.input.cursor();
    let mut dirty = false;
    app.state = State::Running;
    app.handle(key(KeyCode::F(1)), &mut dirty);
    let screen = draw(&mut app, 180, 40);
    assert!(screen.contains("Shift + Tab"));
    assert!(screen.contains(if cfg!(target_os = "macos") {
        "⌥ + P"
    } else {
        "Ctrl + P"
    }));
    assert_eq!(app.input.lines(), &["未发送🙂"]);
    assert_eq!(app.input.cursor(), cursor);
    app.state = State::Ready;
    app.handle(function_key('p'), &mut dirty);
    app.handle(Event::Paste("missing".into()), &mut dirty);
    assert!(active_menu(&app).items().is_empty());
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert_eq!(app.input.lines(), &["未发送🙂"]);
    app.handle(function_key('p'), &mut dirty);
    app.input = editor();
    app.handle(Event::Paste("/effort high".into()), &mut dirty);
    assert!(app.menu.is_none());
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(app.model_picker_draft.is_some());
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert_eq!(app.input.lines(), &["未发送🙂"]);
    assert!(app.input_history.is_empty());
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn model_default_uses_ctrl_s_and_ignores_alt_s_and_repeats() {
    let mut app = app();
    app.state = State::Ready;
    app.choices = vec![ModelChoice {
        reference: "fixture/primary".into(),
        model: "primary".into(),
        protocol: pp_config::models::Protocol::Completion,
        thinking: pp_config::models::ModelThinking::default(),
        context_window: None,
    }];
    let (sender, requests) = mpsc::channel();
    app.commands = Some(sender);
    let mut dirty = false;
    app.handle(Event::Paste("/model ".into()), &mut dirty);
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT)),
        &mut dirty,
    );
    let mut repeat = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
    repeat.kind = KeyEventKind::Repeat;
    app.handle(Event::Key(repeat), &mut dirty);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    app.handle(control('s'), &mut dirty);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::SaveDefaultModel(reference)) if reference == "fixture/primary"
    ));
    assert_eq!(app.input.lines(), &[""]);
    assert!(app.model_panel.is_some());
}

#[test]
fn model_default_ack_keeps_search_selection_and_current_model() {
    let (mut app, requests, events) = plan_app();
    app.choices = favorite_model_choices();
    app.current_model = Some(app.choices[0].clone());
    app.default_model_reference = Some(app.choices[0].reference.clone());
    let mut dirty = false;
    app.handle(Event::Paste("/model ENDOR-gA".into()), &mut dirty);
    app.handle(control('s'), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Ok(WorkerCommand::SaveDefaultModel(reference)) if reference == app.choices[2].reference)
    );
    assert_eq!(
        app.default_model_reference.as_deref(),
        Some(app.choices[0].reference.as_str())
    );
    app.handle(key(KeyCode::Enter), &mut dirty);
    app.handle(control('s'), &mut dirty);
    assert!(requests.try_recv().is_err());
    events
        .send(WorkerEvent::DefaultModelSaved(Ok(app.choices[2]
            .reference
            .clone())))
        .unwrap();
    app.receive();
    assert_eq!(model_query(&app), "ENDOR-gA");
    assert_eq!(active_menu(&app).items()[0].value, MenuAction::Model(2));
    assert_eq!(active_menu(&app).label_suffixes()[0], Some(" "));
    assert_eq!(
        app.current_model.as_ref().unwrap().reference,
        app.choices[0].reference
    );
    app.handle(control('s'), &mut dirty);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::SaveDefaultModel(_))
    ));
    events
        .send(WorkerEvent::DefaultModelSaved(Err(anyhow::anyhow!(
            "fixture save failure"
        ))))
        .unwrap();
    app.receive();
    assert_eq!(model_query(&app), "ENDOR-gA");
    assert_eq!(
        app.default_model_reference.as_deref(),
        Some(app.choices[2].reference.as_str())
    );
    assert_eq!(app.input.lines(), &[""]);
    assert!(app.input_history.is_empty());
}

#[test]
fn effort_ctrl_s_saves_highlighted_level_without_changing_current_level() {
    let mut app = app();
    app.state = State::Ready;
    app.think_level = ThinkLevel::High;
    let (sender, requests) = mpsc::channel();
    app.commands = Some(sender);
    let (events, receiver) = mpsc::channel();
    app.events = Some(receiver);
    let mut dirty = false;
    app.handle(Event::Paste("/effort ".into()), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    let mut repeat = KeyEvent::new(KeyCode::Char('s'), FUNCTION_MODIFIER);
    repeat.kind = KeyEventKind::Repeat;
    app.handle(Event::Key(repeat), &mut dirty);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    app.handle(opposite_function_key('s'), &mut dirty);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    app.handle(function_key('s'), &mut dirty);
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::SaveDefaultThinkLevel(ThinkLevel::Xhigh))
    ));
    assert_eq!(app.think_level, ThinkLevel::High);
    app.handle(function_key('s'), &mut dirty);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    app.handle(Event::Paste("新草稿".into()), &mut dirty);
    events
        .send(WorkerEvent::DefaultThinkLevelSaved(Ok(ThinkLevel::Xhigh)))
        .unwrap();
    app.receive();
    assert_eq!(app.input.lines(), &["新草稿"]);
    assert_eq!(app.think_level, ThinkLevel::High);
    let baseline = draw(&mut app, 100, 30);
    assert!(!baseline.contains("Default thinking effort saved:"));
    let snapshot = draw_buffer(&mut app, 100, 30);
    let mut scrollback = Vec::new();
    app.conversation
        .write_full_for_test(&mut scrollback, &snapshot)
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&scrollback)
            .contains("Default thinking effort saved: xhigh (takes effect next launch)")
    );
    assert_eq!(draw(&mut app, 100, 30), baseline);
    assert_eq!(app.input.lines(), &["新草稿"]);
    assert_eq!(app.think_level, ThinkLevel::High);
    app.handle(function_key('s'), &mut dirty);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn shortcuts_platform_function_modifier_preserves_editor_bindings() {
    let mut other = app();
    let mut app = app();
    let mut dirty = false;
    app.state = State::Ready;
    app.input.insert_str("draft-中");
    app.input.move_cursor(CursorMove::Back);
    let cursor = app.input.cursor();
    app.handle(function_key('p'), &mut dirty);
    assert_eq!(app.input.lines(), &[""]);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.model_picker_draft.is_none());
    assert_eq!(app.input.lines(), &["draft-中"]);
    assert_eq!(app.input.cursor(), cursor);

    other.state = State::Ready;
    other.input.insert_str("draft-中");
    other.handle(opposite_function_key('p'), &mut dirty);
    assert!(other.model_picker_draft.is_none());
    assert_eq!(other.input.lines(), &["draft-中"]);

    app.input.move_cursor(CursorMove::Forward);
    app.handle(control('j'), &mut dirty);
    assert_eq!(app.input.lines(), &["draft-中", ""]);
    app.handle(control('a'), &mut dirty);
    app.handle(control('e'), &mut dirty);
    app.handle(control('k'), &mut dirty);
    assert_eq!(app.input.lines(), &["draft-中", ""]);
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT)),
        &mut dirty,
    );
    assert_eq!(app.input.lines(), &["draft-中", ""]);
    app.handle(key(KeyCode::Char('π')), &mut dirty);
    assert_eq!(app.input.lines(), &["draft-中", "π"]);
    assert!(app.model_picker_draft.is_none());
    assert!(app.handle(control('c'), &mut dirty));
}

#[test]
fn effort_space_opens_inline_menu_and_confirms_only_after_enter() {
    let (control, binding) = model_control();
    let mut app = app();
    app.state = State::Ready;
    app.model_control = Some(control);
    app.think_level = ThinkLevel::High;
    app.choices = vec![all_levels_choice()];
    app.current_model = Some(app.choices[0].clone());
    let (sender, requests) = mpsc::channel();
    app.commands = Some(sender);
    let mut dirty = false;
    app.input.insert_str("/effort");
    app.refresh_candidates(true);
    assert!(
        menu_values(&app)
            .iter()
            .all(|action| matches!(action, MenuAction::Command(_)))
    );
    app.handle(key(KeyCode::Char(' ')), &mut dirty);
    let screen = draw(&mut app, 100, 30);
    // 测试缓冲区把中文占用的第二列表示为空格。
    let compact = screen.replace(' ', "");
    assert!(
        menu_values(&app)
            .iter()
            .all(|action| matches!(action, MenuAction::Effort(_)))
    );
    assert!(compact.contains("Deepthinking(current)"));
    assert!(screen.contains("❯ high"));
    assert!(compact.find("Disablethinking").unwrap() < compact.rfind("❯/effort").unwrap());
    assert!(compact.contains("Highestlevelthemodelsupports"));
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    for (width, height) in [(20, 6), (1, 1), (100, 30)] {
        draw(&mut app, width, height);
        assert_eq!(app.input.lines(), &["/effort "]);
    }
    assert!(draw(&mut app, 100, 30).contains("❯ max"));
    app.handle(key(KeyCode::Enter), &mut dirty);
    // 确认后只登记期望值：不改 State、不经 worker，也不移动草稿。
    assert_eq!(app.pending_think, Some((0, ThinkLevel::Max)));
    assert_eq!(app.think_level, ThinkLevel::High);
    assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
    assert!(app.menu.is_none());
    app.input.insert_str("新草稿");
    let revision = wait_for_queued_think(&mut app, ThinkLevel::Max);
    assert_eq!(app.think_level, ThinkLevel::High);
    binding.report(pp_runtime::model_control::Report::ThinkApplied {
        revision,
        effort: ThinkLevel::Max,
    });
    app.receive();
    assert_eq!(app.think_level, ThinkLevel::Max);
    assert_eq!(app.pending_think, None);
    assert_eq!(app.input.lines(), &["新草稿"]);
}

#[test]
fn sqlite_model_menu_orders_favorites_then_usage() {
    let (mut app, requests, _events) = plan_app();
    app.choices = ["p/one-a", "p/one-b", "p/two"]
        .into_iter()
        .map(|id| ModelChoice {
            reference: id.into(),
            model: format!("vendor-{id}"),
            protocol: pp_config::models::Protocol::Completion,
            thinking: pp_config::models::ModelThinking::default(),
            context_window: None,
        })
        .collect();
    app.current_model = Some(app.choices[0].clone());
    app.default_model_reference = Some(app.choices[0].reference.clone());
    app.favorite_models = vec!["p/two".into()];
    app.model_usage = std::collections::BTreeMap::from([
        ("p/one-a".to_string(), 5),
        ("p/one-b".to_string(), 5),
        ("p/two".to_string(), 1),
    ]);

    let mut dirty = false;
    app.handle(Event::Paste("/model ".into()), &mut dirty);
    // 收藏 p/two 尽管次数最低仍居首；非收藏按次数降序，5/5 平手保持目录顺序。
    assert_eq!(
        menu_values(&app),
        vec![
            MenuAction::Model(2),
            MenuAction::Model(0),
            MenuAction::Model(1)
        ]
    );

    // 过滤到平手对：排序键在筛选结果内同样生效且保持目录顺序。
    app.close_model_panel();
    app.input = editor();
    app.handle(Event::Paste("/model one".into()), &mut dirty);
    assert_eq!(
        menu_values(&app),
        vec![MenuAction::Model(0), MenuAction::Model(1)]
    );

    // 打开面板、筛选、取消不发请求，也不改动缓存的用量快照。
    let usage_before = app.model_usage.clone();
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.model_usage, usage_before);

    // 选中 p/one-b 后目录刷新重排；选择跟随同一模型而不是固定位置。
    app.input = editor();
    app.handle(Event::Paste("/model ".into()), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    assert_eq!(active_menu(&app).selected_index(), Some(2));
    let mut usage = app.model_usage.clone();
    usage.insert("p/one-b".to_string(), 9);
    app.receive_event_for_test(WorkerEvent::ModelsReloaded(Ok(crate::tui::ModelListing {
        roles: Default::default(),
        default_effort: ThinkLevel::default(),
        choices: app.choices.clone(),
        default_reference: "p/one-a".into(),
        favorites: app.favorite_models.clone(),
        usage: usage.clone(),
        override_selection: None,
        source: ModelSource::Default,
    })));
    assert_eq!(app.model_usage, usage);
    assert_eq!(
        menu_values(&app),
        vec![
            MenuAction::Model(2),
            MenuAction::Model(1),
            MenuAction::Model(0)
        ]
    );
    assert_eq!(active_menu(&app).selected_index(), Some(1));

    // 用量读取失败：保留旧快照与菜单，单独报错，不把缓存清零。
    app.receive_event_for_test(WorkerEvent::ModelsReloaded(Err(anyhow::anyhow!(
        "Agent database unavailable."
    ))));
    assert_eq!(app.model_usage, usage);
    assert_eq!(
        menu_values(&app),
        vec![
            MenuAction::Model(2),
            MenuAction::Model(1),
            MenuAction::Model(0)
        ]
    );
    assert!(draw(&mut app, 130, 30).contains("Could not refresh models"));

    // 偏好已提交但随后的快照读取失败：保存成功与刷新失败分别提示，不伪装保存失败。
    app.receive_event_for_test(WorkerEvent::DefaultModelSaved(Ok("p/one-b".into())));
    assert!(!draw(&mut app, 130, 30).contains("Default model saved:"));
    app.receive_event_for_test(WorkerEvent::ModelsReloaded(Err(anyhow::anyhow!(
        "Agent database unavailable."
    ))));
    assert_eq!(app.default_model_reference.as_deref(), Some("p/one-b"));
    assert!(draw(&mut app, 130, 30).contains("Could not refresh models"));
}

#[test]
fn model_picker_submits_override_with_effective_default_effort() {
    let (control, _binding, choices) = model_control_with(
        r#"{"providers":{"fixture":{"api_key":"fixture","models":{"one":{"model":"one"},"two":{"model":"two","thinking":{"efforts":["low","high"]}}}}}}"#,
    );
    let (mut app, requests, _events) = plan_app();
    app.model_control = Some(control);
    app.choices = choices;
    app.current_model = Some(app.choices[0].clone());
    // 全局默认档位 High 由 two 支持：⌥P 应记录该模型的有效默认档位。
    app.default_think_level = ThinkLevel::High;
    let mut dirty = false;
    app.handle(function_key('p'), &mut dirty);
    app.handle(key(KeyCode::Down), &mut dirty);
    app.handle(key(KeyCode::Enter), &mut dirty);
    assert!(
        matches!(requests.try_recv(), Err(TryRecvError::Empty)),
        "model_control 路径不应发送 worker 命令"
    );
    let selection = wait_for_override(&mut app, "fixture/two");
    // 目标档位沿既有策略：two 无显式 defaultLevel，当前档位 off 与全局默认都不在
    // 它允许的 low/high 内，因此落到该模型的首个允许档位 Low。
    assert_eq!(selection.effort, ThinkLevel::Low);
    // 目标已登记但尚未由 Agent 应用：状态栏仍显示旧模型，来源等应用后再更新。
    assert!(app.pending_model.is_some());
    assert_eq!(app.current_model.as_ref().unwrap().reference, "fixture/one");
}

#[test]
fn statusline_shows_override_and_fallback_sources_but_not_default() {
    let mut app = app();
    app.current_model = Some(ModelChoice {
        reference: "fixture/one".into(),
        model: "one".into(),
        protocol: pp_config::models::Protocol::Completion,
        thinking: pp_config::models::ModelThinking::default(),
        context_window: None,
    });
    app.think_level = ThinkLevel::None;
    for (source, marker) in [
        (None, None),
        (Some(ModelSource::Default), None),
        (Some(ModelSource::Override), Some(" · override")),
        (Some(ModelSource::Fallback), Some(" · fallback")),
        (Some(ModelSource::Role), Some(" · role")),
    ] {
        app.current_source = source;
        let screen = draw(&mut app, 130, 30);
        let status = screen.lines().last().unwrap();
        match marker {
            Some(marker) => assert!(status.contains(marker), "{status}"),
            None => assert!(
                !status.contains("override")
                    && !status.contains("fallback")
                    && !status.contains(" · role"),
                "{status}"
            ),
        }
        assert!(status.contains("fixture/one"), "{status}");
    }
}
