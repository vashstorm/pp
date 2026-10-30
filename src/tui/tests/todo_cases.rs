use super::*;
use crate::{
    events::TodoState,
    tools::todo::{TodoItem, TodoPhase, TodoStatus},
};

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
fn tui_todo_fixed_position_survives_streaming_and_input_edits() {
    let mut app = app();
    install_todos(&mut app, 8);
    app.input.insert_str("保留输入草稿");
    for i in 0..30 {
        app.append("Assistant", &format!("STREAM_{i}"));
    }
    for native in [false, true] {
        app.native_render = native;
        let text = draw(&mut app, 100, 24);
        assert!(
            text.lines().nth(16).unwrap().contains("TODO · 0/8"),
            "{text}"
        );
        assert!(text.lines().nth(17).unwrap().contains("TODO_TASK_00"));
        assert!(text.lines().nth(18).unwrap().contains("TODO_TASK_01"));
        assert!(text.lines().nth(19).unwrap().starts_with('└'));
        assert!(
            text.lines()
                .nth(21)
                .unwrap()
                .replace(' ', "")
                .contains("保留输入草稿")
        );
        assert_eq!(
            app.main_layout(Rect::new(0, 0, 100, 24))
                .conversation
                .height,
            16
        );
    }
    app.input.insert_newline();
    app.input.insert_str("第二行");
    let text = draw(&mut app, 100, 24);
    assert!(text.lines().nth(15).unwrap().contains("TODO · 0/8"));
    assert!(text.replace(' ', "").contains("第二行"));
}

#[test]
fn tui_todo_shortcuts_and_wheel_preserve_input_and_conversation() {
    let mut app = app();
    install_todos(&mut app, 30);
    app.input.insert_str("keep draft");
    let mut dirty = false;
    app.handle(function_key('t'), &mut dirty);
    assert!(app.todo_panel.expanded);
    draw(&mut app, 100, 24);
    let area = app.todo_panel.area;
    assert_eq!(area.height, 8);
    app.handle(
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: area.x,
            row: area.y + 2,
            modifiers: KeyModifiers::NONE,
        }),
        &mut dirty,
    );
    let text = draw(&mut app, 100, 24);
    assert!(!text.contains("TODO_TASK_00"));
    assert!(!app.history_browser);
    app.handle(
        Event::Key(KeyEvent::new(KeyCode::PageDown, FUNCTION_MODIFIER)),
        &mut dirty,
    );
    let text = draw(&mut app, 100, 24);
    assert!(text.contains("TODO_TASK_10"));
    assert!(!app.history_browser);
    app.handle(key(KeyCode::Char('x')), &mut dirty);
    assert_eq!(app.input.lines(), &["keep draftx"]);
    app.handle(control('o'), &mut dirty);
    assert!(app.todo_panel.expanded);
    app.handle(function_key('t'), &mut dirty);
    assert!(!app.todo_panel.expanded);
}

#[test]
fn tui_todo_ignores_uncommitted_results_and_restores_workflow() {
    let mut app = app();
    install_todos(&mut app, 2);
    app.receive_event_for_test(WorkerEvent::Progress(AgentEvent::ToolStarted {
        id: "init".into(),
        name: "todo".into(),
        arguments: "{\"op\":\"init\"}".into(),
    }));
    app.receive_event_for_test(WorkerEvent::Progress(AgentEvent::ToolFinished {
        id: "init".into(),
        result: serde_json::json!({"ok":true,"op":"init","phases":todo_state(10).phases}),
    }));
    assert!(draw(&mut app, 100, 24).contains("TODO · 0/2"));
    app.todo_panel.toggle();
    let workflow = WorkflowState {
        todos: todo_state(5).phases,
        ..WorkflowState::default()
    };
    app.set_workflow(workflow);
    app.adopt_history(Vec::new());
    assert!(!app.todo_panel.expanded);
    assert!(draw(&mut app, 100, 24).contains("TODO · 0/5"));
    app.receive_event_for_test(WorkerEvent::Reset);
    assert!(!draw(&mut app, 100, 24).contains("TODO ·"));
}

#[test]
fn tui_todo_ask_and_menus_keep_priority_and_no_overlap() {
    let mut app = app();
    install_todos(&mut app, 10);
    let (_reply, token) = ask_fixture(&mut app);
    app.todo_panel.toggle();
    let text = draw(&mut app, 100, 30);
    assert!(text.contains("TODO · 0/10"));
    assert!(
        text.contains("SQLite") && text.contains("Postgres"),
        "{text}"
    );
    let layout = app.main_layout(Rect::new(0, 0, 100, 30));
    assert!(app.todo_panel.area.y >= layout.conversation.bottom());
    let mut dirty = false;
    app.handle(function_key('t'), &mut dirty);
    assert!(app.todo_panel.expanded);
    app.handle(key(KeyCode::Esc), &mut dirty);
    assert!(token.is_cancelled());
    assert!(app.todo_panel.expanded);
    app.close_ask_on_exit();
    app.state = State::Ready;
    app.input.insert_str("/");
    app.refresh_candidates(true);
    draw(&mut app, 100, 30);
    assert!(app.menu.is_some());
    app.handle(function_key('t'), &mut dirty);
    assert!(app.todo_panel.expanded);
}

#[test]
fn tui_todo_tiny_layout_never_overlaps_editor_and_status() {
    let mut app = app();
    install_todos(&mut app, 10);
    app.todo_panel.toggle();
    for width in [1, 2, 12, 80] {
        for height in 1..15 {
            draw(&mut app, width, height);
            let layout = app.main_layout(Rect::new(0, 0, width, height));
            assert!(layout.conversation.bottom() <= app.todo_panel.area.y);
            assert!(app.todo_panel.area.bottom() <= height);
            assert!(app.todo_panel.area.height <= (height / 3).max(1));
        }
    }
}

#[test]
fn tui_todo_plan_hides_legacy_cache_and_deletion_clears_bound_state() {
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
    assert!(app.workflow.plan.is_none());
    assert!(app.plan_panel.is_none());
    assert_eq!(app.plan_progress, (0, 0));
    assert!(!draw(&mut app, 100, 24).contains("TODO ·"));
}
