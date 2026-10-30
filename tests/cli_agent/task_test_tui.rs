// Debug 构建的 /task-test：回显命令本身、显示子 Agent 摘要并真实派发固定批次。
fn task_test_tui_roles() -> String {
    format!(
        "{}model_roles:\n  fast: {{model: fixture/primary, effort: none}}\n  slow: {{model: fixture/primary, effort: none}}\n  review: {{model: fixture/primary, effort: none}}\n",
        fixture_settings()
    )
}

fn task_test_tui_user_text(protocol: &str, body: &Value) -> String {
    let items = if protocol == "completion" {
        body["messages"].as_array()
    } else {
        body["input"].as_array()
    };
    items
        .unwrap()
        .iter()
        .filter(|item| item["role"] == "user")
        .map(slash_message_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn task_test_tui_child(protocol: &str, body: &Value) -> bool {
    let system = if protocol == "completion" {
        body["messages"][0].to_string()
    } else {
        body["instructions"].to_string()
    };
    system.contains("You are subagent")
}

#[test]
fn task_test_tui_echoes_command_and_shows_three_subagents() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            task_test_tui_roles(),
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("/task-test\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        // 回显命令本身，长提示词只作为展开正文发送。
        pty.wait_for("用户块回显 /task-test 而非展开提示词", |screen| {
            let text = screen.contents();
            text.contains("/task-test") && !text.contains("Debug build self-test")
        });
        let text = task_test_tui_user_text(protocol, &body);
        for marker in [
            "fast-rust-count",
            "slow-symbol-lookup",
            "review-events-risk",
            "\"role\":\"fast\"",
            "\"role\":\"slow\"",
        ] {
            assert!(text.contains(marker), "missing {marker} in {text}");
        }
        let batch = json!({
            "context": "/task-test debug scenario: read-only inspection of the current repository.",
            "tasks": [
                {"name": "fast-rust-count", "agent": "scout", "role": "fast", "task": "Count regular Rust source files."},
                {"name": "slow-symbol-lookup", "agent": "scout", "role": "slow", "task": "Find where AgentEvent is defined."},
                {"name": "review-events-risk", "agent": "reviewer", "task": "Review src/events.rs."}
            ]
        });
        let spawn = if protocol == "completion" {
            call("spawn", "task", batch)
        } else {
            response_call("spawn", "task", batch)
        };
        ask_reply_calls(request, protocol, json!([spawn]));
        // 三个子 Agent 都在等待各自的响应，摘要行必须显示排队/运行数量。
        pty.text("Subagents:");
        let mut children = Vec::new();
        let mut acked = false;
        while children.len() < 3 || !acked {
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            if task_test_tui_child(protocol, &body) {
                children.push(request);
            } else {
                assert!(!acked, "父会话不应在子 Agent 完成前重复请求");
                acked = true;
                ask_final_reply(request, protocol, "Waiting for subagent results.");
            }
        }
        let results = ["fast-result-11", "slow-result-22", "review-result-33"];
        for (request, result) in children.into_iter().zip(results) {
            ask_final_reply(request, protocol, result);
        }
        loop {
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            let text = body.to_string();
            let done = results.iter().all(|result| text.contains(result));
            ask_final_reply(
                request,
                protocol,
                if done {
                    "all-subagents-complete"
                } else {
                    "Waiting for remaining results."
                },
            );
            if done {
                break;
            }
        }
        pty.text("all-subagents-complete");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
    }
}

#[test]
fn task_test_tui_rejects_missing_roles_locally() {
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&fixture),
    )
    .unwrap();
    let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
    pty.ready();
    pty.input("/task-test\r");
    // 通知框按宽度换行，因此只断言首行片段与命令名。
    pty.text("Enable model roles FAST, SLOW, REVIEW");
    no_more(&fixture);
    // 本地错误不消耗草稿：输入仍为 /task-test，用 Ctrl + C 退出。
    pty.input("\x03");
    pty.finish();
}
