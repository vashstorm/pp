use super::*;

#[test]
fn command_menu_lists_debug_only_commands_in_debug_builds() {
    let mut app = app();
    let mut dirty = false;
    app.handle(Event::Paste("/task-te".into()), &mut dirty);
    if cfg!(debug_assertions) {
        // 过滤发生在 enumerate 之后：MenuItem 仍指向 COMMANDS 的真实下标。
        let values = menu_values(&app);
        assert_eq!(values.len(), 1);
        let MenuAction::Command(index) = values[0] else {
            panic!("unexpected menu action: {values:?}");
        };
        assert_eq!(COMMANDS[index].name, "/task-test");
        assert_eq!(app.menu.as_ref().unwrap().items()[0].label, "/task-test");
    } else {
        assert!(app.menu.is_none());
        assert_eq!(app.input.lines(), &["/task-te"]);
    }
}

#[test]
fn command_menu_matches_fuzzy_case_insensitively_and_prioritizes_prefixes() {
    let mut app = app();
    let mut dirty = false;
    for (query, expected) in [
        ("/roles", "/Roles"),
        ("/MDL", "/model"),
        ("/PLS", "/plan-list"),
        ("/LIST", "/plan-list"),
        ("/PLAN", "/plan"),
        ("/RE", "/resume"),
    ] {
        app.input = editor();
        app.handle(Event::Paste(query.into()), &mut dirty);
        let menu = app.menu.as_ref().unwrap();
        assert_eq!(menu.items()[0].label, expected, "{query}");
        assert_eq!(menu.selected_index(), Some(0));
        app.handle(key(KeyCode::Tab), &mut dirty);
        assert_eq!(app.input.lines(), &[expected]);
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
            protocol: crate::models::Protocol::Response,
            thinking: crate::models::ModelThinking::default(),
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
            protocol: crate::models::Protocol::Response,
            thinking: crate::models::ModelThinking::default(),
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
        protocol: crate::models::Protocol::Completion,
        thinking: crate::models::ModelThinking::default(),
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
    app.input = editor();
    app.input.insert_str("/model ");
    for state in [State::Running, State::SwitchingModel, State::Unavailable] {
        app.state = state;
        app.refresh_candidates(true);
        assert!(app.menu.is_none());
        assert!(app.model_panel.is_none());
    }
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
            protocol: crate::models::Protocol::Completion,
            thinking: crate::models::ModelThinking::default(),
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
    assert_eq!(app.input.lines(), &["/quit"]);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(app.menu.is_none());
    app.refresh_candidates(false);
    assert!(app.menu.is_none());
}

#[test]
fn shortcuts_cycle_all_levels_after_ack_without_recording_or_moving_draft() {
    let mut app = app();
    app.state = State::Ready;
    let (sender, requests) = mpsc::channel();
    app.commands = Some(sender);
    let (events, receiver) = mpsc::channel();
    app.events = Some(receiver);
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
        let key = encodings[index % encodings.len()];
        let previous = app.think_level;
        app.handle(Event::Key(key), &mut dirty);
        assert!(
            matches!(requests.try_recv(), Ok(WorkerCommand::SetThinkLevel(value)) if value == level)
        );
        assert_eq!(app.think_level, previous);
        let mut repeat = key;
        repeat.kind = KeyEventKind::Repeat;
        app.handle(Event::Key(repeat), &mut dirty);
        app.handle(Event::Key(key), &mut dirty);
        assert!(matches!(requests.try_recv(), Err(TryRecvError::Empty)));
        events.send(WorkerEvent::ThinkLevelSet(Ok(level))).unwrap();
        app.receive();
        assert_eq!(app.think_level, level);
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
            protocol: crate::models::Protocol::Completion,
            thinking: crate::models::ModelThinking::default(),
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
        State::SettingThinkLevel,
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
        protocol: crate::models::Protocol::Completion,
        thinking: crate::models::ModelThinking::default(),
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
    let mut app = app();
    app.state = State::Ready;
    app.think_level = ThinkLevel::High;
    let (sender, requests) = mpsc::channel();
    app.commands = Some(sender);
    let (events, receiver) = mpsc::channel();
    app.events = Some(receiver);
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
    assert!(matches!(
        requests.try_recv(),
        Ok(WorkerCommand::SetThinkLevel(ThinkLevel::Max))
    ));
    assert_eq!(app.think_level, ThinkLevel::High);
    assert!(app.menu.is_none());
    app.input.insert_str("新草稿");
    events
        .send(WorkerEvent::ThinkLevelSet(Ok(ThinkLevel::Max)))
        .unwrap();
    app.receive();
    assert_eq!(app.think_level, ThinkLevel::Max);
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
            protocol: crate::models::Protocol::Completion,
            thinking: crate::models::ModelThinking::default(),
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
