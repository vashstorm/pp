use super::*;

#[test]
fn policy_gates_cancel_review_plan_and_execute() {
    {
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

    {
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

    {
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

    {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("src.txt"), b"evidence\n").unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let cases: Vec<(&str, String)> = vec![
            ("ask", "{}".to_owned()),
            ("write", json!({"path":"out.txt","content":"x"}).to_string()),
            ("edit", "{}".to_owned()),
            ("bash", json!({"command":"echo hi"}).to_string()),
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

    {
        let workflow = WorkflowState::default();
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let name = "plan_submit";
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
fn plan_tools_change_reporting_and_path_isolation() {
    {
        // The save layer validates absolute arguments against the canonical root; once the temp parent is canonicalized the spelling matches.
        let dir = tempfile::Builder::new()
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let content = "- [ ] task\n";
        let mut document = plan_document();
        let plan_path = ".pp/drafts/create-canary.md".to_owned();

        // The first write registers the current plan and returns Written, without joining the ordinary file undo records.
        let execution = tools.execute(
            "write",
            &json!({"path":".pp/drafts/create-canary.md","content":content}).to_string(),
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

        let Some(PlanChange::Written { title, .. }) = &execution.plan_change else {
            panic!("a plan write must return Written");
        };
        assert_eq!(title, "create-canary");

        // Simulate the caller committing the write result into the workflow snapshot.
        document.revision = 1;
        document.title = "create-canary".into();
        document.file_name = Some("create-canary.md".into());
        document.content = content.to_owned();

        // An identical body produces no change and does not bump the revision.
        let execution = tools.execute(
            "write",
            &json!({"path":dir.path().join(".pp/drafts/./create-canary.md"),"content":content})
                .to_string(),
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

        struct MockGate {
            allow: bool,
        }
        impl crate::ports::PlanSubmitGate for MockGate {
            fn seal_for_plan_review(&self) -> Result<bool> {
                Ok(self.allow)
            }
        }

        // Pending steering input defers the submission with an exact error.
        let deferred = MockGate { allow: false };
        let arguments = json!({"expected_revision":1}).to_string();
        let execution = tools.execute(
            "plan_submit",
            &arguments,
            "submit-call",
            ToolPolicy::Plan {
                document: &document,
                sealed: false,
                input: Some(&deferred),
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

        // Without pending steering the submission seals and reports Submitted.
        let allowed = MockGate { allow: true };
        let execution = tools.execute(
            "plan_submit",
            &arguments,
            "submit-call",
            ToolPolicy::Plan {
                document: &document,
                sealed: false,
                input: Some(&allowed),
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

    {
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
        // An unsaved revision-0 binding has no path.
        let error = tools.read_plan(&plan_document()).unwrap_err();
        assert_eq!(error.to_string(), "No saved plan.");
        assert_eq!(
            std::fs::read(dir.path().join("canary.txt")).unwrap(),
            b"unchanged\n"
        );
    }
}

#[test]
fn plan_write_uses_common_write_schema_and_rejects_removed_tool_and_old_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let ordinary = Tools::definitions(EditMode::Replace, ToolLimits::default())
        .into_iter()
        .find(|tool| tool.name == "write")
        .unwrap();
    for mode in [WorkMode::Execute, WorkMode::Plan] {
        let definitions = tools.all_definitions(mode);
        assert!(!definitions.iter().any(|tool| tool.name == "plan_write"));
        let write = definitions
            .iter()
            .find(|tool| tool.name == "write")
            .unwrap();
        assert_eq!(write.parameters, ordinary.parameters);
    }
    let replace_rg = tools
        .all_definitions(WorkMode::Execute)
        .into_iter()
        .find(|tool| tool.name == "rg")
        .unwrap();
    assert!(
        !replace_rg
            .description
            .as_deref()
            .unwrap()
            .contains("Hashline")
    );
    let rg_props = &replace_rg.parameters.as_ref().unwrap()["properties"];
    for alias in ["-n", "case", "context", "-g"] {
        assert!(rg_props.get(alias).is_none(), "unexpected rg alias {alias}");
    }
    let hashline_tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
    let hashline_rg = hashline_tools
        .all_definitions(WorkMode::Execute)
        .into_iter()
        .find(|tool| tool.name == "rg")
        .unwrap();
    assert!(
        hashline_rg
            .description
            .as_deref()
            .unwrap()
            .contains("Hashline")
    );
    assert!(
        !tools
            .review_definitions()
            .iter()
            .any(|tool| tool.name == "write")
    );

    let document = plan_document();
    for (name, arguments) in [
        (
            "plan_write",
            json!({"title":"Removed","file_name":"removed.md","content":"- [ ] forbidden","expected_revision":0}),
        ),
        (
            "write",
            json!({"path":".pp/drafts/removed.md","content":"- [ ] forbidden","expected_revision":0}),
        ),
        (
            "write",
            json!({"path":".pp/drafts/removed.md","content":"- [ ] forbidden","title":"Old argument"}),
        ),
        (
            "write",
            json!({"path":".pp/done/removed.md","content":"- [ ] forbidden"}),
        ),
        (
            "write",
            json!({"path":".pp/drafts/nested/removed.md","content":"- [ ] forbidden"}),
        ),
        (
            "write",
            json!({"path":".pp/plan-catalog.json","content":"{}"}),
        ),
    ] {
        let execution = tools.execute(
            name,
            &arguments.to_string(),
            "removed-call",
            ToolPolicy::Plan {
                document: &document,
                sealed: false,
                input: None,
            },
            &CancellationToken::new(),
            &mut |_| {},
        );
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert_eq!(result["ok"], false, "{name}: {result}");
        assert!(execution.plan_change.is_none());
    }
    assert!(!dir.path().join(".pp").exists());
}
