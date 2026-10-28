use super::plan_adapter::plan_task_targets;
use super::*;

#[test]
fn plan_todo_target_limits_apply_after_phase_filter_even_for_unique_tasks() {
    let current = phases(&[
        ("First", &[("same", TodoStatus::InProgress)]),
        (
            "Second",
            &[
                ("same", TodoStatus::Pending),
                ("unique", TodoStatus::Pending),
            ],
        ),
    ]);
    for args in [
        r#"{"op":"done","task":"unique","occurrence":2}"#,
        r#"{"op":"done","task":"unique","phase":"First"}"#,
        r#"{"op":"done","task":"same","phase":"First","occurrence":2}"#,
        r#"{"op":"done","task":""}"#,
    ] {
        let params = resolve_params(args, true).unwrap();
        let mut errors = Vec::new();
        assert!(plan_task_targets(&current, &params, &mut errors).is_empty());
        assert!(
            errors.iter().any(|error| error.contains("not found")),
            "{errors:?}"
        );
    }
    let params = resolve_params(
        r#"{"op":"done","task":"same","phase":"Second","occurrence":1}"#,
        true,
    )
    .unwrap();
    let mut errors = Vec::new();
    assert_eq!(
        plan_task_targets(&current, &params, &mut errors),
        vec![TaskOrdinal { phase: 1, task: 0 }]
    );
    assert!(errors.is_empty());
}

fn item(content: &str, status: TodoStatus) -> TodoItem {
    TodoItem {
        content: content.to_owned(),
        status,
        blocker: None,
    }
}

fn phase(name: &str, tasks: &[(&str, TodoStatus)]) -> TodoPhase {
    TodoPhase {
        name: name.to_owned(),
        tasks: tasks
            .iter()
            .map(|(content, status)| item(content, *status))
            .collect(),
    }
}

fn phases(list: &[(&str, &[(&str, TodoStatus)])]) -> Vec<TodoPhase> {
    list.iter()
        .map(|(name, tasks)| phase(name, tasks))
        .collect()
}

fn init_args(entries: &[(&str, &[&str])]) -> String {
    let list: Vec<Value> = entries
        .iter()
        .map(|(name, items)| json!({"phase": name, "items": items}))
        .collect();
    json!({"op": "init", "list": list}).to_string()
}

/// init with fresh state; first task of the first phase becomes active.
fn inited(entries: &[(&str, &[&str])]) -> Vec<TodoPhase> {
    let (value, change) = run(&[], &init_args(entries));
    assert_eq!(value["ok"], json!(true), "{value}");
    change.expect("init must return a change").phases
}

fn status_of(phases: &[TodoPhase], content: &str) -> TodoStatus {
    phases
        .iter()
        .flat_map(|phase| phase.tasks.iter())
        .find(|task| task.content == content)
        .unwrap_or_else(|| panic!("task {content} missing"))
        .status
}

fn blocked_reason(phases: &[TodoPhase], content: &str) -> Option<String> {
    phases
        .iter()
        .flat_map(|phase| phase.tasks.iter())
        .find(|task| task.content == content)
        .unwrap_or_else(|| panic!("task {content} missing"))
        .blocker
        .clone()
}

#[test]
fn todo_init_activates_first_task_and_reports_change() {
    let (value, change) = run(
        &[],
        &init_args(&[("First", &["A", "B"]), ("Second", &["C"])]),
    );
    assert_eq!(value["ok"], json!(true));
    assert_eq!(value["op"], json!("init"));
    assert!(
        value["content"]
            .as_str()
            .unwrap()
            .contains("Remaining items (3):")
    );
    let change = change.expect("init must return a change");
    assert_eq!(
        value["phases"],
        serde_json::to_value(&change.phases).unwrap()
    );
    assert_eq!(status_of(&change.phases, "A"), TodoStatus::InProgress);
    assert_eq!(status_of(&change.phases, "B"), TodoStatus::Pending);
    assert_eq!(status_of(&change.phases, "C"), TodoStatus::Pending);
}

#[test]
fn todo_done_keeps_active_until_it_finishes_then_advances() {
    let current = inited(&[("First", &["A", "B"]), ("Second", &["C"])]);
    let (value, change) = run(&current, r#"{"op":"done","task":"B"}"#);
    assert_eq!(value["ok"], json!(true));
    let next = change.unwrap().phases;
    assert_eq!(status_of(&next, "B"), TodoStatus::Completed);
    assert_eq!(status_of(&next, "A"), TodoStatus::InProgress);

    let (value, change) = run(&next, r#"{"op":"done","task":"A"}"#);
    assert_eq!(value["ok"], json!(true));
    let next = change.unwrap().phases;
    assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
    assert_eq!(status_of(&next, "B"), TodoStatus::Completed);
    // No active left: earliest pending starts; completed stays closed.
    assert_eq!(status_of(&next, "C"), TodoStatus::InProgress);
}

#[test]
fn todo_done_without_target_completes_everything() {
    let current = inited(&[("First", &["A"]), ("Second", &["C"])]);
    let (_, change) = run(&current, r#"{"op":"done"}"#);
    let next = change.unwrap().phases;
    assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
    assert_eq!(status_of(&next, "C"), TodoStatus::Completed);
}

#[test]
fn todo_done_phase_targets_whole_phase() {
    let current = inited(&[("First", &["A", "B"]), ("Second", &["C"])]);
    let (_, change) = run(&current, r#"{"op":"done","phase":"First"}"#);
    let next = change.unwrap().phases;
    assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
    assert_eq!(status_of(&next, "B"), TodoStatus::Completed);
    assert_eq!(status_of(&next, "C"), TodoStatus::InProgress);
}

#[test]
fn todo_start_moves_other_active_to_pending_and_reopens() {
    let current = inited(&[("First", &["A", "B"]), ("Second", &["C"])]);
    let (_, change) = run(&current, r#"{"op":"start","task":"C"}"#);
    let next = change.unwrap().phases;
    assert_eq!(status_of(&next, "C"), TodoStatus::InProgress);
    assert_eq!(status_of(&next, "A"), TodoStatus::Pending);

    // Explicit re-open of a closed task is allowed upstream.
    let (_, change) = run(&next, r#"{"op":"done","task":"C"}"#);
    let next = change.unwrap().phases;
    let (_, change) = run(&next, r#"{"op":"start","task":"C"}"#);
    let next = change.unwrap().phases;
    assert_eq!(status_of(&next, "C"), TodoStatus::InProgress);
}

#[test]
fn todo_start_requires_single_task_target() {
    let current = inited(&[("First", &["A"])]);
    for arguments in [r#"{"op":"start"}"#, r#"{"op":"start","phase":"First"}"#] {
        let (value, change) = run(&current, arguments);
        assert_eq!(value["ok"], json!(false), "{value}");
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .contains("Missing task content")
        );
        assert!(change.is_none());
    }
}

#[test]
fn todo_drop_abandons_and_counts_closed() {
    let current = inited(&[("First", &["A", "B"])]);
    let (value, change) = run(&current, r#"{"op":"drop","task":"B"}"#);
    assert_eq!(value["ok"], json!(true));
    let next = change.unwrap().phases;
    assert_eq!(status_of(&next, "B"), TodoStatus::Abandoned);
    let content = value["content"].as_str().unwrap();
    assert!(content.contains("Overall: 1/2 done, 1 open."));
    assert!(content.contains("    - [ ] B (dropped)"));
}

#[test]
fn todo_rm_clears_tasks_but_keeps_phase_objects() {
    let current = inited(&[("First", &["A", "B"]), ("Second", &["C"])]);
    let (_, change) = run(&current, r#"{"op":"rm","phase":"First"}"#);
    let next = change.unwrap().phases;
    assert_eq!(next.len(), 2);
    assert!(next[0].tasks.is_empty());
    assert_eq!(next[1].tasks.len(), 1);

    let (_, change) = run(&next, r#"{"op":"rm"}"#);
    let next = change.unwrap().phases;
    assert_eq!(next.len(), 2);
    assert!(next.iter().all(|phase| phase.tasks.is_empty()));
    // Empty after mutation reads as cleared.
    assert_eq!(next.len(), 2);
    let (value, change) = run(&next, r#"{"op":"view"}"#);
    assert_eq!(value["content"], json!("Todo list is empty."));
    assert!(change.is_none());
}

#[test]
fn todo_block_rules() {
    let current = inited(&[("Tasks", &["A", "B"])]);
    // Whitespace runs collapse; the blocker note rides on one line.
    let (value, change) = run(
        &current,
        r#"{"op":"block","task":"B","reason":"wait\n  on\tx"}"#,
    );
    assert_eq!(value["ok"], json!(true));
    let next = change.unwrap().phases;
    assert_eq!(status_of(&next, "B"), TodoStatus::Blocked);
    assert_eq!(blocked_reason(&next, "B").as_deref(), Some("wait on x"));
    // Active task untouched by blocking another.
    assert_eq!(status_of(&next, "A"), TodoStatus::InProgress);

    // Completing the active task does not auto-start the blocked one.
    let (_, change) = run(&next, r#"{"op":"done","task":"A"}"#);
    let next = change.unwrap().phases;
    assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
    assert_eq!(status_of(&next, "B"), TodoStatus::Blocked);

    // Re-blocking a blocked task refines its note.
    let (_, change) = run(
        &next,
        r#"{"op":"block","task":"B","reason":"still waiting"}"#,
    );
    let next = change.unwrap().phases;
    assert_eq!(blocked_reason(&next, "B").as_deref(), Some("still waiting"));

    // Blocking without a reason clears an existing note (upstream keeps).
    let (_, change) = run(&next, r#"{"op":"block","task":"B"}"#);
    let next = change.unwrap().phases;
    assert_eq!(blocked_reason(&next, "B"), None);

    // Unblock returns pending and the cleared task becomes active.
    let (_, change) = run(&next, r#"{"op":"unblock","task":"B"}"#);
    let next = change.unwrap().phases;
    assert_eq!(status_of(&next, "B"), TodoStatus::InProgress);
    assert_eq!(blocked_reason(&next, "B"), None);
}

#[test]
fn todo_block_phase_never_reopens_closed_tasks() {
    let current = inited(&[("First", &["A", "B"])]);
    let (_, change) = run(&current, r#"{"op":"done","task":"A"}"#);
    let next = change.unwrap().phases;
    let (_, change) = run(&next, r#"{"op":"block","phase":"First","reason":"halt"}"#);
    let next = change.unwrap().phases;
    assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
    assert_eq!(status_of(&next, "B"), TodoStatus::Blocked);
}

#[test]
fn todo_unblock_touches_only_blocked_tasks() {
    let current = phases(&[(
        "Tasks",
        &[("A", TodoStatus::InProgress), ("B", TodoStatus::Blocked)],
    )]);
    let (_, change) = run(&current, r#"{"op":"unblock","phase":"Tasks"}"#);
    let next = change.unwrap().phases;
    assert_eq!(status_of(&next, "A"), TodoStatus::InProgress);
    assert_eq!(status_of(&next, "B"), TodoStatus::Pending);
    assert_eq!(blocked_reason(&next, "B"), None);
}

#[test]
fn todo_block_unblock_require_target() {
    let current = inited(&[("Tasks", &["A"])]);
    for arguments in [r#"{"op":"block"}"#, r#"{"op":"unblock"}"#] {
        let (value, change) = run(&current, arguments);
        assert_eq!(value["ok"], json!(false), "{value}");
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .ends_with("requires a task or phase target")
        );
        assert!(change.is_none());
    }
}

#[test]
fn todo_append_creates_phase_and_validates_batch() {
    let current = inited(&[("First", &["A"])]);
    let (_, change) = run(
        &current,
        r#"{"op":"append","phase":"Second","items":["C","D"]}"#,
    );
    let next = change.unwrap().phases;
    assert_eq!(next.len(), 2);
    assert_eq!(next[1].name, "Second");
    assert_eq!(next[1].tasks.len(), 2);

    // In-batch duplicate: whole op rejected, nothing half-applied.
    let (value, change) = run(
        &next,
        r#"{"op":"append","phase":"Third","items":["X","X"]}"#,
    );
    assert_eq!(value["ok"], json!(false));
    assert!(
        value["error"]
            .as_str()
            .unwrap()
            .contains("Task \"X\" already exists")
    );
    assert!(change.is_none());

    // Existing duplicate likewise.
    let (value, change) = run(
        &next,
        r#"{"op":"append","phase":"Third","items":["X","A"]}"#,
    );
    assert_eq!(value["ok"], json!(false));
    assert!(
        value["error"]
            .as_str()
            .unwrap()
            .contains("Task \"A\" already exists")
    );
    assert!(change.is_none());
}

#[test]
fn todo_init_rejects_duplicates_and_keeps_previous_state() {
    let current = inited(&[("First", &["A"])]);
    for arguments in [
        r#"{"op":"init","list":[{"phase":"P","items":["a"]},{"phase":"P","items":["b"]}]}"#,
        r#"{"op":"init","list":[{"phase":"P","items":["a"]},{"phase":"Q","items":["a"]}]}"#,
    ] {
        let (value, change) = run(&current, arguments);
        assert_eq!(value["ok"], json!(false), "{value}");
        assert!(value["error"].as_str().unwrap().contains("Duplicate"));
        assert!(change.is_none());
        assert_eq!(value["phases"], serde_json::to_value(&current).unwrap());
    }
}

#[test]
fn todo_init_with_empty_list_clears() {
    let current = inited(&[("First", &["A"])]);
    let (value, change) = run(&current, r#"{"op":"init","list":[]}"#);
    assert_eq!(value["ok"], json!(true));
    assert_eq!(value["content"], json!("Todo list cleared."));
    let change = change.expect("init must return a change");
    assert!(change.phases.is_empty());
}

#[test]
fn todo_init_missing_list_fails_and_keeps_old_list() {
    let current = inited(&[("First", &["A"])]);
    let (value, change) = run(&current, r#"{"op":"init"}"#);
    assert_eq!(value["ok"], json!(false));
    assert_eq!(value["error"], json!("Missing list for init operation"));
    assert!(change.is_none());
    assert_eq!(value["phases"], serde_json::to_value(&current).unwrap());
}

#[test]
fn todo_flat_init_targets_given_phase_or_tasks_default() {
    let (value, change) = run(&[], r#"{"op":"init","items":["A","B"],"phase":"Custom"}"#);
    assert_eq!(value["ok"], json!(true));
    let next = change.unwrap().phases;
    assert_eq!(next.len(), 1);
    assert_eq!(next[0].name, "Custom");

    let (value, change) = run(&[], r#"{"op":"init","items":["A"]}"#);
    assert_eq!(value["ok"], json!(true));
    let next = change.unwrap().phases;
    assert_eq!(next[0].name, "Tasks");
}

#[test]
fn todo_infer_missing_op() {
    // Non-empty list → init.
    let (value, _) = run(&[], r#"{"list":[{"phase":"P","items":["a"]}]}"#);
    assert_eq!(value["op"], json!("init"));
    // Bare items with nothing existing → init into Tasks.
    let (value, change) = run(&[], r#"{"items":["a"]}"#);
    assert_eq!(value["op"], json!("init"));
    assert_eq!(change.unwrap().phases[0].name, "Tasks");
    // Items + phase with existing phases → append.
    let current = inited(&[("P", &["a"])]);
    let (value, change) = run(&current, r#"{"items":["b"],"phase":"P"}"#);
    assert_eq!(value["op"], json!("append"));
    assert_eq!(change.unwrap().phases[0].tasks.len(), 2);
    // Bare items with existing phases → no inference.
    let (value, _) = run(&current, r#"{"items":["b"]}"#);
    assert_eq!(value["ok"], json!(false));
    // Task-only never infers done.
    let (value, _) = run(&current, r#"{"task":"a"}"#);
    assert_eq!(value["ok"], json!(false));
    // Empty list is not an init payload.
    let (value, _) = run(&current, r#"{"list":[]}"#);
    assert_eq!(value["ok"], json!(false));
}

#[test]
fn todo_schema_rejects_unknown_fields_nulls_and_types() {
    let current = inited(&[("P", &["a"])]);
    let cases = [
        (r#"{"op":"view","ops":[]}"#, "unknown field \"ops\""),
        (
            r#"{"op":"view","task":null}"#,
            "field \"task\" must not be null",
        ),
        (
            r#"{"op":"init","list":"x"}"#,
            "field \"list\" must be an array",
        ),
        (
            r#"{"op":"init","list":[{"phase":"P","items":[]}]}"#,
            "list[0].items must contain at least one string",
        ),
        (
            r#"{"op":"init","list":[{"phase":"P","items":["a"],"extra":1}]}"#,
            "list[0] has unknown field \"extra\"",
        ),
        (
            r#"{"op":"init","list":[{"items":["a"]}]}"#,
            "list[0] is missing field \"phase\"",
        ),
        (
            r#"{"op":"done","items":"x"}"#,
            "field \"items\" must be an array of strings",
        ),
        (
            r#"{"op":"done","occurrence":0}"#,
            "field \"occurrence\" must be an integer >= 1",
        ),
        (
            r#"{"op":"done","occurrence":1.5}"#,
            "field \"occurrence\" must be an integer >= 1",
        ),
        (
            r#"{"op":"done","occurrence":true}"#,
            "field \"occurrence\" must be an integer >= 1",
        ),
        (r#"{"op":"explode"}"#, "field \"op\" must be one of"),
        (r#"{"op":null}"#, "field \"op\" must not be null"),
        (r#"{"op":5}"#, "field \"op\" must be a string"),
        (r#"["op"]"#, "expected a single todo operation object"),
        (r#"not json"#, "Invalid todo arguments"),
    ];
    for (arguments, expected) in cases {
        let (value, change) = run(&current, arguments);
        assert_eq!(value["ok"], json!(false), "{arguments}: {value}");
        assert!(change.is_none(), "{arguments}");
        let error = value["error"].as_str().unwrap();
        assert!(error.contains(expected), "{arguments}: {error}");
        // Schema failures echo the error text as content.
        assert_eq!(value["content"], value["error"], "{arguments}");
        // Pre-call state survives.
        assert_eq!(
            value["phases"],
            serde_json::to_value(&current).unwrap(),
            "{arguments}"
        );
    }
    // Known op is attached when the raw arguments name one.
    let (value, _) = run(&current, r#"{"op":"view","task":null}"#);
    assert_eq!(value["op"], json!("view"));
    let (value, _) = run(&current, r#"{"op":"explode"}"#);
    assert!(value.get("op").is_none());
}

#[test]
fn todo_occurrence_rejected_for_plain_tasks() {
    let current = inited(&[("P", &["a"])]);
    let (value, change) = run(&current, r#"{"op":"done","task":"a","occurrence":1}"#);
    assert_eq!(value["ok"], json!(false));
    assert_eq!(
        value["error"],
        json!("occurrence is only available for plan tasks.")
    );
    assert_eq!(value["op"], json!("done"));
    assert!(change.is_none());
    assert_eq!(value["phases"], serde_json::to_value(&current).unwrap());
}

#[test]
fn todo_view_borrows_and_never_normalizes() {
    let current = phases(&[
        (
            "P",
            &[
                ("A", TodoStatus::InProgress),
                ("B", TodoStatus::InProgress),
                ("C", TodoStatus::Completed),
            ],
        ),
        ("Q", &[("D", TodoStatus::Blocked)]),
    ]);
    let (value, change) = run(&current, r#"{"op":"view","task":"whatever"}"#);
    assert_eq!(value["ok"], json!(true));
    assert_eq!(value["op"], json!("view"));
    assert!(change.is_none());
    let next: Vec<TodoPhase> = serde_json::from_value(value["phases"].clone()).unwrap();
    assert_eq!(next, current);
    assert_eq!(status_of(&next, "B"), TodoStatus::InProgress);
    let content = value["content"].as_str().unwrap();
    assert!(content.contains("  - A [in_progress] (P)"));
    assert!(content.contains("  - B [in_progress] (P)"));
    // Blocked tasks are not "remaining"; they appear in the checklist only.
    assert!(!content.contains("  - D [blocked] (Q)"));
    assert!(content.contains("    - [ ] D (blocked)"));
}

#[test]
fn todo_view_empty_list() {
    let (value, change) = run(&[], r#"{"op":"view"}"#);
    assert_eq!(value["content"], json!("Todo list is empty."));
    assert!(change.is_none());
}

#[test]
fn todo_exact_matching_no_trim_or_folding() {
    let current = inited(&[("P", &["Fix bug"])]);
    for probe in ["fix bug", "Fix Bug", " fix bug", "Fix bug "] {
        let (value, change) = run(&current, &json!({"op": "done", "task": probe}).to_string());
        assert_eq!(value["ok"], json!(false), "{probe}");
        assert!(change.is_none());
        assert!(
            value["error"]
                .as_str()
                .unwrap()
                .starts_with(&format!("Task \"{probe}\" not found"))
        );
    }
}

#[test]
fn todo_task_id_misuse_diagnostic() {
    let current = inited(&[("P", &["a"])]);
    let (value, _) = run(&current, r#"{"op":"done","task":"task-1"}"#);
    assert_eq!(
        value["error"],
        json!(
            "Task \"task-1\" not found. Tasks are referenced by content, not by IDs — pass the task's full text from the previous result."
        )
    );
}

#[test]
fn todo_not_found_hint_on_empty_list() {
    let (value, _) = run(&[], r#"{"op":"done","task":"x"}"#);
    assert_eq!(
        value["error"],
        json!("Task \"x\" not found (todo list is empty — was it replaced or not yet created?)")
    );
}

#[test]
fn todo_phase_diagnostics() {
    let current = inited(&[("P", &["a"])]);
    // Upstream truthiness: an empty phase string selects nothing and
    // falls through to every task, so done succeeds with no error.
    let (value, change) = run(&current, r#"{"op":"done","phase":""}"#);
    assert_eq!(value["ok"], json!(true));
    assert_eq!(
        status_of(&change.unwrap().phases, "a"),
        TodoStatus::Completed
    );
    let (value, _) = run(&current, r#"{"op":"done","phase":"Nope"}"#);
    assert_eq!(value["error"], json!("Phase \"Nope\" not found"));
}

#[test]
fn plan_todo_projection_preserves_ordinals_and_markdown_authority() {
    use crate::plan::{PlanDocument, PlanStatus};
    let mut document = PlanDocument::new().unwrap();
    document.content = "## Same\n- [ ] duplicate\n## Same\n- [ ] duplicate\n".into();
    let preview = project_plan_todos(&document, &[], false).unwrap();
    assert_eq!(preview.len(), 2);
    assert!(
        preview
            .iter()
            .flat_map(|p| &p.tasks)
            .all(|t| t.status == TodoStatus::Pending)
    );
    document.approved_tasks = Some(vec!["duplicate".into(), "duplicate".into()]);
    document.status = PlanStatus::Executed;
    let mut runtime = project_plan_todos(&document, &[], false).unwrap();
    assert_eq!(runtime[0].tasks[0].status, TodoStatus::InProgress);
    runtime[1].tasks[0].status = TodoStatus::Blocked;
    runtime[1].tasks[0].blocker = Some("waiting".into());
    document.content = "## Renamed\n- [x] duplicate\n## Same\n- [ ] duplicate\n".into();
    let merged = project_plan_todos(&document, &runtime, true).unwrap();
    assert_eq!(merged[0].name, "Renamed");
    assert_eq!(merged[0].tasks[0].status, TodoStatus::Completed);
    assert_eq!(merged[1].tasks[0].status, TodoStatus::Blocked);
    assert_eq!(merged[1].tasks[0].blocker.as_deref(), Some("waiting"));
    document.content = document.content.replace("[x]", "[ ]");
    let reopened = project_plan_todos(&document, &merged, true).unwrap();
    assert_eq!(reopened[0].tasks[0].status, TodoStatus::InProgress);
    let fresh = project_plan_todos(&document, &merged, false).unwrap();
    assert_eq!(fresh[1].tasks[0].status, TodoStatus::Pending);
    document.status = PlanStatus::Completed;
    let readonly = project_plan_todos(&document, &runtime, true).unwrap();
    assert!(
        readonly
            .iter()
            .flat_map(|p| &p.tasks)
            .all(|t| t.status == TodoStatus::Pending && t.blocker.is_none())
    );
    document.status = PlanStatus::Executed;
    document.content.push_str("- [ ] unapproved\n");
    assert_eq!(
        project_plan_todos(&document, &runtime, true)
            .unwrap_err()
            .to_string(),
        "Plan tasks changed; re-plan before completing."
    );
}
