use super::*;

#[test]
fn todo_review_policy_refuses_todo() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let execution = tools.execute(
        "todo",
        r#"{"op":"view"}"#,
        "review-todo",
        ToolPolicy::Review,
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(execution.stop_error.is_none());
    let result: Value = serde_json::from_str(&execution.output).unwrap();
    assert_eq!(
        result["error"],
        json!("Tool is not available during Plan review.")
    );
}

#[test]
fn todo_plan_policy_hides_and_refuses_every_operation() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    assert!(
        tools
            .all_definitions(WorkMode::Plan)
            .iter()
            .all(|tool| tool.name != "todo")
    );
    let document = crate::plan::PlanDocument::new().unwrap();
    for arguments in [
        r#"{"op":"view"}"#,
        r#"{"op":"init","list":[]}"#,
        r#"{"op":"done"}"#,
        "invalid json",
    ] {
        let execution = tools.execute(
            "todo",
            arguments,
            "plan-todo",
            ToolPolicy::Plan {
                document: &document,
                sealed: false,
                input: None,
            },
            &CancellationToken::new(),
            &mut |_| {},
        );
        assert!(execution.stop_error.is_none());
        assert!(execution.plan_change.is_none());
        assert!(execution.todo_change.is_none());
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert_eq!(
            result,
            json!({"ok":false,"error":"Tool is not available in Plan mode."})
        );
    }
}

#[test]
fn todo_executed_plan_done_writes_checkbox_and_reports_change() {
    let dir = tempfile::tempdir().unwrap();
    let content = "## Write\n- [ ] Write canary bytes.\n  - Acceptance: read back.\n- [ ] Verify canary bytes.\n";
    let (tools, document, workflow) =
        plan_todo_fixture(&dir, "todo-executed.md", PlanStatus::Executed, content);
    let execution = todo_call(
        &tools,
        &workflow,
        json!({"op":"done","task":"Write canary bytes."}),
    );
    assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
    assert!(execution.plan_change.is_none());
    let result: Value = serde_json::from_str(&execution.output).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["op"], "done");
    assert_eq!(result["plan"]["id"], document.id);
    assert_eq!(result["plan"]["revision"], 2);
    assert_eq!(result["plan"]["status"], "executed");
    assert_eq!(result["plan"]["readonly"], false);
    assert_eq!(result["phases"][0]["tasks"][0]["status"], "completed");
    assert_eq!(result["phases"][0]["tasks"][1]["status"], "in_progress");
    let change = execution.todo_change.expect("done must report a change");
    assert_eq!(
        change.plan.as_ref().expect("plan projection").plan_id,
        document.id
    );
    assert_eq!(change.plan.as_ref().unwrap().revision, 2);
    let new_content = change
        .plan
        .as_ref()
        .unwrap()
        .content
        .as_deref()
        .expect("checkbox write-back carries content");
    assert_eq!(
        new_content,
        "## Write\n- [x] Write canary bytes.\n  - Acceptance: read back.\n- [ ] Verify canary bytes.\n"
    );
    // The other bytes are untouched and the file is committed.
    let path = dir.path().join(".pp/plans/todo-executed.md");
    assert_eq!(std::fs::read(&path).unwrap(), new_content.as_bytes());
}

#[test]
fn todo_executed_plan_duplicate_needs_phase_or_occurrence() {
    let dir = tempfile::tempdir().unwrap();
    let content = "## Phase\n- [ ] Write canary bytes.\n- [ ] Write canary bytes.\n";
    let (tools, document, workflow) =
        plan_todo_fixture(&dir, "todo-duplicate.md", PlanStatus::Executed, content);
    let ambiguous = todo_call(
        &tools,
        &workflow,
        json!({"op":"done","task":"Write canary bytes."}),
    );
    let ambiguous: Value = serde_json::from_str(&ambiguous.output).unwrap();
    assert_eq!(ambiguous["ok"], false);
    assert_eq!(
        ambiguous["error"],
        json!("Ambiguous plan task; specify phase or occurrence.")
    );
    assert_eq!(ambiguous["phases"][0]["tasks"][0]["status"], "in_progress");
    assert_eq!(
        std::fs::read(dir.path().join(".pp/plans/todo-duplicate.md")).unwrap(),
        content.as_bytes()
    );

    // Phase filter alone is still ambiguous; occurrence picks by ordinal.
    let still_ambiguous = todo_call(
        &tools,
        &workflow,
        json!({"op":"done","task":"Write canary bytes.","phase":"Phase"}),
    );
    let still_ambiguous: Value = serde_json::from_str(&still_ambiguous.output).unwrap();
    assert_eq!(
        still_ambiguous["error"],
        json!("Ambiguous plan task; specify phase or occurrence.")
    );
    let second = todo_call(
        &tools,
        &workflow,
        json!({"op":"done","task":"Write canary bytes.","occurrence":2}),
    );
    let result: Value = serde_json::from_str(&second.output).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    let change = second.todo_change.unwrap();
    assert_eq!(
        change.plan.unwrap().content.unwrap(),
        "## Phase\n- [ ] Write canary bytes.\n- [x] Write canary bytes.\n"
    );
    // Out-of-range occurrences fail without side effects.
    let over = todo_call(
        &tools,
        &workflow,
        json!({"op":"done","task":"Write canary bytes.","occurrence":3}),
    );
    let over: Value = serde_json::from_str(&over.output).unwrap();
    assert_eq!(over["ok"], false);
    assert!(over["error"].as_str().unwrap().contains("occurrence 3"));
    assert_eq!(
        std::fs::read(dir.path().join(".pp/plans/todo-duplicate.md")).unwrap(),
        "## Phase\n- [ ] Write canary bytes.\n- [x] Write canary bytes.\n".as_bytes()
    );
    let _ = document;
}

#[test]
fn todo_executed_plan_block_is_runtime_only_without_revision() {
    let dir = tempfile::tempdir().unwrap();
    let content = "## Phase\n- [ ] Write canary bytes.\n- [ ] Verify canary bytes.\n";
    let (tools, document, workflow) =
        plan_todo_fixture(&dir, "todo-blocked.md", PlanStatus::Executed, content);
    let execution = todo_call(
        &tools,
        &workflow,
        json!({"op":"block","task":"Write canary bytes.","reason":"wait for upstream"}),
    );
    let result: Value = serde_json::from_str(&execution.output).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["phases"][0]["tasks"][0]["status"], "blocked");
    let change = execution.todo_change.expect("block reports a change");
    let plan = change.plan.expect("plan projection");
    assert_eq!(plan.revision, document.revision);
    assert!(plan.content.is_none(), "runtime-only state has no content");
    assert_eq!(
        std::fs::read(dir.path().join(".pp/plans/todo-blocked.md")).unwrap(),
        content.as_bytes()
    );
    // Unblock clears the blocker without touching the file either.
    let unblock = todo_call(
        &tools,
        &workflow,
        json!({"op":"unblock","task":"Write canary bytes."}),
    );
    let result: Value = serde_json::from_str(&unblock.output).unwrap();
    assert_eq!(result["ok"], true);
    let change = unblock.todo_change.unwrap();
    let plan = change.plan.unwrap();
    assert_eq!(plan.revision, document.revision);
    assert!(plan.content.is_none());
    assert_eq!(
        change.phases[0].tasks[0].status,
        todo::TodoStatus::InProgress
    );
}

#[test]
fn todo_executed_plan_structure_operations_are_locked() {
    let dir = tempfile::tempdir().unwrap();
    let content = "## Phase\n- [ ] Write canary bytes.\n";
    let (tools, _document, workflow) =
        plan_todo_fixture(&dir, "todo-locked.md", PlanStatus::Executed, content);
    for arguments in [
        json!({"op":"init","list":[{"phase":"Extra","items":["new"]}]}),
        json!({"op":"append","phase":"Extra","items":["new"]}),
        json!({"op":"rm","task":"Write canary bytes."}),
        json!({"op":"drop","task":"Write canary bytes."}),
    ] {
        let execution = todo_call(&tools, &workflow, arguments);
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert_eq!(result["ok"], false, "{result}");
        assert_eq!(
            result["error"],
            json!(
                "Approved plan tasks cannot be added, removed, replaced, or dropped; re-plan first."
            )
        );
        assert_eq!(
            std::fs::read(dir.path().join(".pp/plans/todo-locked.md")).unwrap(),
            content.as_bytes()
        );
    }
}

#[test]
fn todo_finished_plan_is_read_only_until_init_detaches() {
    let dir = tempfile::tempdir().unwrap();
    let content = "## Phase\n- [x] Write canary bytes.\n- [x] Verify canary bytes.\n";
    let (tools, document, workflow) =
        plan_todo_fixture(&dir, "todo-finished.md", PlanStatus::Completed, content);
    // Bound finished plan: mutations keep the historical diagnostic and
    // never touch the file.
    let done = todo_call(
        &tools,
        &workflow,
        json!({"op":"done","task":"Write canary bytes."}),
    );
    let done: Value = serde_json::from_str(&done.output).unwrap();
    assert_eq!(done["ok"], false);
    assert_eq!(
        done["error"],
        json!("Plan todos are read-only; initialize a new standalone list.")
    );
    assert_eq!(done["plan"]["readonly"], true);
    assert_eq!(done["phases"][0]["tasks"][0]["status"], "completed");
    // View stays available against the document projection.
    let view = todo_call(&tools, &workflow, json!({"op":"view"}));
    let view: Value = serde_json::from_str(&view.output).unwrap();
    assert_eq!(view["ok"], true);
    assert_eq!(view["plan"]["readonly"], true);
    assert!(view["phases"][0]["tasks"][0]["status"] == "completed");
    // Explicit init starts a standalone list: plan-less change, document
    // untouched.
    let init = todo_call(
        &tools,
        &workflow,
        json!({"op":"init","list":[{"phase":"Fresh","items":["standalone"] }]}),
    );
    let result: Value = serde_json::from_str(&init.output).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert!(
        result.get("plan").is_none(),
        "standalone results carry no plan object"
    );
    let change = init.todo_change.expect("init reports a change");
    assert!(change.plan.is_none(), "detaching init has no plan effect");
    assert_eq!(change.phases[0].name, "Fresh");
    assert_eq!(
        std::fs::read(dir.path().join(".pp/plans/todo-finished.md")).unwrap(),
        content.as_bytes()
    );
    let _ = document;
}

#[test]
fn todo_plan_tasks_drift_is_refused_for_every_editable_call() {
    let dir = tempfile::tempdir().unwrap();
    let content = "## Phase\n- [ ] Write canary bytes.\n";
    let (tools, _document, mut workflow) =
        plan_todo_fixture(&dir, "todo-drift.md", PlanStatus::Executed, content);
    // External rewrite: document and file both show a task set that no
    // longer matches the approved vector.
    workflow.plan.as_mut().unwrap().content = "## Phase\n- [ ] Rewritten task.\n".into();
    let path = dir.path().join(".pp/plans/todo-drift.md");
    std::fs::write(&path, workflow.plan.as_ref().unwrap().content.as_bytes()).unwrap();
    for arguments in [
        json!({"op":"view"}),
        json!({"op":"done","task":"Rewritten task."}),
    ] {
        let execution = todo_call(&tools, &workflow, arguments);
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert_eq!(result["ok"], false, "{result}");
        assert_eq!(
            result["error"],
            json!("Plan tasks changed; re-plan before completing.")
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            workflow.plan.as_ref().unwrap().content.as_bytes()
        );
    }
}
