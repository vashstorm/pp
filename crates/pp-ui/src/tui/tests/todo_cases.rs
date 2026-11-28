use super::*;
use pp_core::events::TodoState;
use pp_core::todo::TodoItem;
use pp_core::todo::TodoPhase;
use pp_core::todo::TodoStatus;

fn todo_state(count: usize) -> TodoState {
    TodoState {
        phases: vec![TodoPhase {
            name: "Stage".into(),
            tasks: (0..count)
                .map(|i| TodoItem {
                    content: format!("TODO_TASK_{i:02}"),
                    status: if i == 0 {
                        TodoStatus::InProgress
                    } else {
                        TodoStatus::Pending
                    },
                    blocker: None,
                })
                .collect(),
        }],
        plan: None,
    }
}

fn install_todos(app: &mut App, count: usize) {
    app.show_welcome = false;
    app.receive_event_for_test(WorkerEvent::Progress(AgentEvent::TodoStateUpdated {
        state: todo_state(count),
    }));
}

#[test]
fn tui_todo_fixed_position_shortcuts_and_workflow() {
    let mut app = app();
    install_todos(&mut app, 8);
    app.input.insert_str("保留输入草稿");
    for i in 0..30 {
        app.append("Assistant", &format!("STREAM_{i}"));
    }
    for native in [false, true] {
        app.native_render = native;
        let text = draw(&mut app, 100, 24);
        let lines = text.lines().collect::<Vec<_>>();
        let title = lines
            .iter()
            .position(|line| line.contains("TODO · 0/8"))
            .unwrap();
        assert!(lines[title - 1].trim().is_empty(), "{text}");
        assert!(lines[title + 1].contains("I. Stage (0/8)"), "{text}");
        for offset in 0..3 {
            assert!(
                lines[title + 2 + offset].contains(&format!("TODO_TASK_{offset:02}")),
                "{text}"
            );
        }
        assert!(
            lines
                .iter()
                .skip(title)
                .any(|line| line.replace(' ', "").contains("保留输入草稿")),
            "{text}"
        );
        assert_eq!(
            app.main_layout(Rect::new(0, 0, 100, 24))
                .conversation
                .height,
            14
        );
    }
    app.input.insert_newline();
    app.input.insert_str("第二行");
    let text = draw(&mut app, 100, 24);
    let lines = text.lines().collect::<Vec<_>>();
    let title = lines
        .iter()
        .position(|line| line.contains("TODO · 0/8"))
        .unwrap();
    assert!(lines[title - 1].trim().is_empty() && text.replace(' ', "").contains("第二行"));

    let mut sc = super::app();
    install_todos(&mut sc, 30);
    sc.input.insert_str("keep draft");
    let mut dirty = false;
    sc.handle(function_key('t'), &mut dirty);
    assert!(sc.todo_panel.expanded);
    draw(&mut sc, 100, 24);
    let area = sc.todo_panel.area;
    assert_eq!(area.height, 8);
    sc.handle(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: area.x,
            row: area.y + 2,
            modifiers: KeyModifiers::NONE,
        }),
        &mut dirty,
    );
    let text = draw(&mut sc, 100, 24);
    assert!(!text.contains("TODO_TASK_00") && !sc.history_browser);
    sc.handle(
        Event::Key(KeyEvent::new(KeyCode::PageDown, FUNCTION_MODIFIER)),
        &mut dirty,
    );
    assert!(draw(&mut sc, 100, 24).contains("TODO_TASK_10") && !sc.history_browser);
    sc.handle(key(KeyCode::Char('x')), &mut dirty);
    assert_eq!(sc.input.lines(), &["keep draftx"]);
    sc.handle(control('o'), &mut dirty);
    assert!(sc.todo_panel.expanded);
    sc.handle(function_key('t'), &mut dirty);
    assert!(!sc.todo_panel.expanded);

    let mut wf = super::app();
    install_todos(&mut wf, 2);
    wf.receive_event_for_test(WorkerEvent::Progress(AgentEvent::ToolStarted {
        id: "init".into(),
        name: "todo".into(),
        arguments: "{\"op\":\"init\"}".into(),
    }));
    wf.receive_event_for_test(WorkerEvent::Progress(AgentEvent::ToolFinished {
        id: "init".into(),
        result: serde_json::json!({"ok":true,"op":"init","phases":todo_state(10).phases}),
    }));
    assert!(draw(&mut wf, 100, 24).contains("TODO · 0/2"));
    wf.todo_panel.toggle();
    let workflow = WorkflowState {
        todos: todo_state(5).phases,
        ..WorkflowState::default()
    };
    wf.set_workflow(workflow);
    wf.adopt_history(Vec::new());
    assert!(!wf.todo_panel.expanded && draw(&mut wf, 100, 24).contains("TODO · 0/5"));
    wf.receive_event_for_test(WorkerEvent::Reset);
    assert!(!draw(&mut wf, 100, 24).contains("TODO ·"));
}

#[test]
fn tui_todo_menus_ask_and_tiny_layout() {
    let mut app = app();
    install_todos(&mut app, 10);
    let (_reply, token) = ask_fixture(&mut app);
    app.todo_panel.toggle();
    let text = draw(&mut app, 100, 30);
    assert!(text.contains("TODO · 0/10") && text.contains("SQLite") && text.contains("Postgres"));
    let layout = app.main_layout(Rect::new(0, 0, 100, 30));
    assert!(app.todo_panel.area.y >= layout.conversation.bottom());
    let mut dirty = false;
    app.handle(function_key('t'), &mut dirty);
    assert!(app.todo_panel.expanded);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(token.is_cancelled() && app.todo_panel.expanded);
    app.close_ask_on_exit();
    app.state = State::Ready;
    app.input.insert_str("/");
    app.refresh_candidates(true);
    draw(&mut app, 100, 30);
    assert!(app.menu.is_some());
    app.handle(function_key('t'), &mut dirty);
    assert!(app.todo_panel.expanded);

    for (width, height) in [(100, 30), (18, 7)] {
        let mut comp = super::app();
        comp.state = State::Ready;
        install_todos(&mut comp, 10);
        comp.todo_panel.toggle();
        comp.handle(Event::Paste("/du".into()), &mut dirty);
        let commands = draw(&mut comp, width, height);
        let input_row = commands
            .lines()
            .position(|line| line.trim_end() == "❯ /du")
            .unwrap();
        assert!(
            commands
                .lines()
                .nth(input_row - 2)
                .unwrap()
                .starts_with("❯ /dump")
        );
        comp.handle(key(KeyCode::Tab), &mut dirty);
        comp.handle(key(KeyCode::Char(' ')), &mut dirty);
        comp.handle(key(KeyCode::Down), &mut dirty);
        let arguments = draw(&mut comp, width, height);
        let rows = arguments.lines().collect::<Vec<_>>();
        assert_eq!(rows[input_row].trim_end(), "❯ /dump");
        assert!(rows[input_row - 2].starts_with("❯ result"), "{arguments}");
        assert!(rows[..input_row - 2].iter().any(|row| row.contains("TODO")));
        if height == 30 {
            assert!(rows[input_row - 3].trim_start().starts_with("all "));
            assert!(arguments.contains("TODO_TASK_00"));
        } else {
            assert!(!rows[input_row - 3].trim_start().starts_with("all "));
            let resized = draw(&mut comp, 100, 30);
            let ri = resized
                .lines()
                .position(|line| line.trim_end() == "❯ /dump")
                .unwrap();
            assert!(
                resized
                    .lines()
                    .nth(ri - 3)
                    .unwrap()
                    .trim_start()
                    .starts_with("all ")
            );
            assert!(resized.lines().nth(ri - 2).unwrap().starts_with("❯ result"));
            draw(&mut comp, width, height);
        }
        let area = Rect::new(0, 0, width, height);
        let open = comp.main_layout(area);
        let open_h = comp.todo_panel.area.height;
        comp.handle(key(KeyCode::Esc), &mut dirty);
        let dismissed = draw(&mut comp, width, height);
        let closed = comp.main_layout(area);
        assert_eq!(comp.raw_input_text(), "/dump ");
        assert!(comp.todo_panel.expanded && !dismissed.contains("❯ result"));
        assert_eq!(open.input, closed.input);
        assert_eq!(arguments.lines().last(), dismissed.lines().last());
        if height == 30 {
            assert!(closed.conversation.height > open.conversation.height);
        } else {
            assert_eq!(open.conversation.height, 1);
            assert!(comp.todo_panel.area.height > open_h);
        }
    }

    let mut tiny = super::app();
    install_todos(&mut tiny, 10);
    tiny.todo_panel.toggle();
    for width in [1, 2, 12, 80] {
        for height in 1..15 {
            draw(&mut tiny, width, height);
            let layout = tiny.main_layout(Rect::new(0, 0, width, height));
            assert!(layout.conversation.bottom() <= tiny.todo_panel.area.y);
            assert!(tiny.todo_panel.area.bottom() <= height);
            assert!(tiny.todo_panel.area.height <= (height / 3).max(1));
        }
    }
}

#[test]
fn tui_todo_plan_cache_and_incoming_ask() {
    let mut app = app();
    let mut workflow = ready_plan_workflow();
    workflow.todos = todo_state(2).phases;
    workflow.todo_plan_id = Some(workflow.plan.as_ref().unwrap().id.clone());
    app.set_workflow(workflow.clone());
    app.show_welcome = false;
    assert!(!draw(&mut app, 100, 24).contains("TODO ·"));
    workflow.mode = WorkMode::Execute;
    workflow.plan.as_mut().unwrap().status = PlanStatus::Executed;
    app.set_workflow(workflow);
    assert!(draw(&mut app, 100, 24).contains("TODO · 0/2"));
    app.open_plan_progress_view();
    assert!(app.plan_panel.is_some());
    app.receive_event_for_test(WorkerEvent::Workflow(WorkflowState {
        mode: WorkMode::Plan,
        ..Default::default()
    }));
    assert!(app.workflow.plan.is_none() && app.plan_panel.is_none());
    assert_eq!(app.plan_progress, (0, 0));
    assert!(!draw(&mut app, 100, 24).contains("TODO ·"));

    let mut incoming = super::app();
    install_todos(&mut incoming, 10);
    incoming.model_panel = Some(model::Panel::new("", 25));
    incoming.refresh_candidates(true);
    let (_reply, token) = ask_fixture(&mut incoming);
    let text = draw(&mut incoming, 100, 30);
    assert!(
        text.contains("Choose storage") && text.contains("SQLite") && text.contains("Postgres")
    );
    incoming.handle(key(KeyCode::Esc), &mut false);
    assert!(token.is_cancelled() && incoming.model_panel.is_some());
}
