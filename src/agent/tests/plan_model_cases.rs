use super::*;

#[test]
fn plan_submit_seals_batch_without_another_model_request() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        agent.workflow.mode = crate::plan::WorkMode::Plan;
        agent.workflow.plan = Some(crate::plan::PlanDocument::new().unwrap());
        agent
            .session
            .set_tools(agent.tools.all_definitions(agent.workflow.mode));
        let content = "# Create hello\n\n- [ ] Create hello.txt and read back hello.\n";
        let serving = protocol.to_owned();
        let worker = std::thread::spawn(move || {
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let _: Value = serde_json::from_reader(request.as_reader()).unwrap();
            tool_call_for(
                &serving,
                request,
                "plan_write",
                &json!({"title":"Create hello","file_name":"create-canary.md","content":content,"expected_revision":0})
                    .to_string(),
            );
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let _: Value = serde_json::from_reader(request.as_reader()).unwrap();
            let calls = [
                ("submit", "plan_submit", json!({"expected_revision":1})),
                ("shell", "bash", json!({"command":"touch forbidden"})),
                (
                    "rewrite",
                    "plan_write",
                    json!({"title":"Wrong","file_name":"create-canary.md","content":"- [ ] wrong","expected_revision":1}),
                ),
            ];
            if serving == "completion" {
                let calls: Vec<_> = calls.into_iter().enumerate().map(|(index,(id,name,args))| json!({"id":id,"type":"function","index":index,"function":{"name":name,"arguments":args.to_string()}})).collect();
                completion_stream(
                    request,
                    json!({"role":"assistant","content":null,"tool_calls":calls}),
                    "tool_calls",
                );
            } else {
                let calls: Vec<_> = calls.into_iter().map(|(id,name,args)| json!({"type":"function_call","id":format!("fc-{id}"),"call_id":id,"name":name,"arguments":args.to_string(),"status":"completed"})).collect();
                response_stream(request, &calls, true);
            }
            server
        });
        let queue = InputQueue::default();
        queue.begin_run().unwrap();
        let mut results = Vec::new();
        let outcome = agent
            .run(
                "Plan hello",
                &CancellationToken::new(),
                Some(&queue),
                &mut |event| {
                    if let AgentEvent::ToolFinished { id, result } = event {
                        results.push((id, result));
                    }
                },
            )
            .unwrap();
        let server = worker.join().unwrap();
        assert_eq!(outcome, RunOutcome::AwaitingPlanReview);
        assert!(server.try_recv().unwrap().is_none());
        assert_eq!(results.len(), 4);
        assert_eq!(results[1].0, "submit");
        assert_eq!(results[1].1["ok"], true);
        for (id, result) in &results[2..] {
            assert!(matches!(id.as_str(), "shell" | "rewrite"));
            assert_eq!(result["error"], "Not executed: plan submitted for review.");
        }
        let plan = agent.workflow.plan.as_ref().unwrap();
        assert_eq!(plan.revision, 1);
        assert_eq!(plan.status, crate::plan::PlanStatus::Ready);
        assert_eq!(plan.file_name.as_deref(), Some("create-canary.md"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(plan.path().as_deref().unwrap_or_default()))
                .unwrap(),
            content
        );
        assert!(dir.path().join(".pp/drafts/create-canary.md").exists());
        assert!(!dir.path().join("forbidden").exists());
        let saved: Value = serde_json::to_value(&agent.session).unwrap();
        let history = &saved["request"][if protocol == "completion" {
            "messages"
        } else {
            "input"
        }];
        let outputs = history
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["role"] == "tool" || item["type"] == "function_call_output")
            .count();
        assert_eq!(outputs, 4);
        let id = agent.session_id().unwrap().to_owned();
        let expected = agent.workflow.clone();
        drop(agent);
        let restored = persistence::load(
            &dir.path().join("sessions"),
            &dir.path().join("sessions/.locks"),
            Some(&id),
            true,
        )
        .unwrap();
        assert_eq!(restored.checkpoint.workflow, expected);
        assert!(server.try_recv().unwrap().is_none());
    }
}

#[test]
fn prepare_plan_task_allocates_only_on_first_non_blank_task() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
    seed_session(&mut agent, EXPLORATION);
    agent.workflow.mode = crate::plan::WorkMode::Plan;
    // 空待命下的空任务（continue/steering 边界）不分配文档。
    agent.prepare_plan_task("").unwrap();
    assert!(agent.workflow.plan.is_none());
    // 首条非空任务建立绑定并记录 origin。
    agent.prepare_plan_task("Research the toggle flow").unwrap();
    let plan = agent.workflow.plan.as_ref().unwrap();
    assert_eq!(plan.revision, 0);
    assert_eq!(plan.origin_prompt, "Research the toggle flow");
    // 后续空任务沿用当前绑定，不新建、不重置 origin。
    agent.prepare_plan_task("").unwrap();
    assert_eq!(
        agent.workflow.plan.as_ref().unwrap().origin_prompt,
        "Research the toggle flow"
    );
}

#[test]
fn plan_protocols_deny_side_effects_and_continue_reading() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("canary"), "original\n").unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        let mut document = crate::plan::PlanDocument::new().unwrap();
        document.origin_prompt = "Research canary".into();
        agent.workflow.mode = crate::plan::WorkMode::Plan;
        agent.workflow.plan = Some(document);
        agent
            .session
            .set_tools(agent.tools.all_definitions(agent.workflow.mode));
        let serving = protocol.to_owned();
        let worker = std::thread::spawn(move || {
            for (name, args) in [
                ("write", json!({"path":"canary","content":"changed"})),
                (
                    "edit",
                    json!({"path":"canary","edits":[{"oldText":"original","newText":"changed"}]}),
                ),
                ("bash", json!({"command":"touch marker"})),
                (
                    "bash",
                    json!({"command":"touch marker","mode":"background"}),
                ),
                ("bash_task", json!({"task_id":"unknown","action":"status"})),
                ("plan_search", json!({"query":"original","path":"canary"})),
                ("find", json!({"args":[".","-exec","touch","marker",";"]})),
                ("mcp__unseen__write", json!({})),
                ("read", json!({"path":"canary"})),
            ] {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let _: Value = serde_json::from_reader(request.as_reader()).unwrap();
                tool_call_for(&serving, request, name, &args.to_string());
            }
            let request = server
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            final_for(&serving, request, "Read-only research completed", true);
        });
        let mut results = Vec::new();
        let result = agent.run(
            "Research canary",
            &CancellationToken::new(),
            None,
            &mut |event| {
                if let AgentEvent::ToolFinished { result, .. } = event {
                    results.push(result);
                }
            },
        );
        assert_eq!(result.unwrap(), RunOutcome::Completed);
        worker.join().unwrap();
        assert_eq!(results.len(), 9);
        for result in &results[..8] {
            assert_eq!(
                *result,
                json!({"ok":false,"error":"Tool is not available in Plan mode."})
            );
        }
        assert_eq!(results[8]["content"], "original\n");
        assert_eq!(
            std::fs::read(dir.path().join("canary")).unwrap(),
            b"original\n"
        );
        assert!(!dir.path().join("marker").exists());
    }
}
