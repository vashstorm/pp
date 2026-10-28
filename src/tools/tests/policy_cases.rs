use super::*;

#[test]
fn cancelled_token_skips_execution_but_reports_started_and_finished() {
    let workflow = WorkflowState::default();
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let token = CancellationToken::new();
    token.cancel();
    let mut events = 0;
    let execution = tools.execute(
        "write",
        &json!({"path":"skipped.txt","content":"must-not-exist"}).to_string(),
        "skip-call",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        &token,
        &mut |event| match event {
            AgentEvent::ToolStarted { .. } | AgentEvent::ToolFinished { .. } => events += 1,
            _ => panic!("unexpected event during skipped tool"),
        },
    );
    assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
    assert_eq!(events, 2);
    let result: Value = serde_json::from_str(&execution.output).unwrap();
    assert_eq!(
        result,
        json!({"ok":false,"cancelled":true,"error":"Not executed: task cancelled."})
    );
    assert!(!dir.path().join("skipped.txt").exists());
    assert!(!dir.path().join("skipped").exists());
}

#[test]
fn plan_policy_denies_side_effects_unknown_names_and_malformed_args() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("canary.txt"), b"unchanged\n").unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let document = plan_document();
    let cases: Vec<(&str, String)> = vec![
        (
            "bash",
            json!({"command":"printf side-effect > marker"}).to_string(),
        ),
        // Malformed arguments must not change the denial or execute.
        ("bash", "{not json".to_owned()),
        (
            "write",
            json!({"path":"canary.txt","content":"overwritten"}).to_string(),
        ),
        ("edit", "not json".to_owned()),
        (
            "plan_search",
            json!({"query":"needle","path":"canary.txt"}).to_string(),
        ),
        ("find", json!({"args":[".","-name","*.txt"]}).to_string()),
        (
            "bash_task",
            json!({"task_id":"x","action":"stop"}).to_string(),
        ),
        // Unknown names must not reach MCP dispatch.
        ("mcp_unknown", "{}".to_owned()),
        // ask is whitelisted only with an installed handler.
        ("ask", "{}".to_owned()),
    ];
    let mut events = Vec::new();
    for (name, arguments) in &cases {
        let execution = tools.execute(
            name,
            arguments,
            "gate-call",
            ToolPolicy::Plan {
                document: &document,
                sealed: false,
                input: None,
            },
            &CancellationToken::new(),
            &mut |event| events.push(event),
        );
        assert!(
            execution.stop_error.is_none(),
            "{name}: {:?}",
            execution.stop_error
        );
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert_eq!(
            result,
            json!({"ok":false,"error":"Tool is not available in Plan mode."}),
            "{name}: {result}"
        );
    }
    assert_started_finished_pairs(&events, cases.len());
    assert_eq!(
        fs::read(dir.path().join("canary.txt")).unwrap(),
        b"unchanged\n"
    );
    assert!(!dir.path().join("marker").exists());
}

#[test]
fn plan_policy_allows_read_and_sealed_batch_rejects_every_call() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("notes.txt"), b"hello\n").unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let document = plan_document();
    let read = tools.execute(
        "read",
        &json!({"path":"notes.txt"}).to_string(),
        "plan-read",
        ToolPolicy::Plan {
            document: &document,
            sealed: false,
            input: None,
        },
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(read.stop_error.is_none(), "{:?}", read.stop_error);
    let result: Value = serde_json::from_str(&read.output).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["content"], "hello\n");

    for (name, arguments) in [
        ("read", json!({"path":"notes.txt"}).to_string()),
        ("bash", json!({"command":"echo hi"}).to_string()),
    ] {
        let execution = tools.execute(
            name,
            &arguments,
            "sealed-call",
            ToolPolicy::Plan {
                document: &document,
                sealed: true,
                input: None,
            },
            &CancellationToken::new(),
            &mut |_| {},
        );
        assert!(
            execution.stop_error.is_none(),
            "{name}: {:?}",
            execution.stop_error
        );
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert_eq!(
            result,
            json!({"ok":false,"error":"Not executed: plan submitted for review."}),
            "{name}: {result}"
        );
    }
    assert_eq!(fs::read(dir.path().join("notes.txt")).unwrap(), b"hello\n");
}

#[test]
fn review_policy_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("src.txt"), b"evidence\n").unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let cases: Vec<(&str, String)> = vec![
        ("ask", "{}".to_owned()),
        ("write", json!({"path":"out.txt","content":"x"}).to_string()),
        ("edit", "{}".to_owned()),
        ("bash", json!({"command":"echo hi"}).to_string()),
        (
            "plan_write",
            json!({"title":"t","content":"c","expected_revision":0}).to_string(),
        ),
        ("plan_submit", json!({"expected_revision":1}).to_string()),
        ("unknown_mcp", "{}".to_owned()),
    ];
    for (name, arguments) in &cases {
        let execution = tools.execute(
            name,
            arguments,
            "review-call",
            ToolPolicy::Review,
            &CancellationToken::new(),
            &mut |_| {},
        );
        assert!(
            execution.stop_error.is_none(),
            "{name}: {:?}",
            execution.stop_error
        );
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert_eq!(
            result,
            json!({"ok":false,"error":"Tool is not available during Plan review."}),
            "{name}: {result}"
        );
    }
    let read = tools.execute(
        "read",
        &json!({"path":"src.txt"}).to_string(),
        "review-read",
        ToolPolicy::Review,
        &CancellationToken::new(),
        &mut |_| {},
    );
    let result: Value = serde_json::from_str(&read.output).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["content"], "evidence\n");
    assert!(!dir.path().join("out.txt").exists());
}

#[test]
fn execute_policy_rejects_dedicated_plan_tools() {
    let workflow = WorkflowState::default();
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    for name in ["plan_write", "plan_submit"] {
        let execution = tools.execute(
            name,
            "{}",
            "exec-call",
            ToolPolicy::Execute {
                workflow: &workflow,
            },
            &CancellationToken::new(),
            &mut |_| {},
        );
        assert!(
            execution.stop_error.is_none(),
            "{name}: {:?}",
            execution.stop_error
        );
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert_eq!(
            result,
            json!({"ok":false,"error":format!("{name} is only available in Plan mode.")}),
            "{name}: {result}"
        );
    }
}

#[test]
fn plan_tools_report_changes_through_execute_and_seal_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let content = "- [ ] task\n";
    let mut document = plan_document();
    let plan_path = ".pp/drafts/create-canary.md".to_owned();

    // First write creates the named draft path and reports Written.
    let execution = tools.execute(
            "plan_write",
            &json!({"title":"Plan","file_name":"create-canary.md","content":content,"expected_revision":0}).to_string(),
            "write-call",
            ToolPolicy::Plan {
                document: &document,
                sealed: false,
                input: None,
            },
            &CancellationToken::new(),
            &mut |_| {},
        );
    assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
    let result: Value = serde_json::from_str(&execution.output).unwrap();
    assert_eq!(result["path"].as_str(), Some(plan_path.as_str()));
    assert_eq!(result["revision"], 1);
    assert!(matches!(
        execution.plan_change,
        Some(PlanChange::Written { .. })
    ));
    assert_eq!(
        std::fs::read(dir.path().join(&plan_path)).unwrap(),
        content.as_bytes()
    );

    // Mirror the caller applying the change to its workflow snapshot.
    document.revision = 1;
    document.title = "Plan".into();
    document.file_name = Some("create-canary.md".into());
    document.content = content.to_owned();

    // Identical title/name/content is a no-op without a change or bump.
    let execution = tools.execute(
            "plan_write",
            &json!({"title":"Plan","file_name":"create-canary.md","content":content,"expected_revision":1}).to_string(),
            "noop-call",
            ToolPolicy::Plan {
                document: &document,
                sealed: false,
                input: None,
            },
            &CancellationToken::new(),
            &mut |_| {},
        );
    assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
    let result: Value = serde_json::from_str(&execution.output).unwrap();
    assert_eq!(result, json!({"ok":true,"path":plan_path,"revision":1}));
    assert!(execution.plan_change.is_none());

    // Pending steering input defers the submission with an exact error.
    let queue = InputQueue::default();
    queue.begin_run().unwrap();
    queue
        .enqueue(
            crate::agent::input::PendingKind::Steering,
            "steer".into(),
            None,
            false,
        )
        .unwrap();
    let arguments = json!({"expected_revision":1}).to_string();
    let execution = tools.execute(
        "plan_submit",
        &arguments,
        "submit-call",
        ToolPolicy::Plan {
            document: &document,
            sealed: false,
            input: Some(&queue),
        },
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
    let result: Value = serde_json::from_str(&execution.output).unwrap();
    assert_eq!(
        result,
        json!({"ok":false,"error":"Apply pending input before submitting the plan."})
    );
    assert!(execution.plan_change.is_none());
    assert_eq!(
        queue.claim_steering(false).unwrap().len(),
        1,
        "deferred submission must not consume steering input"
    );

    // Without pending steering the submission seals and reports Submitted.
    let execution = tools.execute(
        "plan_submit",
        &arguments,
        "submit-call",
        ToolPolicy::Plan {
            document: &document,
            sealed: false,
            input: Some(&queue),
        },
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
    let result: Value = serde_json::from_str(&execution.output).unwrap();
    assert_eq!(result, json!({"ok":true,"revision":1}));
    assert!(matches!(execution.plan_change, Some(PlanChange::Submitted)));
    // The file content is untouched by submission.
    assert_eq!(
        std::fs::read(dir.path().join(&plan_path)).unwrap(),
        content.as_bytes()
    );
}

#[test]
fn plan_file_isolation_and_is_plan_path() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let mut document = plan_document();
    document.revision = 1;
    document.file_name = Some("create-canary.md".into());
    let plan_path = ".pp/drafts/create-canary.md".to_owned();
    std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
    std::fs::write(dir.path().join(&plan_path), b"- [ ] task\n").unwrap();
    assert!(tools.is_plan_path(&plan_path, &plan_path));
    assert!(!tools.is_plan_path("canary.txt", &plan_path));
    assert_eq!(tools.read_plan(&document).unwrap(), "- [ ] task\n");
    // Only the exact plan path is ever touched by plan IO.
    std::fs::write(dir.path().join("canary.txt"), b"unchanged\n").unwrap();
    // 未保存的 revision-0 绑定没有路径。
    let error = tools.read_plan(&plan_document()).unwrap_err();
    assert_eq!(error.to_string(), "No saved plan.");
    assert_eq!(
        std::fs::read(dir.path().join("canary.txt")).unwrap(),
        b"unchanged\n"
    );
}
