use super::*;

const ROLES: &str = "model_roles:\n  fast: {model: fixture/primary, effort: none}\n  slow: {model: fixture/primary, effort: none}\n  review: {model: fixture/primary, effort: none}\n";
const BATCH: [&str; 3] = [
    "fast-rust-count",
    "slow-symbol-lookup",
    "review-events-risk",
];

fn models_for(server: &Server, protocol: &str) -> String {
    if protocol == "completion" {
        fixture_models(server)
    } else {
        response_models(server, Some(protocol))
    }
}

fn task_test_receive(server: &Server, protocol: &str) -> (Request, Value) {
    receive_at(server, format!("/v1/{}", api_suffix(protocol)).as_str())
}

fn api_suffix(protocol: &str) -> &'static str {
    if protocol == "completion" {
        "chat/completions"
    } else {
        "responses"
    }
}

fn task_test_call(request: Request, protocol: &str, args: Value) {
    if protocol == "completion" {
        calls_reply(request, json!([call("spawn", "task", args)]));
    } else {
        response_reply(request, json!([response_call("spawn", "task", args)]));
    }
}

fn task_test_final(request: Request, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

fn task_test_child(body: &Value) -> bool {
    let system = if body["instructions"].is_string() {
        body["instructions"].to_string()
    } else {
        body["messages"][0].to_string()
    };
    system.contains("You are subagent")
}

fn task_test_batch() -> Value {
    json!({
        "context": "/task-test debug scenario: read-only inspection of the current repository.",
        "tasks": [
            {"name": "fast-rust-count", "agent": "scout", "role": "fast", "task": "Count regular Rust source files."},
            {"name": "slow-symbol-lookup", "agent": "scout", "role": "slow", "task": "Find where AgentEvent is defined."},
            {"name": "review-events-risk", "agent": "reviewer", "task": "Review src/events.rs."}
        ]
    })
}

fn task_test_user_text(protocol: &str, body: &Value) -> String {
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

fn task_test_registry(home: &std::path::Path) -> Value {
    let root = std::fs::read_dir(config_path(home, "sessions"))
        .unwrap()
        .flatten()
        .find(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|ext| ext == "subagents")
        })
        .expect("missing subagent registry directory");
    serde_json::from_slice(&std::fs::read(root.path().join("registry.json")).unwrap()).unwrap()
}

#[test]
fn task_test_reports_missing_roles_without_requests() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = Cli::start_with(
            dir.path(),
            None,
            fixture_settings(),
            &models_for(&server, protocol),
        );
        cli.input("/task-test\n/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Enable model roles FAST, SLOW, REVIEW in /Roles before /task-test"),
            "{stderr}"
        );
        no_more(&server);
    }
}

#[test]
fn task_test_dispatches_fixed_batch_and_returns_three_results() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = Cli::start_with(
            dir.path(),
            None,
            &format!("{}{ROLES}", fixture_settings()),
            &models_for(&server, protocol),
        );
        cli.input("/task-test\n");
        let (request, body) = task_test_receive(&server, protocol);
        assert!(!task_test_child(&body));
        let text = task_test_user_text(protocol, &body);
        for marker in [
            "fast-rust-count",
            "slow-symbol-lookup",
            "review-events-risk",
            "\"role\":\"fast\"",
            "\"role\":\"slow\"",
        ] {
            assert!(text.contains(marker), "missing {marker} in {text}");
        }
        task_test_call(request, protocol, task_test_batch());
        let mut children = Vec::new();
        let mut acked = false;
        while children.len() < BATCH.len() || !acked {
            let (request, body) = task_test_receive(&server, protocol);
            if task_test_child(&body) {
                children.push(request);
            } else {
                assert!(!acked, "父会话不应在 child 完成前重复请求");
                acked = true;
                task_test_final(request, protocol, "Waiting for subagent results.");
            }
        }
        let results = ["fast-result-11", "slow-result-22", "review-result-33"];
        for (request, result) in children.into_iter().zip(results) {
            task_test_final(request, protocol, result);
        }
        let summary = BATCH
            .iter()
            .zip(results)
            .map(|(name, result)| format!("{name}: completed ({result})"))
            .collect::<Vec<_>>()
            .join("\n");
        loop {
            let (request, body) = task_test_receive(&server, protocol);
            assert!(!task_test_child(&body));
            let text = body.to_string();
            let done = results.iter().all(|result| text.contains(result));
            task_test_final(
                request,
                protocol,
                if done {
                    summary.as_str()
                } else {
                    "Waiting for remaining results."
                },
            );
            if done {
                break;
            }
        }
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("review-events-risk: completed"), "{stdout}");
        let registry = task_test_registry(dir.path());
        assert_eq!(registry["runs"].as_object().unwrap().len(), BATCH.len());
        assert!(
            registry["runs"]
                .as_object()
                .unwrap()
                .values()
                .all(|run| run["consumed"] == true && run["status"] == "completed")
        );
    }
}
