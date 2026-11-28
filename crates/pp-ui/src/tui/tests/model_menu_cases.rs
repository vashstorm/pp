use super::*;

fn completion_states() -> impl Iterator<Item = State> {
    [
        State::ManagingMcp,
        State::Ready,
        State::Running,
        State::RunningShell,
        State::RunningTools,
        State::RunningWebSearch,
        State::Cancelling,
        State::Compacting,
        State::InspectingContext,
        State::Resetting,
        State::SwitchingModel,
        State::SavingModelSettings,
        State::SavingThinkDefault,
        State::Resuming,
        State::ReadingTree,
        State::NavigatingTree,
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
fn command_menu_lists_debug_only_and_prioritizes_prefixes() {
    let mut app = app();
    let mut dirty = false;
    app.handle(Event::Paste("/test-ui".into()), &mut dirty);
    if cfg!(debug_assertions) {
        let values = menu_values(&app);
        assert_eq!(values.len(), 1, "{values:?}");
        let MenuAction::Command(index) = values[0] else {
            panic!("unexpected menu action: {values:?}");
        };
        assert_eq!(COMMANDS[index].name, "/test-ui");
        assert_eq!(app.menu.as_ref().unwrap().items()[0].label, "/test-ui");

        app.input = editor();
        app.handle(Event::Paste("/test-too".into()), &mut dirty);
        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.items().len(), 1);
        assert_eq!(menu.items()[0].label, "/test-tools");
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &["/test-tools"]);
    } else {
        assert!(app.menu.is_none());
        assert_eq!(app.input.lines(), &["/test-ui"]);
    }

    for (query, first, completed) in [
        ("/roles", "/roles", "/roles"),
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
fn model_panel_layout_search_resize_and_wheel_isolation() {
    {
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

    {
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

    {
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
}

#[test]
fn model_favorites_search_and_sqlite_usage_ordering() {
    {
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

    {
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

        let mut plain = super::app();
        plain.state = State::Ready;
        plain.choices = vec![ModelChoice {
            reference: "gateway/team/model with spaces".into(),
            model: "vendor-one".into(),
            protocol: pp_config::models::Protocol::Completion,
            thinking: pp_config::models::ModelThinking::default(),
            context_window: None,
        }];
        plain.handle(Event::Paste("/model vendor".into()), &mut dirty);
        assert_eq!(menu_values(&plain), vec![MenuAction::Model(0)]);
        plain.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(plain.input.lines(), &[""]);
        assert_eq!(model_query(&plain), "gateway/team/model with spaces");
        plain.handle(key(KeyCode::Esc), &mut dirty);
        assert!(plain.model_panel.is_none());
        plain.refresh_candidates(false);
        assert!(plain.menu.is_none());
        plain.handle(key(KeyCode::Backspace), &mut dirty);
        assert!(plain.model_panel.is_none());
        plain.input = editor();
        plain.input.insert_str("/model missing");
        plain.refresh_candidates(true);
        assert!(active_menu(&plain).items().is_empty());
        assert!(draw(&mut plain, 100, 30).contains("No matching models"));
        plain.close_model_panel();
    }
}

#[test]
fn completion_commands_models_and_files_across_editable_states() {
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
        assert!(requests.try_recv().is_err() && app.input_history.is_empty());
        assert!(!cancellation.is_cancelled());
        assert!(std::mem::discriminant(&app.state) == std::mem::discriminant(&state));
    }

    let (mut reopen, requests, _events) = plan_app();
    reopen.state = State::Compacting;
    reopen.handle(Event::Paste("/mo".into()), &mut false);
    reopen.handle(key(KeyCode::Esc), &mut false);
    assert!(reopen.menu.is_none());
    reopen.handle(key(KeyCode::Tab), &mut false);
    assert_eq!(reopen.input.lines(), &["/model"]);
    reopen.clear_input();
    reopen.input_history.push("/mo".into());
    reopen.handle(key(KeyCode::Up), &mut false);
    assert!(reopen.menu.is_none());
    reopen.handle(key(KeyCode::Tab), &mut false);
    assert_eq!(reopen.input.lines(), &["/model"]);
    assert!(requests.try_recv().is_err());

    let (mut prefix, requests, _events) = plan_app();
    prefix.file_commands.entries = ["/local-test-abc", "/local-test-acc"]
        .into_iter()
        .map(|name| commands::FileCommand {
            name: name.into(),
            description: String::new(),
            content: String::new(),
        })
        .collect();
    prefix.handle(Event::Paste("  /local-test".into()), &mut false);
    assert_eq!(menu_values(&prefix).len(), 2);
    prefix.handle(key(KeyCode::Down), &mut false);
    prefix.handle(key(KeyCode::Tab), &mut false);
    assert_eq!(prefix.input.lines(), &["  /local-test-a"]);
    assert_eq!(menu_values(&prefix).len(), 2);
    prefix.handle(key(KeyCode::Tab), &mut false);
    assert_eq!(prefix.input.lines(), &["  /local-test-a"]);
    prefix.handle(key(KeyCode::Char('b')), &mut false);
    prefix.handle(key(KeyCode::Tab), &mut false);
    assert_eq!(prefix.input.lines(), &["/local-test-abc"]);
    assert!(requests.try_recv().is_err());

    let mut exact = app();
    exact.state = State::Ready;
    exact.choices = ["m", "m2"]
        .into_iter()
        .map(|id| ModelChoice {
            reference: format!("a/{id}"),
            model: id.into(),
            protocol: pp_config::models::Protocol::Completion,
            thinking: pp_config::models::ModelThinking::default(),
            context_window: None,
        })
        .collect();
    exact.current_model = Some(exact.choices[1].clone());
    exact.default_model_reference = Some(exact.choices[0].reference.clone());
    let mut dirty = false;
    exact.handle(Event::Paste("/model a/".into()), &mut dirty);
    assert_eq!(active_menu(&exact).selected_index(), Some(1));
    exact.handle(key(KeyCode::Char('m')), &mut dirty);
    assert_eq!(
        menu_values(&exact),
        vec![MenuAction::Model(0), MenuAction::Model(1)]
    );
    assert_eq!(active_menu(&exact).selected_index(), Some(0));

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
            assert!(app.model_panel.is_none() && requests.try_recv().is_err());
        }

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
fn menu_overlay_and_command_menu_render_without_moving_editor_or_history() {
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
                    // Conversation, input body and status bar outside the overlay must stay put cell by cell.
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

    {
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
            assert_eq!(buffer[(x, separator_row)].fg, menu::separator());
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
                    menu::selected()
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
}

#[test]
fn shortcuts_cycle_levels_model_picker_draft_and_modifiers() {
    {
        let (control, binding) = model_control();
        let mut app = app();
        // Switching level during a run: must not fall back to a blocked state and must not queue through the worker.
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
            // Only a desired value is recorded: State is unchanged, the applied level is unchanged, and no worker command is sent.
            assert!(matches!(app.state, State::Running));
            assert_eq!(app.pending_think, Some((0, level)));
            assert_eq!(app.think_level, previous);
            assert_eq!(app.input.lines(), &["已提交"]);
            assert_eq!(app.input.cursor(), cursor);
            assert_eq!(app.history_index, Some(0));
            assert_eq!(app.input_history, ["已提交"]);
            assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
            // Repeat events from a held key do not re-record the target.
            let mut repeat = pressed;
            repeat.kind = KeyEventKind::Repeat;
            app.handle(Event::Key(repeat), &mut dirty);
            assert_eq!(app.pending_think, Some((0, level)));
            assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
            // The control thread confirms the pending target first, then publishes the boundary application result.
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
            // After the pending confirmation the UI only re-sends ApplyModelTargets; other commands still bypass the worker.
            while let Ok(command) = requests.try_recv() {
                assert!(matches!(command, WorkerCommand::ApplyModelTargets));
            }
        }
        app.navigate_history(false);
        assert_eq!(app.input.lines(), &["草稿", "未发送🙂"]);
    }

    {
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
            assert!(screen.lines().last().unwrap().contains(if succeeds {
                "second"
            } else {
                "first"
            }));
        }
    }

    {
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
}

#[test]
fn shortcuts_busy_repeats_help_and_ctrl_s_defaults() {
    {
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
        for mut ev in [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('p'), FUNCTION_MODIFIER),
            KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE),
        ] {
            ev.kind = KeyEventKind::Repeat;
            app.handle(Event::Key(ev), &mut dirty);
            ev.kind = KeyEventKind::Release;
            app.handle(Event::Key(ev), &mut dirty);
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
        assert!(app.menu.is_some() && requests.try_recv().is_err() && app.input_history.is_empty());

        let mut help_app = super::app();
        help_app.state = State::Ready;
        let (sender, help_req) = mpsc::channel();
        help_app.commands = Some(sender);
        help_app.input.insert_str("未发送🙂");
        let cursor = help_app.input.cursor();
        help_app.state = State::Running;
        help_app.handle(key(KeyCode::F(1)), &mut dirty);
        let screen = draw(&mut help_app, 180, 40);
        assert!(screen.contains("Shift + Tab"));
        assert!(screen.contains(if cfg!(target_os = "macos") {
            "⌥ + P"
        } else {
            "Ctrl + P"
        }));
        assert_eq!(help_app.input.lines(), &["未发送🙂"]);
        assert_eq!(help_app.input.cursor(), cursor);
        help_app.state = State::Ready;
        help_app.handle(function_key('p'), &mut dirty);
        help_app.handle(Event::Paste("missing".into()), &mut dirty);
        assert!(active_menu(&help_app).items().is_empty());
        help_app.handle(key(KeyCode::Esc), &mut dirty);
        assert_eq!(help_app.input.lines(), &["未发送🙂"]);
        help_app.handle(function_key('p'), &mut dirty);
        help_app.input = editor();
        help_app.handle(Event::Paste("/effort high".into()), &mut dirty);
        assert!(help_app.menu.is_none());
        help_app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(help_app.model_picker_draft.is_some());
        help_app.handle(key(KeyCode::Esc), &mut dirty);
        assert_eq!(help_app.input.lines(), &["未发送🙂"]);
        assert!(help_app.input_history.is_empty() && help_req.try_recv().is_err());
    }

    {
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

        let (mut ack, req_ack, events) = plan_app();
        ack.choices = favorite_model_choices();
        ack.current_model = Some(ack.choices[0].clone());
        ack.default_model_reference = Some(ack.choices[0].reference.clone());
        ack.handle(Event::Paste("/model ENDOR-gA".into()), &mut dirty);
        ack.handle(control('s'), &mut dirty);
        assert!(
            matches!(req_ack.try_recv(), Ok(WorkerCommand::SaveDefaultModel(reference)) if reference == ack.choices[2].reference)
        );
        assert_eq!(
            ack.default_model_reference.as_deref(),
            Some(ack.choices[0].reference.as_str())
        );
        ack.handle(key(KeyCode::Enter), &mut dirty);
        ack.handle(control('s'), &mut dirty);
        assert!(req_ack.try_recv().is_err());
        events
            .send(WorkerEvent::DefaultModelSaved(Ok(ack.choices[2]
                .reference
                .clone())))
            .unwrap();
        ack.receive();
        assert_eq!(model_query(&ack), "ENDOR-gA");
        assert_eq!(active_menu(&ack).items()[0].value, MenuAction::Model(2));
        assert_eq!(active_menu(&ack).label_suffixes()[0], Some(" "));
        assert_eq!(
            ack.current_model.as_ref().unwrap().reference,
            ack.choices[0].reference
        );
        ack.handle(control('s'), &mut dirty);
        assert!(matches!(
            req_ack.try_recv(),
            Ok(WorkerCommand::SaveDefaultModel(_))
        ));
        events
            .send(WorkerEvent::DefaultModelSaved(Err(anyhow::anyhow!(
                "fixture save failure"
            ))))
            .unwrap();
        ack.receive();
        assert_eq!(model_query(&ack), "ENDOR-gA");
        assert_eq!(
            ack.default_model_reference.as_deref(),
            Some(ack.choices[2].reference.as_str())
        );
        assert_eq!(ack.input.lines(), &[""]);
        assert!(ack.input_history.is_empty());

        let mut eff = super::app();
        eff.state = State::Ready;
        eff.think_level = ThinkLevel::High;
        let (sender, eff_req) = mpsc::channel();
        eff.commands = Some(sender);
        let (events, receiver) = mpsc::channel();
        eff.events = Some(receiver);
        eff.handle(Event::Paste("/effort ".into()), &mut dirty);
        eff.handle(key(KeyCode::Down), &mut dirty);
        let mut repeat = KeyEvent::new(KeyCode::Char('s'), FUNCTION_MODIFIER);
        repeat.kind = KeyEventKind::Repeat;
        eff.handle(Event::Key(repeat), &mut dirty);
        assert!(matches!(eff_req.try_recv(), Err(TryRecvError::Empty)));
        eff.handle(opposite_function_key('s'), &mut dirty);
        assert!(matches!(eff_req.try_recv(), Err(TryRecvError::Empty)));
        eff.handle(function_key('s'), &mut dirty);
        assert!(matches!(
            eff_req.try_recv(),
            Ok(WorkerCommand::SaveDefaultThinkLevel(ThinkLevel::Xhigh))
        ));
        assert_eq!(eff.think_level, ThinkLevel::High);
        eff.handle(function_key('s'), &mut dirty);
        assert!(matches!(eff_req.try_recv(), Err(TryRecvError::Empty)));
        eff.handle(Event::Paste("新草稿".into()), &mut dirty);
        events
            .send(WorkerEvent::DefaultThinkLevelSaved(Ok(ThinkLevel::Xhigh)))
            .unwrap();
        eff.receive();
        assert_eq!(eff.input.lines(), &["新草稿"]);
        assert_eq!(eff.think_level, ThinkLevel::High);
        let baseline = draw(&mut eff, 100, 30);
        assert!(!baseline.contains("Default thinking effort saved:"));
        let snapshot = draw_buffer(&mut eff, 100, 30);
        let mut scrollback = Vec::new();
        eff.conversation
            .write_full_for_test(&mut scrollback, &snapshot)
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&scrollback)
                .contains("Default thinking effort saved: xhigh (takes effect next launch)")
        );
        assert_eq!(draw(&mut eff, 100, 30), baseline);
        assert_eq!(eff.input.lines(), &["新草稿"]);
        assert_eq!(eff.think_level, ThinkLevel::High);
        eff.handle(function_key('s'), &mut dirty);
        assert!(matches!(eff_req.try_recv(), Err(TryRecvError::Empty)));
    }
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
    // The test buffer represents the second column occupied by a wide character as a space.
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
    // After confirmation only a desired value is recorded: State is untouched, the worker is bypassed and the draft does not move.
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
    // Favorite p/two stays first despite the lowest count; non-favorites sort by descending count, and a 5/5 tie keeps catalog order.
    assert_eq!(
        menu_values(&app),
        vec![
            MenuAction::Model(2),
            MenuAction::Model(0),
            MenuAction::Model(1)
        ]
    );

    // Filter down to the tie: the sort key still applies within the filtered results and keeps catalog order.
    app.close_model_panel();
    app.input = editor();
    app.handle(Event::Paste("/model one".into()), &mut dirty);
    assert_eq!(
        menu_values(&app),
        vec![MenuAction::Model(0), MenuAction::Model(1)]
    );

    // Opening the panel, filtering and cancelling send no request and leave the cached usage snapshot untouched.
    let usage_before = app.model_usage.clone();
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.model_usage, usage_before);

    // After selecting p/one-b the catalog refresh reorders; the selection follows the same model rather than a fixed position.
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

    // Usage read failure: keep the old snapshot and menu, report the error separately and do not zero the cache.
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

    // The preference was committed but the following snapshot read failed: report the save and the refresh failure separately instead of faking a save failure.
    app.receive_event_for_test(WorkerEvent::DefaultModelSaved(Ok("p/one-b".into())));
    assert!(!draw(&mut app, 130, 30).contains("Default model saved:"));
    app.receive_event_for_test(WorkerEvent::ModelsReloaded(Err(anyhow::anyhow!(
        "Agent database unavailable."
    ))));
    assert_eq!(app.default_model_reference.as_deref(), Some("p/one-b"));
    assert!(draw(&mut app, 130, 30).contains("Could not refresh models"));
}

#[test]
fn model_picker_submits_override_and_statusline_hides_source_until_fallback() {
    {
        let (control, _binding, choices) = model_control_with(
            r#"{"providers":{"fixture":{"api_key":"fixture","models":{"one":{"model":"one"},"two":{"model":"two","thinking":{"efforts":["low","high"]}}}}}}"#,
        );
        let (mut app, requests, _events) = plan_app();
        app.model_control = Some(control);
        app.choices = choices;
        app.current_model = Some(app.choices[0].clone());
        // The global default level High is supported by two: ⌥P should record that model's effective default level.
        app.default_think_level = ThinkLevel::High;
        let mut dirty = false;
        app.handle(function_key('p'), &mut dirty);
        app.handle(key(KeyCode::Down), &mut dirty);
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Err(TryRecvError::Empty)),
            "the model_control path must not send worker commands"
        );
        let selection = wait_for_override(&mut app, "fixture/two");
        // The target level follows the existing policy: two has no explicit defaultLevel, and neither the current off level nor the global default is among
        // its allowed low/high, so it lands on the model's first allowed level, Low.
        assert_eq!(selection.effort, ThinkLevel::Low);
        // The target is recorded but not yet applied by the Agent: the status bar still shows the old model, and the source updates after application.
        assert!(app.pending_model.is_some());
        assert_eq!(app.current_model.as_ref().unwrap().reference, "fixture/one");
    }

    {
        let mut app = app();
        app.current_model = Some(ModelChoice {
            reference: "fixture/one".into(),
            model: "one".into(),
            protocol: pp_config::models::Protocol::Completion,
            thinking: pp_config::models::ModelThinking::default(),
            context_window: None,
        });
        app.think_level = ThinkLevel::None;
        for source in [
            None,
            Some(ModelSource::Default),
            Some(ModelSource::Override),
            Some(ModelSource::Fallback),
            Some(ModelSource::Role),
        ] {
            app.current_source = source;
            let screen = draw(&mut app, 130, 30);
            let status = screen.lines().last().unwrap();
            assert!(
                !status.contains("override")
                    && !status.contains("fallback")
                    && !status.contains(" · role"),
                "{status}"
            );
            assert!(status.contains("fixture/one"), "{status}");
        }

        // Switching to a fallback model adds a prominent card to the content column instead of relying on one word in the status bar.
        app.show_welcome = false;
        app.receive_event_for_test(WorkerEvent::Progress(AgentEvent::ModelRouteChanged {
            model: "fixture/two".into(),
            effort: ThinkLevel::Low,
            source: ModelSource::Fallback,
            temporary: false,
        }));
        let screen = draw(&mut app, 130, 30);
        assert!(screen.contains("Fallback"), "{screen}");
        assert!(screen.contains("Using the fallback model."), "{screen}");
        assert!(screen.contains("fixture/two"), "{screen}");
        let status = screen.lines().last().unwrap();
        assert!(!status.contains("fallback"), "{status}");
    }
}
