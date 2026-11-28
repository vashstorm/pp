//! Visibility boundaries of the five-event hooks: input rejection, argument rewriting, result transformation, restricted paths and Stop.

mod integration {
    use super::super::*;
    use pp_test_support::TestServer;
    use serde_json::Value;
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    fn hook_script(dir: &Path, name: &str, body: &str) -> String {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path.display().to_string()
    }

    /// Body of the user-level `hooks.yml`: only top-level event keys.
    fn hook_yaml(event: &str, script: &str) -> String {
        format!("{event}:\n  - hooks:\n      - type: command\n        command: '{script}'\n")
    }

    fn two_hook_yaml(first: &str, first_script: &str, second: &str, second_script: &str) -> String {
        format!(
            "{first}:\n  - hooks:\n      - type: command\n        command: '{first_script}'\n{second}:\n  - hooks:\n      - type: command\n        command: '{second_script}'\n"
        )
    }

    /// Extract tool results as (call_id, text) in native order.
    fn tool_outputs(protocol: &str, body: &Value) -> Vec<(String, String)> {
        match protocol {
            "completion" => body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["role"] == "tool")
                .map(|entry| {
                    (
                        entry["tool_call_id"].as_str().unwrap().to_owned(),
                        entry["content"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect(),
            _ => body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["type"] == "function_call_output")
                .map(|entry| {
                    (
                        entry["call_id"].as_str().unwrap().to_owned(),
                        entry["output"].as_str().unwrap_or_default().to_owned(),
                    )
                })
                .collect(),
        }
    }

    /// Native assistant calls: name and raw argument string.
    fn assistant_arguments(protocol: &str, body: &Value) -> Vec<(String, String)> {
        match protocol {
            "completion" => body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["role"] == "assistant")
                .flat_map(|entry| {
                    entry["tool_calls"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .map(|call| {
                            (
                                call["function"]["name"].as_str().unwrap().to_owned(),
                                call["function"]["arguments"].as_str().unwrap().to_owned(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .collect(),
            _ => body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|entry| entry["type"] == "function_call")
                .map(|entry| {
                    (
                        entry["name"].as_str().unwrap().to_owned(),
                        entry["arguments"].as_str().unwrap().to_owned(),
                    )
                })
                .collect(),
        }
    }

    fn developer_texts(protocol: &str, body: &Value) -> Vec<String> {
        let entries = match protocol {
            "completion" => body["messages"].as_array().unwrap().clone(),
            _ => body["input"].as_array().unwrap().clone(),
        };
        entries
            .into_iter()
            .filter(|entry| entry["role"] == "developer")
            .map(|entry| match &entry["content"] {
                Value::String(text) => text.clone(),
                other => panic!("unexpected developer content {other}"),
            })
            .collect()
    }

    #[test]
    fn hooks_user_prompt_submit_steering_and_plan_isolation() {
        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let script = hook_script(
                    dir.path(),
                    "deny.sh",
                    "cat > /dev/null\nprintf 'blocked by policy' >&2\nexit 2",
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("UserPromptSubmit", &script),
                );
                let error = agent
                    .run("hello", &CancellationToken::new(), None, &mut |_| {})
                    .unwrap_err();
                assert!(
                    format!("{error:#}").contains("blocked by policy"),
                    "{error:#}"
                );
                assert!(
                    server
                        .recv_timeout(Duration::from_millis(150))
                        .unwrap()
                        .is_none(),
                    "a rejected prompt must not reach the provider"
                );
                // Rejecting the first input may leave a valid empty session, but must not leave a user record.
                let id = agent
                    .session_id()
                    .expect("a valid session was established before the hook")
                    .to_owned();
                drop(agent);
                let restored = persistence::load_test(
                    &dir.path().join("sessions"),
                    &dir.path().join("sessions/.locks"),
                    Some(&id),
                    true,
                )
                .unwrap();
                assert!(
                    restored
                        .replay
                        .iter()
                        .all(|record| !matches!(record, persistence::ReplayRecord::User(_)))
                );
            }
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let counted = dir.path().join("input-runs");
                let script = hook_script(
                    dir.path(),
                    "input.sh",
                    &format!(
                        r#"payload=$(cat)
echo run >> {counted}
if [[ $payload == *"steer bad"* ]]; then
  printf 'rejected by policy' >&2
  exit 2
fi
printf '{{"hookSpecificOutput":{{"hookEventName":"UserPromptSubmit","additionalContext":"input context"}}}}'"#,
                        counted = counted.display()
                    ),
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("UserPromptSubmit", &script),
                );
                let queue = InputQueue::default();
                queue.begin_run().unwrap();
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let handle = std::thread::spawn(move || {
                    let mut bodies = Vec::new();
                    for text in ["first reply", "final reply"] {
                        let request = worker
                            .recv_timeout(Duration::from_secs(10))
                            .unwrap()
                            .unwrap();
                        bodies.push(request.json().unwrap());
                        final_for(&serving, request, text, true);
                    }
                    bodies
                });
                let mut enqueued = false;
                let mut accepted = Vec::new();
                let mut rejected = Vec::new();
                let outcome = agent
                    .run(
                        "initial prompt",
                        &CancellationToken::new(),
                        Some(&queue),
                        &mut |event| match event {
                            AgentEvent::ModelRequestStarted if !enqueued => {
                                enqueued = true;
                                queue
                                    .enqueue(
                                        input::PendingKind::Steering,
                                        "steer bad".into(),
                                        None,
                                        false,
                                    )
                                    .unwrap();
                                queue
                                    .enqueue(
                                        input::PendingKind::Steering,
                                        "steer good".into(),
                                        None,
                                        false,
                                    )
                                    .unwrap();
                            }
                            AgentEvent::UserInputAccepted { text, .. } => accepted.push(text),
                            AgentEvent::UserInputRejected { text, reason, .. } => {
                                rejected.push((text, reason))
                            }
                            _ => {}
                        },
                    )
                    .unwrap();
                assert_eq!(outcome, RunOutcome::Completed);
                assert_eq!(accepted, vec!["steer good".to_owned()]);
                assert_eq!(
                    rejected,
                    vec![("steer bad".to_owned(), "rejected by policy".to_owned())]
                );
                let bodies = handle.join().unwrap();
                assert_eq!(user_messages(protocol, &bodies[0]), ["initial prompt"]);
                assert_eq!(
                    user_messages(protocol, &bodies[1]),
                    ["initial prompt", "steer good"]
                );
                assert!(
                    developer_texts(protocol, &bodies[1])
                        .iter()
                        .filter(|text| text.contains("input context"))
                        .count()
                        >= 1,
                    "allowed supplementary input must carry its own context"
                );
                // Rejected items are not journaled: a restore replays only really committed input.
                let id = agent.session_id().unwrap().to_owned();
                drop(agent);
                let restored = persistence::load_test(
                    &dir.path().join("sessions"),
                    &dir.path().join("sessions/.locks"),
                    Some(&id),
                    true,
                )
                .unwrap();
                let users: Vec<String> = restored
                    .replay
                    .iter()
                    .filter_map(|record| match record {
                        persistence::ReplayRecord::User(text) => Some(text.clone()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(users, vec!["initial prompt", "steer good"]);
                drop(restored);
                // A restore replays only saved history: the script side-effect counter does not grow by reading the session.
                assert_eq!(
                    std::fs::read_to_string(&counted).unwrap().lines().count(),
                    3
                );
                let restored = persistence::load_test(
                    &dir.path().join("sessions"),
                    &dir.path().join("sessions/.locks"),
                    Some(&id),
                    true,
                )
                .unwrap();
                drop(restored);
                assert_eq!(
                    std::fs::read_to_string(&counted).unwrap().lines().count(),
                    3
                );
            }
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let script = hook_script(
                    dir.path(),
                    "boom.sh",
                    r#"payload=$(cat)
if [[ $payload == *"steer boom"* ]]; then
  exit 7
fi"#,
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("UserPromptSubmit", &script),
                );
                let queue = InputQueue::default();
                queue.begin_run().unwrap();
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let handle = std::thread::spawn(move || {
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    final_for(&serving, request, "first reply", true);
                });
                let mut enqueued = false;
                let error = agent
                    .run(
                        "initial prompt",
                        &CancellationToken::new(),
                        Some(&queue),
                        &mut |event| {
                            if let AgentEvent::ModelRequestStarted = event
                                && !enqueued
                            {
                                enqueued = true;
                                queue
                                    .enqueue(
                                        input::PendingKind::Steering,
                                        "steer boom".into(),
                                        None,
                                        false,
                                    )
                                    .unwrap();
                            }
                        },
                    )
                    .unwrap_err();
                handle.join().unwrap();
                assert!(format!("{error:#}").contains("status 7"), "{error:#}");
                // A failed item stays in the queue awaiting explicit recovery; it is neither retried automatically nor lost.
                assert_eq!(queue.snapshot_since(None).unwrap().unwrap().steering(), 1);
                assert!(queue.recall(1).unwrap().is_some());
                no_more_requests(&server);
            }
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("exists.txt"), "REAL-CONTENT").unwrap();
                let marker = dir.path().join("ran");
                let script = hook_script(
                    dir.path(),
                    "mark.sh",
                    &format!(
                        "cat > /dev/null
echo ran >> {}",
                        marker.display()
                    ),
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("PreToolUse", &script),
                );
                // Plan read-only path: even with the same hooks configured, no script may start.
                agent.workflow.mode = pp_core::plan::WorkMode::Plan;
                agent.workflow.plan = Some(pp_core::plan::PlanDocument::new().unwrap());
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let read = serde_json::json!({"path": "exists.txt"}).to_string();
                let handle = std::thread::spawn(move || {
                    let batch = vec![("read", read.as_str())];
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    tool_batch_for(&serving, request, &batch);
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    let body = request.json().unwrap();
                    final_for(&serving, request, "done", true);
                    body
                });
                agent
                    .run(
                        "inspect the tree",
                        &CancellationToken::new(),
                        None,
                        &mut |_| {},
                    )
                    .unwrap();
                let next = handle.join().unwrap();
                assert!(!marker.exists(), "Plan mode must not run hook scripts");
                let outputs = tool_outputs(protocol, &next);
                assert!(outputs[0].1.contains("REAL-CONTENT"), "{}", outputs[0].1);
            }
        }
    }

    fn no_more_requests(server: &TestServer) {
        assert!(
            server
                .recv_timeout(Duration::from_millis(150))
                .unwrap()
                .is_none(),
            "a hook failure must not fall through to a model request"
        );
    }

    #[test]
    fn hooks_pre_tool_use_rewrite_permission_and_human_isolation() {
        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let source = dir.path().join("second.txt");
                std::fs::write(&source, "REAL-CONTENT").unwrap();
                let written = dir.path().join("written.txt");
                // The rewrite target is a workspace-relative path: the temporary directory is spelled /var on macOS, while the save
                // layer canonicalizes it to /private/var, so an absolute path would be misjudged as outside the workspace.
                let read_script = hook_script(
                    dir.path(),
                    "read-rewrite.sh",
                    "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"updatedInput\":{\"path\":\"second.txt\"}}}'",
                );
                let write_script = hook_script(
                    dir.path(),
                    "write-rewrite.sh",
                    "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"updatedInput\":{\"path\":\"written.txt\",\"content\":\"rewritten\"}}}'",
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &format!(
                        "PreToolUse:\n  - matcher: read\n    hooks:\n      - type: command\n        command: '{read_script}'\n  - matcher: write\n    hooks:\n      - type: command\n        command: '{write_script}'\n"
                    ),
                );
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let read = serde_json::json!({"path": "first.txt"}).to_string();
                let write =
                    serde_json::json!({"path": "target.txt", "content": "original"}).to_string();
                let handle = std::thread::spawn(move || {
                    let batch = vec![("read", read.as_str()), ("write", write.as_str())];
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    tool_batch_for(&serving, request, &batch);
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    let body = request.json().unwrap();
                    final_for(&serving, request, "done", true);
                    body
                });
                let mut started_arguments = Vec::new();
                agent
                    .run(
                        "touch two files",
                        &CancellationToken::new(),
                        None,
                        &mut |event| {
                            if let AgentEvent::ToolStarted { arguments, .. } = event {
                                started_arguments.push(arguments);
                            }
                        },
                    )
                    .unwrap();
                let next = handle.join().unwrap();
                // Only the rewritten target changes.
                assert!(!dir.path().join("target.txt").exists());
                assert!(!dir.path().join("first.txt").exists());
                assert_eq!(std::fs::read_to_string(&written).unwrap(), "rewritten");
                assert_eq!(started_arguments.len(), 2);
                assert!(
                    started_arguments[0].contains("second.txt"),
                    "{}",
                    started_arguments[0]
                );
                assert!(
                    started_arguments[1].contains("written.txt"),
                    "{}",
                    started_arguments[1]
                );
                // The model result comes from the rewritten call: it reads the real content and reports the write as successful.
                let outputs = tool_outputs(protocol, &next);
                assert_eq!(outputs[0].0, "call-1");
                assert!(outputs[0].1.contains("REAL-CONTENT"), "{}", outputs[0].1);
                assert!(outputs[1].1.contains("\"ok\":true"), "{}", outputs[1].1);
                // The native assistant call stores the same effective arguments.
                let calls = assistant_arguments(protocol, &next);
                assert_eq!(calls[0].0, "read");
                assert!(calls[0].1.contains("second.txt"), "{}", calls[0].1);
                assert_eq!(calls[1].0, "write");
                assert!(calls[1].1.contains("written.txt"), "{}", calls[1].1);
            }
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                std::fs::create_dir_all(dir.path().join("sub")).unwrap();
                std::fs::write(dir.path().join("sub/AGENTS.md"), "sub rules").unwrap();
                let target = dir.path().join("sub/blocked.txt");
                // The rewrite target is a workspace-relative path and is unaffected by the temporary directory's symlink spelling.
                let script = hook_script(
                    dir.path(),
                    "rewrite.sh",
                    "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"updatedInput\":{\"path\":\"sub/blocked.txt\",\"content\":\"x\"}}}'",
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("PreToolUse", &script),
                );
                // The top-level rule was not sent with the request, and `sub`'s rule certainly did not enter the request snapshot.
                agent.tools.reset_project_instructions([]);
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let arguments =
                    serde_json::json!({"path": "plain.txt", "content": "x"}).to_string();
                let handle = std::thread::spawn(move || {
                    let batch = vec![("write", arguments.as_str())];
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    tool_batch_for(&serving, request, &batch);
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    let body = request.json().unwrap();
                    final_for(&serving, request, "done", true);
                    body
                });
                agent
                    .run("write a file", &CancellationToken::new(), None, &mut |_| {})
                    .unwrap();
                let next = handle.join().unwrap();
                assert!(
                    !target.exists(),
                    "the rewritten target must also pass the rule gate"
                );
                let outputs = tool_outputs(protocol, &next);
                assert!(outputs[0].1.contains("AGENTS.md"), "{}", outputs[0].1);
                assert!(outputs[0].1.contains("Not executed"), "{}", outputs[0].1);
                assert!(!dir.path().join("plain.txt").exists());
            }
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let script = hook_script(
                    dir.path(),
                    "deny.sh",
                    "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"policy says no\"}}'",
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("PreToolUse", &script),
                );
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let arguments =
                    serde_json::json!({"path": "denied.txt", "content": "x"}).to_string();
                let handle = std::thread::spawn(move || {
                    let batch = vec![("write", arguments.as_str())];
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    tool_batch_for(&serving, request, &batch);
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    let body = request.json().unwrap();
                    final_for(&serving, request, "done", true);
                    body
                });
                agent
                    .run("write", &CancellationToken::new(), None, &mut |_| {})
                    .unwrap();
                let next = handle.join().unwrap();
                assert!(!dir.path().join("denied.txt").exists());
                let outputs = tool_outputs(protocol, &next);
                assert!(outputs[0].1.contains("policy says no"), "{}", outputs[0].1);
                assert!(outputs[0].1.contains("\"ok\":false"), "{}", outputs[0].1);
            }
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let script = hook_script(
                    dir.path(),
                    "allow.sh",
                    "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"allow\"}}'",
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    "",
                    Some(&hook_yaml("PreToolUse", &script)),
                );
                agent
                    .permission_gate()
                    .select(Some(pp_config::config::PermissionMode::ReadOnly));
                agent.set_permission_handler(Box::new(|_request, _cancellation| {
                    Ok(pp_tools::permission::PermissionDecision::Deny)
                }));
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let handle = std::thread::spawn(move || {
                    let batch = vec![("bash", r#"{"command":"true"}"#)];
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    tool_batch_for(&serving, request, &batch);
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    let body = request.json().unwrap();
                    final_for(&serving, request, "done", true);
                    body
                });
                agent
                    .run(
                        "run a command",
                        &CancellationToken::new(),
                        None,
                        &mut |_| {},
                    )
                    .unwrap();
                let next = handle.join().unwrap();
                let outputs = tool_outputs(protocol, &next);
                assert!(
                    outputs[0].1.contains("denied by the user"),
                    "{}",
                    outputs[0].1
                );
            }
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let script = hook_script(
                    dir.path(),
                    "ask.sh",
                    "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"ask\",\"permissionDecisionReason\":\"confirm this write\"}}'",
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("PreToolUse", &script),
                );
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let arguments =
                    serde_json::json!({"path": "asked.txt", "content": "x"}).to_string();
                let handle = std::thread::spawn(move || {
                    let batch = vec![("write", arguments.as_str())];
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    tool_batch_for(&serving, request, &batch);
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    let body = request.json().unwrap();
                    final_for(&serving, request, "done", true);
                    body
                });
                agent
                    .run("write", &CancellationToken::new(), None, &mut |_| {})
                    .unwrap();
                let next = handle.join().unwrap();
                assert!(!dir.path().join("asked.txt").exists());
                let outputs = tool_outputs(protocol, &next);
                assert!(
                    outputs[0].1.contains("no interactive UI"),
                    "{}",
                    outputs[0].1
                );
            }
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("exists.txt"), "REAL-CONTENT").unwrap();
                let script = hook_script(
                    dir.path(),
                    "context.sh",
                    "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"additionalContext\":\"ONLY-ONCE-CONTEXT\"}}'",
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("PreToolUse", &script),
                );
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let read = serde_json::json!({"path": "exists.txt"}).to_string();
                let handle = std::thread::spawn(move || {
                    let batch = vec![("read", read.as_str())];
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    tool_batch_for(&serving, request, &batch);
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    let body = request.json().unwrap();
                    final_for(&serving, request, "done", true);
                    body
                });
                agent
                    .run("read", &CancellationToken::new(), None, &mut |_| {})
                    .unwrap();
                let next = handle.join().unwrap();
                let developer = developer_texts(protocol, &next).join("\n");
                assert_eq!(
                    developer.matches("ONLY-ONCE-CONTEXT").count(),
                    1,
                    "pre-hook context may enter history only once: {developer}"
                );
            }
        }

        {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("ran");
            let script = hook_script(
                dir.path(),
                "mark.sh",
                &format!("cat > /dev/null\necho ran >> {}", marker.display()),
            );
            // Claude's project config files are not a source: pp reads only its own hooks.yml and .pp/hooks.yml.
            std::fs::create_dir_all(dir.path().join(".claude")).unwrap();
            std::fs::write(
            dir.path().join(".claude/settings.json"),
            serde_json::json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [{"type": "command", "command": script}]}]}})
                .to_string(),
        )
        .unwrap();
            let mut agent = runtime_input_agent(
                dir.path(),
                "completion",
                "127.0.0.1:1",
                &hook_yaml("PreToolUse", &script),
            );
            agent
                .run_shell("true", "shell-1", &CancellationToken::new(), &mut |_| {})
                .unwrap();
            assert!(
                !marker.exists(),
                "neither the manual shell nor the foreign .claude config may start scripts"
            );
        }
    }

    #[test]
    fn hooks_post_tool_use_context_transform_and_image_switch() {
        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let existing = dir.path().join("exists.txt");
                std::fs::write(&existing, "REAL-CONTENT").unwrap();
                let ok_marker = dir.path().join("post-ok");
                let fail_marker = dir.path().join("post-fail");
                let ok = hook_script(
                    dir.path(),
                    "post-ok.sh",
                    &format!(
                        "cat > /dev/null\necho ok >> {}\nprintf '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PostToolUse\",\"additionalContext\":\"post success context\"}}}}'",
                        ok_marker.display()
                    ),
                );
                let fail = hook_script(
                    dir.path(),
                    "post-fail.sh",
                    &format!(
                        "cat > /dev/null\necho fail >> {}\nprintf '{{\"hookSpecificOutput\":{{\"hookEventName\":\"PostToolUseFailure\",\"additionalContext\":\"post failure context\"}}}}'",
                        fail_marker.display()
                    ),
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &two_hook_yaml("PostToolUse", &ok, "PostToolUseFailure", &fail),
                );
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let read = serde_json::json!({"path": "exists.txt"}).to_string();
                let missing = serde_json::json!({"path": "missing.txt"}).to_string();
                let handle = std::thread::spawn(move || {
                    let batch = vec![("read", read.as_str()), ("read", missing.as_str())];
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    tool_batch_for(&serving, request, &batch);
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    let body = request.json().unwrap();
                    final_for(&serving, request, "done", true);
                    body
                });
                let mut finished = Vec::new();
                agent
                    .run(
                        "read two files",
                        &CancellationToken::new(),
                        None,
                        &mut |event| {
                            if let AgentEvent::ToolFinished { result, .. } = event {
                                finished.push(result);
                            }
                        },
                    )
                    .unwrap();
                let next = handle.join().unwrap();
                assert!(
                    ok_marker.exists(),
                    "a successful call must fire PostToolUse"
                );
                assert!(
                    fail_marker.exists(),
                    "a failed call must fire PostToolUseFailure"
                );
                // The real result stays as is: ToolFinished and the file's real content are unchanged.
                assert_eq!(finished[0]["content"], "REAL-CONTENT");
                assert_eq!(finished[1]["ok"], false);
                assert_eq!(std::fs::read_to_string(&existing).unwrap(), "REAL-CONTENT");
                let outputs = tool_outputs(protocol, &next);
                assert!(outputs[0].1.contains("REAL-CONTENT"), "{}", outputs[0].1);
                assert!(outputs[1].1.contains("\"ok\":false"), "{}", outputs[1].1);
                let developer = developer_texts(protocol, &next);
                let joined = developer.join("\n");
                assert!(joined.contains("post success context"), "{joined}");
                assert!(joined.contains("post failure context"), "{joined}");
                // All tool results must precede the developer context.
                let entries = match protocol {
                    "completion" => next["messages"].as_array().unwrap().clone(),
                    _ => next["input"].as_array().unwrap().clone(),
                };
                let last_tool = entries
                    .iter()
                    .rposition(|entry| {
                        entry["role"] == "tool" || entry["type"] == "function_call_output"
                    })
                    .unwrap();
                let first_developer = entries
                    .iter()
                    .position(|entry| entry["role"] == "developer")
                    .unwrap();
                assert!(
                    last_tool < first_developer,
                    "context must come after the complete batch"
                );
            }
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let existing = dir.path().join("exists.txt");
                std::fs::write(&existing, "REAL-CONTENT").unwrap();
                // The second handler tries to change `ok`: its decision is discarded per the post failure policy and a warning is logged.
                let script = hook_script(
                    dir.path(),
                    "transform.sh",
                    "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"updatedToolOutput\":{\"ok\":true,\"content\":\"REPLACED\"}}}'",
                );
                let bad = hook_script(
                    dir.path(),
                    "bad.sh",
                    "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"updatedToolOutput\":{\"ok\":false,\"content\":\"CORRUPT\"}}}'",
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &format!(
                        "PostToolUse:\n  - hooks:\n      - type: command\n        command: '{script}'\n      - type: command\n        command: '{bad}'\n",
                    ),
                );
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let read = serde_json::json!({"path": "exists.txt"}).to_string();
                let handle = std::thread::spawn(move || {
                    let batch = vec![("read", read.as_str())];
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    tool_batch_for(&serving, request, &batch);
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    let body = request.json().unwrap();
                    final_for(&serving, request, "done", true);
                    body
                });
                let mut finished = Vec::new();
                let mut diagnostics = Vec::new();
                agent
                    .run(
                        "read",
                        &CancellationToken::new(),
                        None,
                        &mut |event| match event {
                            AgentEvent::ToolFinished { result, .. } => finished.push(result),
                            AgentEvent::HookDiagnostic { message, .. } => diagnostics.push(message),
                            _ => {}
                        },
                    )
                    .unwrap();
                let next = handle.join().unwrap();
                assert_eq!(finished[0]["content"], "REAL-CONTENT");
                assert_eq!(std::fs::read_to_string(&existing).unwrap(), "REAL-CONTENT");
                let outputs = tool_outputs(protocol, &next);
                assert!(outputs[0].1.contains("REPLACED"), "{}", outputs[0].1);
                assert!(!outputs[0].1.contains("CORRUPT"), "{}", outputs[0].1);
                assert!(!outputs[0].1.contains("REAL-CONTENT"), "{}", outputs[0].1);
                assert!(
                    diagnostics.iter().any(|message| message.contains("ok")),
                    "changing a status field must leave a diagnostic: {diagnostics:?}"
                );
                // The Tools page result character count follows the text finally fed back to the model, not the filtered-out real result.
                let reported = outputs[0].1.chars().count() as u64;
                let database = agent.journal.stats_database().to_owned();
                drop(agent);
                let stored: i64 = rusqlite::Connection::open(database)
                    .unwrap()
                    .query_row(
                        "SELECT result_chars FROM tool_calls WHERE call_id='call-1'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(stored as u64, reported);
            }
        }

        {
            let dir = tempfile::tempdir().unwrap();
            image::DynamicImage::new_rgb8(1, 1)
                .save(dir.path().join("fixture.png"))
                .unwrap();
            let script = hook_script(
                dir.path(),
                "filter-image.sh",
                "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"updatedToolOutput\":{\"content\":\"filtered\"}}}'",
            );
            let server = Arc::new(TestServer::start().unwrap());
            let mut agent = runtime_input_agent(
                dir.path(),
                "completion",
                &server.addr().to_string(),
                &hook_yaml("PostToolUse", &script),
            );
            let worker = Arc::clone(&server);
            let arguments = serde_json::json!({"path": "fixture.png"}).to_string();
            let handle = std::thread::spawn(move || {
                let request = worker
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                tool_batch_for("completion", request, &[("read", arguments.as_str())]);
                let request = worker
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                let body = request.json().unwrap();
                final_for("completion", request, "done", true);
                body
            });
            agent
                .run(
                    "look at the image",
                    &CancellationToken::new(),
                    None,
                    &mut |_| {},
                )
                .unwrap();
            let next = handle.join().unwrap();
            let outputs = tool_outputs("completion", &next);
            let output: Value = serde_json::from_str(&outputs[0].1).unwrap();
            assert_eq!(output["content"], "filtered");
            assert_eq!(
                output["kind"], "image",
                "the attachment discriminator must be kept"
            );

            // Switch to a Responses model: convert re-validates the correspondence between Chat attachments and tool results.
            let path = dir.path().join("models.yml");
            let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            models["providers"]["other"] = json!({
                "api_key": "fixture-key",
                "protocol": "response",
                "base_url": format!("http://{}/v1", server.addr()),
                "models": {"two": {"model": "two"}}
            });
            std::fs::write(&path, models.to_string()).unwrap();
            agent.reload_models().unwrap();
            agent.switch_model("other/two").unwrap();
            let Session::Response(request) = &agent.session else {
                panic!("the switch should keep Responses native history");
            };
            let InputParam::Items(items) = &request.input else {
                panic!()
            };
            let output = items
                .iter()
                .find_map(|item| match item {
                    InputItem::Item(Item::FunctionCallOutput(output))
                        if output.call_id.as_deref() == Some("call-1") =>
                    {
                        Some(output)
                    }
                    _ => None,
                })
                .unwrap();
            let async_openai::types::responses::FunctionCallOutput::Content(parts) = &output.output
            else {
                panic!("the image attachment must enter the Responses tool result");
            };
            assert!(parts.iter().any(|part| matches!(
            part,
            async_openai::types::responses::InputContent::InputImage(image)
                if image.image_url.as_deref().is_some_and(|url| url.starts_with("data:image/png;base64,"))
        )));
        }
    }

    #[test]
    fn hooks_stop_continuation_precedence_and_awaiting_skip() {
        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let once = dir.path().join("once");
                let input = dir.path().join("stop-input-1.json");
                let script = hook_script(
                    dir.path(),
                    "stop.sh",
                    &format!(
                        r#"if [ -e {once} ]; then
  cat > {second}
  exit 0
fi
touch {once}
cat > {first}
printf '{{"decision":"block","reason":"finish the report"}}'"#,
                        once = once.display(),
                        first = input.display(),
                        second = dir.path().join("stop-input-2.json").display(),
                    ),
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("Stop", &script),
                );
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let handle = std::thread::spawn(move || {
                    let mut bodies = Vec::new();
                    for text in ["first reply", "second reply"] {
                        let request = worker
                            .recv_timeout(Duration::from_secs(10))
                            .unwrap()
                            .unwrap();
                        bodies.push(request.json().unwrap());
                        final_for(&serving, request, text, true);
                    }
                    bodies
                });
                let outcome = agent
                    .run("do the task", &CancellationToken::new(), None, &mut |_| {})
                    .unwrap();
                assert_eq!(outcome, RunOutcome::Completed);
                let bodies = handle.join().unwrap();
                assert_eq!(bodies.len(), 2);
                let first: Value = serde_json::from_slice(&std::fs::read(&input).unwrap()).unwrap();
                assert_eq!(first["stop_hook_active"], false);
                assert_eq!(first["last_assistant_message"], "first reply");
                let second: Value = serde_json::from_slice(
                    &std::fs::read(dir.path().join("stop-input-2.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(second["stop_hook_active"], true);
                assert_eq!(second["permission_mode"], "bypassPermissions");
                let developer = developer_texts(protocol, &bodies[1]).join("\n");
                assert!(developer.contains("finish the report"), "{developer}");
            }
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                // The script first waits for the test thread to enqueue the Steering and then returns continue:false,
                // pinning the race window of "input received while Stop runs".
                let started = dir.path().join("stop-started");
                let release = dir.path().join("stop-release");
                let script = hook_script(
                    dir.path(),
                    "stop-deny.sh",
                    &format!(
                        "cat > /dev/null\ntouch {started}\nfor i in $(seq 1 500); do\n  [ -e {release} ] && break\n  sleep 0.01\ndone\nprintf '{{\"continue\":false,\"stopReason\":\"stop wins\"}}'",
                        started = started.display(),
                        release = release.display(),
                    ),
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("Stop", &script),
                );
                let queue = InputQueue::default();
                queue.begin_run().unwrap();
                let worker_queue = queue.clone();
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let handle = std::thread::spawn(move || {
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    final_for(&serving, request, "final reply", true);
                    let deadline = std::time::Instant::now() + Duration::from_secs(5);
                    while !started.exists() && std::time::Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    assert!(started.exists(), "the Stop script must start");
                    worker_queue
                        .enqueue(
                            input::PendingKind::Steering,
                            "late steering".into(),
                            None,
                            false,
                        )
                        .unwrap();
                    std::fs::write(&release, "").unwrap();
                    // The old logic kept requesting the model after Stop; reply here so the extra request surfaces quickly.
                    let mut count = 1;
                    while let Ok(Some(request)) = worker.recv_timeout(Duration::from_millis(500)) {
                        count += 1;
                        final_for(&serving, request, "final reply", true);
                    }
                    count
                });
                let error = agent
                    .run(
                        "do the task",
                        &CancellationToken::new(),
                        Some(&queue),
                        &mut |_| {},
                    )
                    .unwrap_err();
                let requests = handle.join().unwrap();
                assert!(format!("{error:#}").contains("stop wins"), "{error:#}");
                assert_eq!(
                    requests, 1,
                    "no further model request after a Stop decision"
                );
                let snapshot = queue.snapshot_since(None).unwrap().unwrap();
                assert_eq!(
                    snapshot.steering(),
                    1,
                    "supplementary input must stay pending"
                );
                assert_eq!(snapshot.items[0].preview, "late steering");
            }
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let script = hook_script(
                    dir.path(),
                    "deny-steering.sh",
                    r#"payload=$(cat)
if [[ $payload == *"steer bad"* ]]; then
  printf 'rejected by policy' >&2
  exit 2
fi"#,
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("UserPromptSubmit", &script),
                );
                let queue = InputQueue::default();
                queue.begin_run().unwrap();
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let handle = std::thread::spawn(move || {
                    let mut count = 0;
                    if let Ok(Some(request)) = worker.recv_timeout(Duration::from_secs(10)) {
                        count += 1;
                        final_for(&serving, request, "final reply", true);
                        while let Ok(Some(request)) =
                            worker.recv_timeout(Duration::from_millis(500))
                        {
                            count += 1;
                            final_for(&serving, request, "final reply", true);
                        }
                    }
                    count
                });
                let mut enqueued = false;
                let mut rejected = Vec::new();
                let outcome = agent
                    .run(
                        "initial prompt",
                        &CancellationToken::new(),
                        Some(&queue),
                        &mut |event| match event {
                            AgentEvent::ModelRequestStarted if !enqueued => {
                                enqueued = true;
                                queue
                                    .enqueue(
                                        input::PendingKind::Steering,
                                        "steer bad".into(),
                                        None,
                                        false,
                                    )
                                    .unwrap();
                            }
                            AgentEvent::UserInputRejected { text, .. } => rejected.push(text),
                            _ => {}
                        },
                    )
                    .unwrap();
                assert_eq!(outcome, RunOutcome::Completed);
                assert_eq!(rejected, vec!["steer bad".to_owned()]);
                assert_eq!(
                    handle.join().unwrap(),
                    1,
                    "no model request may be issued once every supplementary input is rejected"
                );
                no_more_requests(&server);
            }
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let marker = dir.path().join("stop-ran");
                let script = hook_script(
                    dir.path(),
                    "stop.sh",
                    &format!("cat > /dev/null\ntouch {}", marker.display()),
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("Stop", &script),
                );
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let handle = std::thread::spawn(move || {
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    final_for(&serving, request, "Should I continue with option A?", true);
                });
                let outcome = agent
                    .run("ask me", &CancellationToken::new(), None, &mut |_| {})
                    .unwrap();
                assert_eq!(outcome, RunOutcome::Completed);
                handle.join().unwrap();
                assert!(
                    !marker.exists(),
                    "the Stop hook does not fire while awaiting the user's answer"
                );
            }
        }
    }

    #[test]
    fn hooks_stop_continuation_limit_fails_the_task() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let script = hook_script(
                dir.path(),
                "stop.sh",
                "cat > /dev/null\nprintf '{\"decision\":\"block\",\"reason\":\"not done\"}'",
            );
            let server = Arc::new(TestServer::start().unwrap());
            let mut agent = runtime_input_agent(
                dir.path(),
                protocol,
                &server.addr().to_string(),
                &hook_yaml("Stop", &script),
            );
            let serving = protocol.to_owned();
            let worker = Arc::clone(&server);
            let handle = std::thread::spawn(move || {
                let mut count = 0;
                while let Ok(Some(request)) = worker.recv_timeout(Duration::from_secs(10)) {
                    count += 1;

                    final_for(&serving, request, "The build completed successfully.", true);
                }
                count
            });
            let error = agent
                .run("do the task", &CancellationToken::new(), None, &mut |_| {})
                .unwrap_err();
            let requests = handle.join().unwrap();
            assert!(
                format!("{error:#}").contains("Stop hook continuation limit reached (8)"),
                "{error:#}"
            );
            assert_eq!(requests, 9, "initial request + 8 hook continuations");
        }
    }

    #[test]
    fn hooks_halt_and_cancellation_rollback() {
        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                let script = hook_script(
                    dir.path(),
                    "halt.sh",
                    "cat > /dev/null\nprintf '{\"continue\":false,\"stopReason\":\"halted by policy\"}'",
                );
                let server = Arc::new(TestServer::start().unwrap());
                let mut agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("PreToolUse", &script),
                );
                let serving = protocol.to_owned();
                let worker = Arc::clone(&server);
                let arguments =
                    serde_json::json!({"path": "halted.txt", "content": "x"}).to_string();
                let handle = std::thread::spawn(move || {
                    let batch = vec![("write", arguments.as_str())];
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    tool_batch_for(&serving, request, &batch);
                });
                let error = agent
                    .run("write", &CancellationToken::new(), None, &mut |_| {})
                    .unwrap_err();
                handle.join().unwrap();
                assert!(
                    format!("{error:#}").contains("halted by policy"),
                    "{error:#}"
                );
                assert!(!dir.path().join("halted.txt").exists());
                let id = agent.session_id().unwrap().to_owned();
                drop(agent);
                let restored = persistence::load_test(
                    &dir.path().join("sessions"),
                    &dir.path().join("sessions/.locks"),
                    Some(&id),
                    true,
                )
                .unwrap();
                let card = restored.replay.iter().find_map(|record| match record {
                    persistence::ReplayRecord::Event(AgentEvent::ToolFinished {
                        result, ..
                    }) if result["error"]
                        .as_str()
                        .is_some_and(|text| text.contains("Not executed")) =>
                    {
                        Some(result.clone())
                    }
                    _ => None,
                });
                assert!(
                    card.is_some(),
                    "cards for calls that never ran must enter the display snapshot so a restore can replay them"
                );
            }
        }

        {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("exists.txt"), "REAL-CONTENT").unwrap();
            let started = dir.path().join("hook-started");
            let script = hook_script(
                dir.path(),
                "wait.sh",
                &format!(
                    "cat > /dev/null\ntouch {}\nwhile true; do sleep 0.05; done",
                    started.display()
                ),
            );
            let server = Arc::new(TestServer::start().unwrap());
            let agent = runtime_input_agent(
                dir.path(),
                "response",
                &server.addr().to_string(),
                &hook_yaml("PreToolUse", &script),
            );
            let read = serde_json::json!({"path": "exists.txt"}).to_string();
            let worker = Arc::clone(&server);
            let serve = std::thread::spawn(move || {
                let batch = vec![("read", read.as_str())];
                let request = worker
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                tool_batch_for("response", request, &batch);
            });
            let token = CancellationToken::new();
            let cancelled = token.clone();
            let runner = std::thread::spawn(move || {
                let mut agent = agent;
                let result = agent.run("read the file", &cancelled, None, &mut |_| {});
                (agent, result)
            });
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !started.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the PreToolUse script did not start"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            token.cancel();
            let (mut agent, result) = runner.join().unwrap();
            serve.join().unwrap();
            // Cancellation ends as `RunOutcome::Cancelled`, not as a tool failure or task error.
            assert_eq!(result.unwrap(), RunOutcome::Cancelled);
            // After cancellation the native history must not keep a function_call without a result.
            let Session::Response(request) = &mut agent.session else {
                panic!("response protocol fixture")
            };
            assert!(
                !super::super::super::turn::response_input(request)
                    .iter()
                    .any(|item| matches!(item, InputItem::Item(Item::FunctionCall(_)))),
                "cancellation must roll back this turn's calls"
            );
            // The native request of the next task is equally clean.
            let worker = Arc::clone(&server);
            let handle = std::thread::spawn(move || {
                let request = worker
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                let body = request.json().unwrap();
                final_for("response", request, "done", true);
                body
            });
            let outcome = agent
                .run("again", &CancellationToken::new(), None, &mut |_| {})
                .unwrap();
            assert_eq!(outcome, RunOutcome::Completed);
            let next = handle.join().unwrap();
            assert!(
                !next["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item["type"] == "function_call"),
                "the next request must not carry a call without a result: {next}"
            );
            assert_eq!(user_messages("response", &next), ["read the file", "again"]);
        }

        {
            for protocol in ["completion", "response"] {
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("first.txt"), "FIRST").unwrap();
                std::fs::write(dir.path().join("second.txt"), "SECOND").unwrap();
                let started = dir.path().join("hook-started");
                let script = hook_script(
                    dir.path(),
                    "wait.sh",
                    &format!(
                        "cat > /dev/null\ntouch {}\nwhile true; do sleep 0.05; done",
                        started.display()
                    ),
                );
                let server = Arc::new(TestServer::start().unwrap());
                let agent = runtime_input_agent(
                    dir.path(),
                    protocol,
                    &server.addr().to_string(),
                    &hook_yaml("PostToolUse", &script),
                );
                let first = serde_json::json!({"path": "first.txt"}).to_string();
                let second = serde_json::json!({"path": "second.txt"}).to_string();
                let worker = Arc::clone(&server);
                let serving = protocol.to_owned();
                let serve = std::thread::spawn(move || {
                    let batch = vec![("read", first.as_str()), ("read", second.as_str())];
                    let request = worker
                        .recv_timeout(Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    tool_batch_for(&serving, request, &batch);
                });
                let token = CancellationToken::new();
                let cancelled = token.clone();
                let runner = std::thread::spawn(move || {
                    let mut agent = agent;
                    let result = agent.run("read both files", &cancelled, None, &mut |_| {});
                    (agent, result)
                });
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while !started.exists() {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "the PostToolUse script did not start"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                token.cancel();
                let (agent, result) = runner.join().unwrap();
                serve.join().unwrap();
                assert_eq!(result.unwrap(), RunOutcome::Cancelled);
                let id = agent.session_id().unwrap().to_owned();
                drop(agent);
                let restored = persistence::load_test(
                    &dir.path().join("sessions"),
                    &dir.path().join("sessions/.locks"),
                    Some(&id),
                    true,
                )
                .unwrap();
                let skipped = restored
                    .replay
                    .iter()
                    .find_map(|record| match record {
                        persistence::ReplayRecord::Event(AgentEvent::ToolFinished {
                            result,
                            ..
                        }) if result["error"]
                            .as_str()
                            .is_some_and(|text| text.contains("Not executed")) =>
                        {
                            Some(result.clone())
                        }
                        _ => None,
                    })
                    .expect("a call that did not run must have a card");
                assert_eq!(
                    skipped["cancelled"].as_bool(),
                    Some(true),
                    "skips caused by cancellation must keep the cancelled marker: {skipped}"
                );
            }
        }
    }
}
