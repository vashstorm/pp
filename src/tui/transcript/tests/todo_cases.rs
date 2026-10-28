use super::*;

#[test]
fn tui_todo_snapshot_renders_full_static_card() {
    let mut transcript = Transcript::new();
    transcript.progress(todo_snapshot_event(
        3,
        todo_phases(),
        crate::plan::PlanStatus::Draft,
    ));
    let text = entry_text(&mut transcript.entries[0], 100);
    assert!(
        text.contains("Plan tasks · Draft · r3 · Preview (not executing)"),
        "{text}"
    );
    assert!(text.contains("I. Write (1/2)"), "{text}");
    assert!(text.contains("II. Verify (1/3)"), "{text}");
    assert!(text.contains("☑ Alpha"), "{text}");
    assert!(text.contains("Beta (in progress)"), "{text}");
    assert!(text.contains("☐ Check"), "{text}");
    assert!(text.contains("(blocked: wait up)"), "{text}");
    assert!(text.contains("Skipped (dropped)"), "{text}");

    // 结束计划不得误标成待执行草稿。
    let mut transcript = Transcript::new();
    transcript.progress(todo_snapshot_event(
        4,
        todo_phases(),
        crate::plan::PlanStatus::Completed,
    ));
    let text = entry_text(&mut transcript.entries[0], 100);
    assert!(text.contains("Plan tasks · Completed · r4"), "{text}");
    assert!(!text.contains("Preview"), "{text}");
}

#[test]
fn tui_todo_duplicate_tasks_get_phase_occurrence_hint() {
    let mut transcript = Transcript::new();
    transcript.progress(todo_snapshot_event(
        1,
        todo_phases(),
        crate::plan::PlanStatus::Ready,
    ));
    let text = entry_text(&mut transcript.entries[0], 120);
    assert!(text.contains("phase \"Verify\" occurrence 1"), "{text}");
    assert!(text.contains("phase \"Verify\" occurrence 2"), "{text}");
    // 非重复任务没有寻址提示。
    assert!(!text.contains("Alpha ·"), "{text}");
    assert!(!text.contains("Beta ·"), "{text}");
}

#[test]
fn tui_todo_full_card_survives_narrow_widths_without_loss() {
    let mut transcript = Transcript::new();
    transcript.progress(todo_snapshot_event(
        2,
        todo_phases(),
        crate::plan::PlanStatus::Executed,
    ));
    // 结构化分支不受窗口截断影响：极窄宽度下全部任务仍可见。
    let text = entry_text(&mut transcript.entries[0], 12);
    assert!(text.contains("Alpha"), "{text}");
    assert!(text.contains("Beta"), "{text}");
    assert!(text.contains("Check"), "{text}");
    assert!(text.contains("Skipped"), "{text}");
    assert!(text.contains("Verify"), "{text}");
    // Ctrl+O/完整模式渲染同一份完整列表，不额外重复 summary。
    transcript.entries[0].expanded = true;
    let expanded = entry_text(&mut transcript.entries[0], 100);
    assert_eq!(
        expanded.matches("Alpha").count(),
        1,
        "summary must not duplicate the structured list: {expanded}"
    );
}

#[test]
fn tui_todo_dynamic_text_is_display_safe() {
    use crate::tools::todo::{TodoItem, TodoPhase, TodoStatus};
    let mut transcript = Transcript::new();
    transcript.progress(todo_snapshot_event(
        1,
        vec![TodoPhase {
            name: "验证".into(),
            tasks: vec![TodoItem {
                content: "读取\u{1b}[31m状态\u{7f}\t列".into(),
                status: TodoStatus::Blocked,
                blocker: Some("原因\u{2}含控制".into()),
            }],
        }],
        crate::plan::PlanStatus::Executed,
    ));
    let text = entry_text(&mut transcript.entries[0], 60);
    assert!(text.contains("验证"), "{text}");
    assert!(text.contains("读取"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");
    assert!(!text.contains('\u{7f}'), "{text}");
    assert!(!text.contains('\u{2}'), "{text}");
    assert!(text.contains("(blocked: 原因\u{fffd}含控制)"), "{text}");
}

#[test]
fn tui_todo_tool_result_shows_error_before_trusted_list() {
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
    let error_at = text
        .find("Plan tasks changed")
        .expect("diagnostic must render");
    let list_at = text.find("I. Write").expect("checklist must render");
    assert!(error_at < list_at, "{text}");
    assert!(text.contains("Beta (in progress)"), "{text}");
}

#[test]
fn tui_todo_damaged_or_empty_falls_back_to_readable_text() {
    let mut transcript = Transcript::new();
    // 空清单显示空清单文本。
    tool(&mut transcript, "todo-1", "todo", json!({"op":"view"}));
    transcript.progress(AgentEvent::ToolFinished {
        id: "todo-1".into(),
        result: json!({"ok":true,"op":"view","content":"Todo list is empty.","phases":[]}),
    });
    let text = entry_text(&mut transcript.entries[0], 100);
    assert!(text.contains("Todo list is empty."), "{text}");

    // 旧/损坏数据缺 phases：降级为可读 content。
    tool(&mut transcript, "todo-2", "todo", json!({"op":"view"}));
    transcript.progress(AgentEvent::ToolFinished {
        id: "todo-2".into(),
        result: json!({"ok":true,"op":"view","content":"fallback body"}),
    });
    let text = entry_text(&mut transcript.entries[1], 100);
    assert!(text.contains("fallback body"), "{text}");

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
    let mut input_history = Vec::new();
    let error = transcript.replay(
        vec![
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
        ],
        &mut input_history,
    );
    assert!(error.is_none());
    let text = screen(&render(&mut transcript, 100, 30));
    assert!(text.contains("Todo reminder 1/3"), "{text}");
    assert!(text.contains("again"), "{text}");
}
