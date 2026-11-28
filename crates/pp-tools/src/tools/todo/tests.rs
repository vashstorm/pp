use super::plan_adapter::plan_task_targets;
use super::*;
use pp_core::todo::{TodoItem, TodoStatus};

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
fn todo_init_and_view_rules() {
    {
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
                .contains("Overall: 0/3 done, 3 open.")
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

    {
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

        let (value, change) = run(&current, r#"{"op":"init"}"#);
        assert_eq!(value["ok"], json!(false));
        assert_eq!(value["error"], json!("Missing list for init operation"));
        assert!(change.is_none());
        assert_eq!(value["phases"], serde_json::to_value(&current).unwrap());

        let (value, change) = run(&current, r#"{"op":"init","list":[]}"#);
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["content"], json!("Todo list cleared."));
        assert!(change.expect("init must return a change").phases.is_empty());
    }

    {
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

    {
        let mut current = Vec::new();
        for args in [
            r#"{"op":"init","items":["a"],"occurrence":1}"#,
            r#"{"op":"append","phase":"Tasks","items":["b"],"occurrence":1}"#,
            r#"{"op":"view","occurrence":1}"#,
        ] {
            let (value, change) = run(&current, args);
            assert_eq!(value["ok"], true, "{args}: {value}");
            if let Some(change) = change {
                current = change.phases;
            }
        }
        assert_eq!(current[0].tasks.len(), 2);
    }

    {
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
        assert!(content.contains("Overall: 1/4 done, 2 open, 1 blocked."));
        assert!(content.contains("    - [ ] A (in progress)"));
        assert!(content.contains("    - [ ] B (in progress)"));
        assert!(content.contains("    - [ ] D (blocked)"));
        let (value, change) = run(&[], r#"{"op":"view"}"#);
        assert!(change.is_none());
        assert!(
            value["content"].as_str().unwrap().contains("empty"),
            "{value}"
        );
    }
}

#[test]
fn todo_done_start_drop_rm_and_parallel_rules() {
    {
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

    {
        let current = inited(&[("First", &["A"]), ("Second", &["C"])]);
        let (_, change) = run(&current, r#"{"op":"done"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
        assert_eq!(status_of(&next, "C"), TodoStatus::Completed);

        let current = inited(&[("First", &["A", "B"]), ("Second", &["C"])]);
        let (_, change) = run(&current, r#"{"op":"done","phase":"First"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
        assert_eq!(status_of(&next, "B"), TodoStatus::Completed);
        assert_eq!(status_of(&next, "C"), TodoStatus::InProgress);
    }

    {
        let current = inited(&[("First", &["A", "B"]), ("Second", &["C"])]);
        let (_, change) = run(&current, r#"{"op":"start","task":"C"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "C"), TodoStatus::InProgress);
        assert_eq!(status_of(&next, "A"), TodoStatus::InProgress);

        // Explicit re-open of a closed task is allowed upstream.
        let (_, change) = run(&next, r#"{"op":"done","task":"C"}"#);
        let next = change.unwrap().phases;
        let (_, change) = run(&next, r#"{"op":"start","task":"C"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "C"), TodoStatus::InProgress);
        assert_eq!(status_of(&next, "A"), TodoStatus::InProgress);

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

    {
        let mut current = inited(&[("First", &["A", "B"]), ("Second", &["C", "D", "E"])]);
        for task in ["B", "C", "D", "D"] {
            let (value, change) = run(&current, &json!({"op":"start","task":task}).to_string());
            assert_eq!(value["ok"], true);
            current = change.unwrap().phases;
        }
        for task in ["A", "B", "C", "D"] {
            assert_eq!(status_of(&current, task), TodoStatus::InProgress);
        }
        assert_eq!(status_of(&current, "E"), TodoStatus::Pending);

        // Done, block and append operations never revert other concurrent items and never start later work early.
        for args in [
            r#"{"op":"done","task":"B"}"#,
            r#"{"op":"block","task":"C","reason":"dependency"}"#,
            r#"{"op":"append","phase":"Second","items":["F"]}"#,
        ] {
            let (value, change) = run(&current, args);
            assert_eq!(value["ok"], true);
            current = change.unwrap().phases;
            for task in ["A", "D"] {
                assert_eq!(status_of(&current, task), TodoStatus::InProgress);
            }
            assert_eq!(status_of(&current, "E"), TodoStatus::Pending);
        }
        assert_eq!(status_of(&current, "B"), TodoStatus::Completed);
        assert_eq!(status_of(&current, "C"), TodoStatus::Blocked);
        assert_eq!(blocked_reason(&current, "C").as_deref(), Some("dependency"));
    }

    {
        let current = inited(&[("First", &["A", "B"])]);
        let (value, change) = run(&current, r#"{"op":"drop","task":"B"}"#);
        assert_eq!(value["ok"], json!(true));
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "B"), TodoStatus::Abandoned);
        let content = value["content"].as_str().unwrap();
        assert!(content.contains("Overall: 1/2 done, 1 open."));
        assert!(content.contains("    - [ ] B (dropped)"));

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
        let (value, change) = run(&next, r#"{"op":"view"}"#);
        assert_eq!(value["content"], json!("Todo list is empty."));
        assert!(change.is_none());
    }
}

#[test]
fn todo_block_unblock_and_append_rules() {
    {
        let current = inited(&[("Tasks", &["A", "B"])]);
        let (value, change) = run(
            &current,
            r#"{"op":"block","task":"B","reason":"wait\n  on\tx"}"#,
        );
        assert_eq!(value["ok"], json!(true));
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "B"), TodoStatus::Blocked);
        assert_eq!(blocked_reason(&next, "B").as_deref(), Some("wait on x"));
        assert_eq!(status_of(&next, "A"), TodoStatus::InProgress);

        let (_, change) = run(&next, r#"{"op":"done","task":"A"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
        assert_eq!(status_of(&next, "B"), TodoStatus::Blocked);

        let (_, change) = run(
            &next,
            r#"{"op":"block","task":"B","reason":"still waiting"}"#,
        );
        let next = change.unwrap().phases;
        assert_eq!(blocked_reason(&next, "B").as_deref(), Some("still waiting"));

        let (_, change) = run(&next, r#"{"op":"block","task":"B"}"#);
        let next = change.unwrap().phases;
        assert_eq!(blocked_reason(&next, "B"), None);

        let (_, change) = run(&next, r#"{"op":"unblock","task":"B"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "B"), TodoStatus::InProgress);
        assert_eq!(blocked_reason(&next, "B"), None);
    }

    {
        let current = inited(&[("First", &["A", "B"])]);
        let (_, change) = run(&current, r#"{"op":"done","task":"A"}"#);
        let next = change.unwrap().phases;
        let (_, change) = run(&next, r#"{"op":"block","phase":"First","reason":"halt"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "A"), TodoStatus::Completed);
        assert_eq!(status_of(&next, "B"), TodoStatus::Blocked);

        let current = phases(&[(
            "Tasks",
            &[("A", TodoStatus::InProgress), ("B", TodoStatus::Blocked)],
        )]);
        let (_, change) = run(&current, r#"{"op":"unblock","phase":"Tasks"}"#);
        let next = change.unwrap().phases;
        assert_eq!(status_of(&next, "A"), TodoStatus::InProgress);
        assert_eq!(status_of(&next, "B"), TodoStatus::Pending);
        assert_eq!(blocked_reason(&next, "B"), None);

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

    {
        let current = inited(&[("First", &["A"])]);
        let (_, change) = run(
            &current,
            r#"{"op":"append","phase":"Second","items":["C","D"]}"#,
        );
        let next = change.unwrap().phases;
        assert_eq!(next.len(), 2);
        assert_eq!(next[1].name, "Second");
        assert_eq!(next[1].tasks.len(), 2);

        for (items_json, expected) in [
            (r#"["X","X"]"#, "Task \"X\" already exists"),
            (r#"["X","A"]"#, "Task \"A\" already exists"),
        ] {
            let (value, change) = run(
                &next,
                &format!(r#"{{"op":"append","phase":"Third","items":{items_json}}}"#),
            );
            assert_eq!(value["ok"], json!(false));
            assert!(value["error"].as_str().unwrap().contains(expected));
            assert!(change.is_none());
        }
    }

    {
        let (_, change) = run(&[], r#"{"op":"init","items":["t1","t2","t3"]}"#);
        let mut phases = change.unwrap().phases;
        for (task, op, status) in [
            ("t1", "start", TodoStatus::InProgress),
            ("t2", "done", TodoStatus::Completed),
            ("t3", "drop", TodoStatus::Abandoned),
        ] {
            phases = run(
                &phases,
                &json!({"op":"block","task":task,"reason":"wait"}).to_string(),
            )
            .1
            .unwrap()
            .phases;
            phases = run(&phases, &json!({"op":op,"task":task}).to_string())
                .1
                .unwrap()
                .phases;
            let idx = task.strip_prefix('t').unwrap().parse::<usize>().unwrap() - 1;
            assert_eq!(phases[0].tasks[idx].status, status);
            assert_eq!(phases[0].tasks[idx].blocker, None);
        }
    }
}

#[test]
fn todo_schema_and_op_inference() {
    {
        let (value, _) = run(&[], r#"{"list":[{"phase":"P","items":["a"]}]}"#);
        assert_eq!(value["op"], json!("init"));
        let (value, change) = run(&[], r#"{"items":["a"]}"#);
        assert_eq!(value["op"], json!("init"));
        assert_eq!(change.unwrap().phases[0].name, "Tasks");

        let current = inited(&[("P", &["a"])]);
        let (value, change) = run(&current, r#"{"items":["b"],"phase":"P"}"#);
        assert_eq!(value["op"], json!("append"));
        assert_eq!(change.unwrap().phases[0].tasks.len(), 2);
        for payload in [r#"{"items":["b"]}"#, r#"{"task":"a"}"#, r#"{"list":[]}"#] {
            let (value, _) = run(&current, payload);
            assert_eq!(value["ok"], json!(false), "{payload}");
        }
    }

    {
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
            assert_eq!(value["content"], value["error"], "{arguments}");
            assert_eq!(
                value["phases"],
                serde_json::to_value(&current).unwrap(),
                "{arguments}"
            );
        }
        let (value, _) = run(&current, r#"{"op":"view","task":null}"#);
        assert_eq!(value["op"], json!("view"));
        let (value, _) = run(&current, r#"{"op":"explode"}"#);
        assert!(value.get("op").is_none());
    }
}

#[test]
fn todo_target_occurrence_exact_match_and_diagnostics() {
    {
        let current = phases(&[
            ("First", &[("same", TodoStatus::InProgress)]),
            (
                "Second",
                &[("same", TodoStatus::Pending), ("same", TodoStatus::Pending)],
            ),
        ]);
        let (value, change) = run(&current, r#"{"op":"done","task":"same","occurrence":2}"#);
        assert_eq!(value["ok"], true, "{value}");
        let next = change.unwrap().phases;
        assert_eq!(next[0], current[0]);
        assert_eq!(next[1].tasks[0].status, TodoStatus::Completed);
        assert_eq!(next[1].tasks[1], current[1].tasks[1]);
        for (op, status) in [
            ("start", TodoStatus::InProgress),
            ("done", TodoStatus::Completed),
            ("drop", TodoStatus::Abandoned),
            ("block", TodoStatus::Blocked),
            ("unblock", TodoStatus::Pending),
            ("rm", TodoStatus::Pending),
        ] {
            let mut before = current.clone();
            if op == "unblock" {
                before[1].tasks[1].status = TodoStatus::Blocked;
                before[1].tasks[1].blocker = Some("waiting".into());
            }
            let args = json!({"op":op,"task":"same","phase":"Second","occurrence":2});
            let (value, change) = run(&before, &args.to_string());
            assert_eq!(value["ok"], true, "{args}: {value}");
            let next = change.unwrap().phases;
            assert_eq!(next[1].tasks[0], before[1].tasks[0], "{op}");
            if op == "rm" {
                assert_eq!(next[1].tasks.len(), 1);
            } else {
                assert_eq!(next[1].tasks[1].status, status, "{op}");
                assert!(next[1].tasks[1].blocker.is_none() || op == "block");
            }
        }
    }

    {
        let current = inited(&[("First", &["a"]), ("Second", &["b"])]);
        for args in [
            r#"{"op":"done","task":"a","occurrence":2}"#,
            r#"{"op":"start","task":"a","occurrence":18446744073709551615}"#,
            r#"{"op":"done","task":"a","phase":"Second","occurrence":1}"#,
            r#"{"op":"done","occurrence":1}"#,
            r#"{"op":"rm","task":"","occurrence":1}"#,
        ] {
            let (value, change) = run(&current, args);
            assert_eq!(value["ok"], false, "{args}: {value}");
            assert!(change.is_none(), "{args}");
            assert_eq!(value["phases"], serde_json::to_value(&current).unwrap());
        }
    }

    {
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

        let current = inited(&[("P", &["a"])]);
        let empty = Vec::new();
        for (state, arguments, task, hint) in [
            (
                &current,
                r#"{"op":"done","task":"task-1"}"#,
                "task-1",
                "not by IDs",
            ),
            (&empty, r#"{"op":"done","task":"x"}"#, "x", "empty"),
        ] {
            let (value, _) = run(state, arguments);
            let error = value["error"].as_str().unwrap();
            assert!(
                error.starts_with(&format!("Task \"{task}\" not found")),
                "{arguments}: {error}"
            );
            assert!(error.contains(hint), "{arguments}: {error}");
        }

        let (value, change) = run(&current, r#"{"op":"done","phase":""}"#);
        assert_eq!(value["ok"], json!(true));
        assert_eq!(
            status_of(&change.unwrap().phases, "a"),
            TodoStatus::Completed
        );
        let (value, _) = run(&current, r#"{"op":"done","phase":"Nope"}"#);
        assert_eq!(value["error"], json!("Phase \"Nope\" not found"));
    }
}

#[test]
fn plan_todo_projection_preserves_ordinals_and_markdown_authority() {
    use pp_core::plan::PlanDocument;
    use pp_core::plan::PlanStatus;
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
