//! 五事件 hook 的可见边界：输入拒绝、参数改写、结果变换、受限路径与 Stop。

use super::*;
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

/// 用户级 `hooks.yml` 正文：只有事件顶层键。
fn hook_yaml(event: &str, script: &str) -> String {
    format!("{event}:\n  - hooks:\n      - type: command\n        command: '{script}'\n")
}

fn two_hook_yaml(first: &str, first_script: &str, second: &str, second_script: &str) -> String {
    format!(
        "{first}:\n  - hooks:\n      - type: command\n        command: '{first_script}'\n{second}:\n  - hooks:\n      - type: command\n        command: '{second_script}'\n"
    )
}

/// (call_id, text) 按原生顺序提取工具结果。
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

/// 原生 assistant 调用：名称与原始参数字符串。
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
fn hooks_user_prompt_submit_blocks_before_any_model_request() {
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
        // 拒绝首条输入可以留下有效的空 session，但不能留下 user 记录。
        let id = agent
            .session_id()
            .expect("hook 前已建立有效 session")
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

#[test]
fn hooks_steering_rejection_keeps_other_inputs_and_injects_context() {
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
            "允许的补充输入必须带上自己的上下文"
        );
        // 被拒项不写 journal：恢复后只重放真正提交的输入。
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
        // 恢复只重放已保存历史：脚本副作用计数不因读取会话增加。
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

#[test]
fn hooks_steering_hook_failure_restores_pending_input() {
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
        // 故障项留在队列里等待显式恢复，不自动重试也不丢失。
        assert_eq!(queue.snapshot_since(None).unwrap().unwrap().steering(), 1);
        assert!(queue.recall(1).unwrap().is_some());
        no_more_requests(&server);
    }
}

fn no_more_requests(server: &TestServer) {
    assert!(
        server
            .recv_timeout(Duration::from_millis(150))
            .unwrap()
            .is_none(),
        "hook 故障不得继续请求模型"
    );
}

#[test]
fn hooks_plan_mode_runs_no_scripts() {
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
        // Plan 只读路径：即使配置了同一条 hooks，也不得启动任何脚本。
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
        assert!(!marker.exists(), "Plan 模式不得运行 hook 脚本");
        let outputs = tool_outputs(protocol, &next);
        assert!(outputs[0].1.contains("REAL-CONTENT"), "{}", outputs[0].1);
    }
}

#[test]
fn hooks_pre_tool_use_rewrite_reaches_disk_events_and_native_call() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("second.txt");
        std::fs::write(&source, "REAL-CONTENT").unwrap();
        let written = dir.path().join("written.txt");
        // 改写目标是工作区相对路径：临时目录在 macOS 上是 /var 拼写，保存层
        // 规范化为 /private/var，绝对路径会被误判为工作区外。
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
        let write = serde_json::json!({"path": "target.txt", "content": "original"}).to_string();
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
        // 只有改写后的目标发生变化。
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
        // 模型结果来自改写后的调用：读到真实内容，写入报告成功。
        let outputs = tool_outputs(protocol, &next);
        assert_eq!(outputs[0].0, "call-1");
        assert!(outputs[0].1.contains("REAL-CONTENT"), "{}", outputs[0].1);
        assert!(outputs[1].1.contains("\"ok\":true"), "{}", outputs[1].1);
        // 原生 assistant 调用保存同一份有效参数。
        let calls = assistant_arguments(protocol, &next);
        assert_eq!(calls[0].0, "read");
        assert!(calls[0].1.contains("second.txt"), "{}", calls[0].1);
        assert_eq!(calls[1].0, "write");
        assert!(calls[1].1.contains("written.txt"), "{}", calls[1].1);
    }
}

#[test]
fn hooks_pre_tool_use_rewrite_is_rechecked_by_the_instruction_gate() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/AGENTS.md"), "sub rules").unwrap();
        let target = dir.path().join("sub/blocked.txt");
        // 改写目标是工作区相对路径，不受临时目录符号链接拼写影响。
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
        // 顶层规则未随请求发送，`sub` 的规则更没有进入请求快照。
        agent.tools.reset_project_instructions([]);
        let serving = protocol.to_owned();
        let worker = Arc::clone(&server);
        let arguments = serde_json::json!({"path": "plain.txt", "content": "x"}).to_string();
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
        assert!(!target.exists(), "改写后的目标也必须经过规则门禁");
        let outputs = tool_outputs(protocol, &next);
        assert!(outputs[0].1.contains("AGENTS.md"), "{}", outputs[0].1);
        assert!(outputs[0].1.contains("Not executed"), "{}", outputs[0].1);
        assert!(!dir.path().join("plain.txt").exists());
    }
}

#[test]
fn hooks_pre_tool_use_deny_blocks_even_in_yolo() {
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
        let arguments = serde_json::json!({"path": "denied.txt", "content": "x"}).to_string();
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

#[test]
fn hooks_pre_tool_use_allow_keeps_the_original_approval_denial() {
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
            "tools:\n  approvalMode: write\n",
            Some(&hook_yaml("PreToolUse", &script)),
        );
        agent.set_approval_handler(Box::new(|_request, _cancellation| {
            Ok(crate::tools::approval::ApprovalDecision::Deny)
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

#[test]
fn hooks_pre_tool_use_ask_fails_without_an_approval_handler() {
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
        let arguments = serde_json::json!({"path": "asked.txt", "content": "x"}).to_string();
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

#[test]
fn hooks_post_tool_use_splits_success_and_failure_context() {
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
        assert!(ok_marker.exists(), "成功的调用必须触发 PostToolUse");
        assert!(
            fail_marker.exists(),
            "失败的调用必须触发 PostToolUseFailure"
        );
        // 真实结果保持原样：ToolFinished 与文件的真实内容不变。
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
        // 所有工具结果必须排在 developer 上下文之前。
        let entries = match protocol {
            "completion" => next["messages"].as_array().unwrap().clone(),
            _ => next["input"].as_array().unwrap().clone(),
        };
        let last_tool = entries
            .iter()
            .rposition(|entry| entry["role"] == "tool" || entry["type"] == "function_call_output")
            .unwrap();
        let first_developer = entries
            .iter()
            .position(|entry| entry["role"] == "developer")
            .unwrap();
        assert!(last_tool < first_developer, "上下文必须在完整批次之后");
    }
}

#[test]
fn hooks_post_tool_use_transforms_only_the_model_result() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("exists.txt");
        std::fs::write(&existing, "REAL-CONTENT").unwrap();
        // 第二个 handler 试图改 `ok`：按 post 故障策略丢弃它的决定并告警。
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
            "改状态字段必须留下诊断：{diagnostics:?}"
        );
        // Tools 页的结果字符数取最终回填模型的文本，而不是被过滤掉的真实结果。
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

#[test]
fn hooks_post_tool_use_preserves_image_attachment_across_model_switch() {
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
    assert_eq!(output["kind"], "image", "附件识别字段必须保留");

    // 切到 Responses 模型：convert 会重新校验 Chat 附件与工具结果的对应关系。
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
        panic!("切换后应保存 Responses 原生历史");
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
    let async_openai::types::responses::FunctionCallOutput::Content(parts) = &output.output else {
        panic!("图片附件必须进入 Responses 工具结果");
    };
    assert!(parts.iter().any(|part| matches!(
        part,
        async_openai::types::responses::InputContent::InputImage(image)
            if image.image_url.as_deref().is_some_and(|url| url.starts_with("data:image/png;base64,"))
    )));
}

#[test]
fn hooks_do_not_run_for_human_shell_or_foreign_configs() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let script = hook_script(
        dir.path(),
        "mark.sh",
        &format!("cat > /dev/null\necho ran >> {}", marker.display()),
    );
    // Claude 的项目配置文件不是来源：pp 只读自己的 hooks.yml 与 .pp/hooks.yml。
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
        "人工 shell 与外来的 .claude 配置都不得启动脚本"
    );
}

#[test]
fn hooks_stop_requests_continuation_then_allows() {
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
        let second: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("stop-input-2.json")).unwrap())
                .unwrap();
        assert_eq!(second["stop_hook_active"], true);
        assert_eq!(second["permission_mode"], "bypassPermissions");
        let developer = developer_texts(protocol, &bodies[1]).join("\n");
        assert!(developer.contains("finish the report"), "{developer}");
    }
}

#[test]
fn slow_hooks_stop_continuation_limit_fails_the_task() {
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
        assert_eq!(requests, 9, "初始请求 + 8 次 hook 续跑");
    }
}

#[test]
fn hooks_stop_decision_precedes_steering_input() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        // 脚本先等测试线程把 Steering 放进队列，再返回 continue:false，
        // 固定“Stop 执行期间收到输入”的竞态窗口。
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
            assert!(started.exists(), "Stop 脚本必须启动");
            worker_queue
                .enqueue(
                    input::PendingKind::Steering,
                    "late steering".into(),
                    None,
                    false,
                )
                .unwrap();
            std::fs::write(&release, "").unwrap();
            // 旧逻辑会在 Stop 后继续请求模型；这里回一个回复，让多发的请求快速暴露。
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
        assert_eq!(requests, 1, "Stop 决定后不得再请求模型");
        let snapshot = queue.snapshot_since(None).unwrap().unwrap();
        assert_eq!(snapshot.steering(), 1, "补充输入必须保留为待处理");
        assert_eq!(snapshot.items[0].preview, "late steering");
    }
}

#[test]
fn hooks_pre_tool_use_context_is_injected_once() {
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
            "前置 hook 上下文只能进入历史一次：{developer}"
        );
    }
}

#[test]
fn hooks_denied_steering_does_not_start_another_request() {
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
            while let Ok(Some(request)) = worker.recv_timeout(Duration::from_millis(500)) {
                count += 1;
                final_for(&serving, request, "final reply", true);
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
            "全部补充输入被拒绝时不得再发起模型请求"
        );
        no_more_requests(&server);
    }
}

#[test]
fn hooks_halted_batch_stays_in_the_persisted_snapshot() {
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
        let arguments = serde_json::json!({"path": "halted.txt", "content": "x"}).to_string();
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
            persistence::ReplayRecord::Event(AgentEvent::ToolFinished { result, .. })
                if result["error"]
                    .as_str()
                    .is_some_and(|text| text.contains("Not executed")) =>
            {
                Some(result.clone())
            }
            _ => None,
        });
        assert!(
            card.is_some(),
            "未执行的工具卡片必须进入展示快照以便恢复回放"
        );
    }
}

#[test]
fn hooks_pre_tool_use_cancel_rolls_back_the_native_call() {
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
            "PreToolUse 脚本没有启动"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    token.cancel();
    let (mut agent, result) = runner.join().unwrap();
    serve.join().unwrap();
    // 取消以 `RunOutcome::Cancelled` 收尾，而不是工具失败或任务错误。
    assert_eq!(result.unwrap(), RunOutcome::Cancelled);
    // 取消后原生历史不能留下没有结果的 function_call。
    let Session::Response(request) = &mut agent.session else {
        panic!("response protocol fixture")
    };
    assert!(
        !super::super::turn::response_input(request)
            .iter()
            .any(|item| matches!(item, InputItem::Item(Item::FunctionCall(_)))),
        "取消必须回退本轮调用"
    );
    // 下一次任务的原生请求同样干净。
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
        "下一次请求不得携带无结果的调用：{next}"
    );
    assert_eq!(user_messages("response", &next), ["read the file", "again"]);
}

#[test]
fn hooks_cancel_during_post_tool_use_marks_skipped_calls_cancelled() {
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
                "PostToolUse 脚本没有启动"
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
                persistence::ReplayRecord::Event(AgentEvent::ToolFinished { result, .. })
                    if result["error"]
                        .as_str()
                        .is_some_and(|text| text.contains("Not executed")) =>
                {
                    Some(result.clone())
                }
                _ => None,
            })
            .expect("未执行的调用必须有卡片");
        assert_eq!(
            skipped["cancelled"].as_bool(),
            Some(true),
            "取消导致的跳过必须保留 cancelled 标记：{skipped}"
        );
    }
}

#[test]
fn hooks_stop_skipped_while_awaiting_user_answer() {
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
        assert!(!marker.exists(), "等待用户答复时不触发 Stop hook");
    }
}
