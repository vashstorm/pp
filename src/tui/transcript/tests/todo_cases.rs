use super::*;

#[test]
fn tui_todo_snapshots_never_create_history_cards() {
    let mut transcript = Transcript::new();
    for status in [
        crate::plan::PlanStatus::Draft,
        crate::plan::PlanStatus::Completed,
    ] {
        transcript.progress(todo_snapshot_event(3, todo_phases(), status));
    }
    transcript.progress(AgentEvent::TodoStateUpdated {
        state: crate::events::TodoState {
            phases: todo_phases(),
            plan: None,
        },
    });
    assert!(transcript.entries.is_empty());
    transcript.replay(vec![ReplayRecord::Event(todo_snapshot_event(
        1,
        todo_phases(),
        crate::plan::PlanStatus::Executed,
    ))]);
    assert!(transcript.entries.is_empty());
}

#[test]
fn tui_todo_tool_result_keeps_error_without_duplicate_list() {
    let mut transcript = Transcript::new();
    tool(&mut transcript, "todo-1", "todo", json!({"task":"A"}));
    // 开始时无法推断 op，标题保持 Todo。
    transcript.progress(AgentEvent::ToolFinished {
            id: "todo-1".into(),
            result: json!({
                "ok": false,
                "op": "done",
                "error": "Plan tasks changed; re-plan before completing.",
                "content": "Errors: Plan tasks changed; re-plan before completing.\nOverall: 0/2 done, 2 open.",
                "phases": todo_phases(),
            }),
        });
    // 标题从结果修正为真实操作。
    assert_eq!(
        transcript.entries[0].title,
        format!("{} Todo · done", palette::tool_icon("todo"))
    );
    let text = entry_text(&mut transcript.entries[0], 100);
    assert!(text.contains("Plan tasks changed"), "{text}");
    assert!(!text.contains("I. Write"), "{text}");
    assert!(!text.contains("Beta"), "{text}");
}

#[test]
fn tui_todo_success_keeps_only_operation_title() {
    let mut transcript = Transcript::new();
    // 空清单与旧格式成功结果均不在历史中展示正文。
    tool(&mut transcript, "todo-1", "todo", json!({"op":"view"}));
    transcript.progress(AgentEvent::ToolFinished {
        id: "todo-1".into(),
        result: json!({"ok":true,"op":"view","content":"Todo list is empty.","phases":[]}),
    });
    let text = entry_text(&mut transcript.entries[0], 100);
    assert!(!text.contains("Todo list is empty."), "{text}");
    assert!(text.contains("Todo · view"), "{text}");

    // 旧格式缺 phases 也不能把历史正文当作当前权威。
    tool(&mut transcript, "todo-2", "todo", json!({"op":"view"}));
    transcript.progress(AgentEvent::ToolFinished {
        id: "todo-2".into(),
        result: json!({"ok":true,"op":"view","content":"fallback body"}),
    });
    let text = entry_text(&mut transcript.entries[1], 100);
    assert!(!text.contains("fallback body"), "{text}");

    // 完全缺失 content：退回 error 诊断。
    tool(&mut transcript, "todo-3", "todo", json!({"op":"done"}));
    transcript.progress(AgentEvent::ToolFinished {
        id: "todo-3".into(),
        result: json!({"ok":false,"error":"todo failed clearly"}),
    });
    let text = entry_text(&mut transcript.entries[2], 100);
    assert!(text.contains("todo failed clearly"), "{text}");
}

#[test]
fn tui_todo_reminder_note_lists_open_items_only() {
    use crate::tools::todo::{TodoItem, TodoStatus};
    let mut transcript = Transcript::new();
    transcript.progress(AgentEvent::TodoReminder {
        todos: vec![
            TodoItem {
                content: "left".into(),
                status: TodoStatus::InProgress,
                blocker: None,
            },
            TodoItem {
                content: "stuck".into(),
                status: TodoStatus::Blocked,
                blocker: Some("wait".into()),
            },
            TodoItem {
                content: "fin".into(),
                status: TodoStatus::Completed,
                blocker: None,
            },
            TodoItem {
                content: "gone".into(),
                status: TodoStatus::Abandoned,
                blocker: None,
            },
        ],
        attempt: 2,
        max_attempts: 3,
    });
    let text = entry_text(&mut transcript.entries[0], 100);
    assert!(
        text.contains("Todo reminder 2/3: 2 incomplete todo item(s):"),
        "{text}"
    );
    assert!(text.contains("- left"), "{text}");
    assert!(text.contains("- stuck"), "{text}");
    assert!(!text.contains("fin"), "{text}");
    assert!(!text.contains("gone"), "{text}");
}

#[test]
fn tui_todo_reminder_replays_as_note_only() {
    use crate::tools::todo::{TodoItem, TodoStatus};
    let mut transcript = Transcript::new();
    let error = transcript.replay(vec![
        ReplayRecord::User("go".into()),
        ReplayRecord::Event(AgentEvent::TodoReminder {
            todos: vec![TodoItem {
                content: "again".into(),
                status: TodoStatus::Pending,
                blocker: None,
            }],
            attempt: 1,
            max_attempts: 3,
        }),
    ]);
    assert!(error.is_none());
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(text.contains("Todo reminder 1/3"), "{text}");
    assert!(text.contains("again"), "{text}");
}
