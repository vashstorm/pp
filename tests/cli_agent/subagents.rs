use super::*;
use crate::streaming::LiveCli;

#[test]
fn subagent_role_is_required_and_main_roles_are_rejected_in_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let models = if protocol == "completion" {
            fixture_models(&server)
        } else {
            response_models(&server, Some("response"))
        };
        let settings = json!({"tools":{"approvalMode":"yolo"},"model_roles":{
            "default":{"model":"fixture/primary","effort":"none"},
            "plan":{"model":"fixture/primary","effort":"high"},
            "fast":{"model":"fixture/primary","effort":"low"}
        }});
        let cli = Cli::start_with(
            home.path(),
            Some("check required task role"),
            &settings.to_string(),
            &models,
        );
        let (request, body) = receive_sub(&server, protocol);
        let task = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                if protocol == "completion" {
                    &tool["function"]
                } else {
                    tool
                }
            })
            .find(|tool| tool["name"] == "task")
            .unwrap();
        assert_eq!(
            task.pointer("/parameters/properties/tasks/items/properties/role/enum")
                .unwrap(),
            &json!(["fast"])
        );
        assert_eq!(
            task.pointer("/parameters/properties/tasks/items/required")
                .unwrap(),
            &json!(["task", "role"])
        );
        let mut request = request;
        for (id, invalid, error) in [
            ("missing", json!({"task":"missing role"}), "missing field"),
            (
                "default",
                json!({"role":"default","task":"invalid role"}),
                "reserved for the main agent",
            ),
            (
                "plan",
                json!({"role":"plan","task":"invalid role"}),
                "reserved for the main agent",
            ),
        ] {
            sub_call(
                request,
                protocol,
                id,
                "task",
                json!({"context":"no dispatch on invalid batch","tasks":[
                    {"role":"fast","task":"valid child"},
                    invalid
                ]}),
            );
            let (next, body) = receive_sub(&server, protocol);
            assert!(!is_child(&body));
            assert!(body.to_string().contains(error), "{body}");
            request = next;
        }
        sub_final(request, protocol, "invalid roles rejected");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        no_more(&server);
    }
}

#[test]
fn explorer_role_is_read_only_and_uses_the_exploration_prompt_in_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let models = if protocol == "completion" {
            fixture_models(&server)
        } else {
            response_models(&server, Some("response"))
        };
        let settings = "tools: {approvalMode: yolo}\nmodel_roles:\n  fast: {model: fixture/primary, effort: none}\n  explorer: {model: fixture/primary, effort: none}\n";
        let cli = Cli::start_with(
            home.path(),
            Some("map the execution loop"),
            settings,
            &models,
        );
        let (request, body) = receive_sub(&server, protocol);
        let task = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                if protocol == "completion" {
                    &tool["function"]
                } else {
                    tool
                }
            })
            .find(|tool| tool["name"] == "task")
            .unwrap();
        assert!(
            task.pointer("/parameters/properties/tasks/items/properties/role/enum")
                .unwrap()
                .as_array()
                .unwrap()
                .contains(&json!("explorer")),
            "{task}"
        );
        sub_call(
            request,
            protocol,
            "explore",
            "task",
            json!({"context":"read-only exploration","tasks":[{"role":"explorer","task":"map the execution loop"}]}),
        );
        let mut explored = false;
        loop {
            let (request, body) = receive_sub(&server, protocol);
            if is_child(&body) {
                assert!(!explored);
                explored = true;
                for denied in ["write", "edit", "bash", "bash_task", "task"] {
                    assert!(!sub_has_tool(&body, protocol, denied), "{denied}: {body}");
                }
                for allowed in ["read", "rg", "glob", "web_search"] {
                    assert!(sub_has_tool(&body, protocol, allowed), "{allowed}: {body}");
                }
                assert!(
                    body.to_string()
                        .contains("EXPLORER · Read-only code exploration")
                );
                sub_final(request, protocol, "execution loop mapped");
            } else if body.to_string().contains("execution loop mapped") {
                sub_final(request, protocol, "explorer result received");
                break;
            } else {
                sub_final(request, protocol, "Waiting for the explorer.");
            }
        }
        assert!(cli.finish().status.success());
        no_more(&server);
    }
}

#[test]
fn artifact_child_output_is_readable_from_parent_in_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = if protocol == "completion" {
            fixture_models(&server)
        } else {
            response_models(&server, Some("response"))
        };
        let cli = Cli::start_with(
            dir.path(),
            Some("inspect child artifact"),
            &format!(
                "{FIXTURE_FAST_ROLE}tools:\n  approvalMode: yolo\n  artifactSpillThreshold: 16\n"
            ),
            &models,
        );
        let (request, _) = receive_sub(&server, protocol);
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"produce and inspect child logs","tasks":[{"role":"fast","task":"produce a log"}]}),
        );
        let mut produced = false;
        let mut reading = false;
        loop {
            let (request, body) = receive_sub(&server, protocol);
            if is_child(&body) {
                if !produced {
                    produced = true;
                    sub_call(
                        request,
                        protocol,
                        "log",
                        "bash",
                        json!({"command":"printf 'child-artifact-marker\\nmore child output\\n'"}),
                    );
                } else {
                    assert!(body.to_string().contains("artifact://0"));
                    sub_final(request, protocol, "saved child log at artifact://0");
                }
            } else if reading {
                assert!(body.to_string().contains("child-artifact-marker"));
                sub_final(request, protocol, "child log verified");
                break;
            } else if body.to_string().contains("saved child log at artifact://0") {
                reading = true;
                sub_call(
                    request,
                    protocol,
                    "read-child",
                    "read",
                    json!({"path":"artifact://0:raw:1-1"}),
                );
            } else {
                sub_final(request, protocol, "Waiting for child output.");
            }
        }
        assert!(cli.finish().status.success());
    }
}
#[test]
fn subagent_result_read_pages_full_disk_output_in_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let models = if protocol == "completion" {
            fixture_models(&server)
        } else {
            response_models(&server, Some("response"))
        };
        let cli = Cli::start_with(
            home.path(),
            Some("read the full child result"),
            &format!("{FIXTURE_FAST_ROLE}tools: {{approvalMode: yolo}}\nread_max_lines: 1\n"),
            &models,
        );
        let first_line = "x".repeat(12_000);
        let output = format!("{first_line}\nchild-result-tail");
        let (request, _) = receive_sub(&server, protocol);
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"read disk result","tasks":[{"role":"fast","task":"produce a long result"}]}),
        );
        let mut child_finished = false;
        let mut resource = None;
        while !child_finished || resource.is_none() {
            let (request, body) = receive_sub(&server, protocol);
            if is_child(&body) {
                assert!(!child_finished);
                sub_final(request, protocol, &output);
                child_finished = true;
            } else {
                assert!(resource.is_none());
                let spawned = sub_tool_result(&body, protocol, "spawn");
                let run = spawned["tasks"][0]["run_id"].as_str().unwrap();
                let uri = spawned["tasks"][0]["output_uri"].as_str().unwrap();
                assert_eq!(spawned["wait_args"], json!({"run_ids":[run]}));
                resource = Some((
                    request,
                    uri.to_owned(),
                    run.to_owned(),
                    spawned["wait_args"].clone(),
                    spawned["tasks"][0]["agent_id"].clone(),
                ));
            }
        }
        let (request, uri, run, wait_args, agent_id) = resource.unwrap();
        sub_call(
            request,
            protocol,
            "wrong-wait",
            "wait",
            json!({"task_ids":[agent_id]}),
        );
        let (request, body) = receive_sub(&server, protocol);
        let wrong = sub_tool_result(&body, protocol, "wrong-wait");
        assert_eq!(wrong["ok"], false);
        let error = wrong["error"].as_str().unwrap();
        for hint in [
            "run_ids",
            "task_ids",
            "agent_id",
            "wait_args",
            "Unknown background task",
        ] {
            assert!(error.contains(hint), "{wrong}");
        }
        sub_call(request, protocol, "wait", "wait", wait_args);
        let (request, body) = receive_sub(&server, protocol);
        assert_eq!(sub_tool_result(&body, protocol, "wait")["ok"], true);
        sub_call(request, protocol, "read-first", "read", json!({"path":uri}));
        let (request, body) = receive_sub(&server, protocol);
        let first = sub_tool_result(&body, protocol, "read-first");
        assert_eq!(first["source"], uri);
        assert_eq!(first["content"], format!("{first_line}\n"));
        assert_eq!(first["published"]["result"]["truncated"], true);
        let next = first["next_path"].as_str().unwrap();
        assert_eq!(next, format!("{uri}/{run}:2-"));
        sub_call(request, protocol, "read-next", "read", json!({"path":next}));
        let (request, body) = receive_sub(&server, protocol);
        let second = sub_tool_result(&body, protocol, "read-next");
        assert_eq!(second["content"], "child-result-tail");
        assert_eq!(second["truncated"], false);
        sub_final(request, protocol, "full child result verified");
        assert!(cli.finish().status.success());
        let storage = std::fs::read_dir(config_path(home.path(), "sessions"))
            .unwrap()
            .flatten()
            .find(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext == "subagents")
            })
            .unwrap()
            .path();
        let saved: Value = serde_json::from_slice(
            &std::fs::read(storage.join(format!("{run}.result.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["output"], output);
        no_more(&server);
    }
}

fn receive_sub(server: &TestServer, protocol: &str) -> (CapturedRequest, Value) {
    receive_at(
        server,
        if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        },
    )
}
fn sub_call(request: CapturedRequest, protocol: &str, id: &str, name: &str, args: Value) {
    if protocol == "completion" {
        calls_reply(request, json!([call(id, name, args)]));
    } else {
        response_reply(request, json!([response_call(id, name, args)]));
    }
}
fn sub_final(request: CapturedRequest, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}
fn is_child(body: &Value) -> bool {
    let system = if body["instructions"].is_string() {
        body["instructions"].to_string()
    } else {
        body["messages"][0].to_string()
    };
    system.contains("You are subagent")
}

fn sub_has_tool(body: &Value, protocol: &str, name: &str) -> bool {
    body["tools"].as_array().unwrap().iter().any(|tool| {
        if protocol == "completion" {
            tool["function"]["name"] == name
        } else {
            tool["name"] == name
        }
    })
}

fn sub_tool_result(body: &Value, protocol: &str, id: &str) -> Value {
    if protocol == "completion" {
        let message = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
            .unwrap();
        result(message, id)
    } else {
        let item = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == id)
            .unwrap();
        response_result(item, id)
    }
}
#[test]
fn subagent_batch_runs_in_parallel_without_parent_history_and_returns_both_results() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = if protocol == "completion" {
            fixture_models(&server)
        } else {
            response_models(&server, Some("response"))
        };
        let cli = Cli::start_with(
            dir.path(),
            Some("parent-only-private task"),
            &task_fixture_settings(),
            &models,
        );
        let (request, body) = receive_sub(&server, protocol);
        assert!(body["tools"].to_string().contains("task"));
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"shared-context-marker","tasks":[{"role":"fast","task":"child-one"},{"role":"fast","task":"child-two"}]}),
        );
        let mut children = Vec::new();
        let mut acked = false;
        while children.len() < 2 || !acked {
            let (request, body) = receive_sub(&server, protocol);
            if is_child(&body) {
                assert!(!body.to_string().contains("parent-only-private"));
                assert!(body.to_string().contains("shared-context-marker"));
                children.push(request);
            } else {
                assert!(!acked, "父会话不应在 child 完成前重复请求");
                acked = true;
                sub_final(request, protocol, "Waiting for child results.");
            }
        }
        for (index, request) in children.into_iter().enumerate() {
            sub_final(
                request,
                protocol,
                if index == 0 {
                    "child-result-one"
                } else {
                    "child-result-two"
                },
            );
        }
        loop {
            let (request, body) = receive_sub(&server, protocol);
            assert!(!is_child(&body));
            let text = body.to_string();
            let done = text.contains("child-result-one") && text.contains("child-result-two");
            sub_final(
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
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("all-subagents-complete"));
        let sessions = config_path(dir.path(), "sessions");
        let root = std::fs::read_dir(sessions)
            .unwrap()
            .flatten()
            .find(|e| e.path().extension().is_some_and(|x| x == "subagents"))
            .unwrap();
        let registry: Value =
            serde_json::from_slice(&std::fs::read(root.path().join("registry.json")).unwrap())
                .unwrap();
        assert_eq!(registry["runs"].as_object().unwrap().len(), 2);
        assert!(
            registry["runs"]
                .as_object()
                .unwrap()
                .values()
                .all(|r| r["consumed"] == true && r["status"] == "completed")
        );
    }
}
#[test]
fn subagent_structured_yield_is_validated_and_returns_data() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = if protocol == "completion" {
            fixture_models(&server)
        } else {
            response_models(&server, Some("response"))
        };
        let cli = Cli::start_with(
            dir.path(),
            Some("structured task"),
            &task_fixture_settings(),
            &models,
        );
        let (request, _) = receive_sub(&server, protocol);
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"structured","tasks":[{"role":"fast","task":"Return a count","output_schema":{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false}}]}),
        );
        let mut child = false;
        while !child {
            let (request, body) = receive_sub(&server, protocol);
            if is_child(&body) {
                assert!(body["tools"].to_string().contains("yield"));
                sub_call(
                    request,
                    protocol,
                    "yield",
                    "yield",
                    json!({"data":{"count":7}}),
                );
                child = true;
            } else {
                sub_final(request, protocol, "Waiting.");
            }
        }
        loop {
            let (request, body) = receive_sub(&server, protocol);
            assert!(!is_child(&body));
            let done = body.to_string().contains("count")
                && body.to_string().contains("pp-subagent-result:");
            sub_final(request, protocol, "structured-finished");
            if done {
                break;
            }
        }
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
    }
}

include!("subagents/wait_cases.rs");

#[test]
fn subagent_readonly_capabilities_reject_unadvertised_writes() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let remote = super::mcp_remote::fixture::RemoteServer::new("json");
        let mut config = remote.config("http", 3000);
        config["mcpServers"]["remote"]["read_only_tools"] = json!(["echo"]);
        std::fs::write(config_path(dir.path(), "mcp.json"), config.to_string()).unwrap();
        let settings = task_fixture_settings_with_tools(
            "  approvalMode: always-ask\n  approval: {task: allow}\n",
        );
        let models = if protocol == "completion" {
            fixture_models(&server)
        } else {
            response_models(&server, Some("response"))
        };
        let cli = Cli::start_with(
            dir.path(),
            Some("readonly investigation"),
            &settings,
            &models,
        );
        let (request, _) = receive_sub(&server, protocol);
        sub_call(
            request,
            protocol,
            "write-tier-mcp",
            "mcp__remote__fail",
            json!({}),
        );
        let (request, body) = receive_sub(&server, protocol);
        let denied = sub_tool_result(&body, protocol, "write-tier-mcp");
        assert_eq!(denied["ok"], false);
        assert!(
            denied["error"].as_str().unwrap().contains("approval"),
            "{denied}"
        );
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"readonly","tasks":[{"role":"fast","readonly":true,"task":"inspect only"}]}),
        );
        let mut stage = 0;
        loop {
            let (request, body) = receive_sub(&server, protocol);
            if is_child(&body) {
                if stage == 0 {
                    assert!(body["tools"].to_string().contains("mcp__remote__echo"));
                    assert!(!body["tools"].to_string().contains("mcp__remote__fail"));
                    assert!(
                        !body["tools"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|t| t["name"] == "write" || t["function"]["name"] == "write")
                    );
                    sub_call(
                        request,
                        protocol,
                        "bad-write",
                        "write",
                        json!({"path":"must-not-exist","content":"bad"}),
                    );
                } else if stage == 1 {
                    assert!(body.to_string().contains("capability set"));
                    sub_call(
                        request,
                        protocol,
                        "bad-remote",
                        "mcp__remote__fail",
                        json!({}),
                    );
                } else if stage == 2 {
                    assert!(body.to_string().matches("capability set").count() >= 2);
                    sub_call(
                        request,
                        protocol,
                        "good-remote",
                        "mcp__remote__echo",
                        json!({"text":"child public evidence"}),
                    );
                } else {
                    let output = sub_tool_result(&body, protocol, "good-remote");
                    assert_eq!(
                        output["result"]["content"][0]["text"],
                        "child public evidence"
                    );
                    sub_final(request, protocol, "write-was-blocked");
                }
                stage += 1;
            } else {
                let done = body.to_string().contains("write-was-blocked");
                sub_final(request, protocol, "readonly-finished");
                if done {
                    break;
                }
            }
        }
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert!(!dir.path().join("must-not-exist").exists());
        let log = remote.log();
        assert_eq!(
            log.iter()
                .filter(|entry| entry["body"]["method"] == "initialize")
                .count(),
            1
        );
        let calls: Vec<_> = log
            .iter()
            .filter(|entry| entry["body"]["method"] == "tools/call")
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["body"]["params"]["name"], "echo");
    }
}

#[test]
fn subagent_inherits_headless_approval_and_does_not_start_denied_shell() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let settings =
        task_fixture_settings_with_tools("  approvalMode: write\n  approval: {task: allow}\n");
    let cli = Cli::start_with(
        dir.path(),
        Some("approval task"),
        &settings,
        &fixture_models(&server),
    );
    let (request, _) = receive(&server);
    sub_call(
        request,
        "completion",
        "spawn",
        "task",
        json!({"context":"approval inheritance","tasks":[{"role":"fast","task":"run a command"}]}),
    );
    let mut attempted = false;
    loop {
        let (request, body) = receive(&server);
        if is_child(&body) {
            if !attempted {
                attempted = true;
                sub_call(
                    request,
                    "completion",
                    "shell",
                    "bash",
                    json!({"command":"touch denied-side-effect"}),
                );
            } else {
                assert!(body.to_string().contains("no interactive UI"));
                sub_final(request, "completion", "approval-refused");
            }
        } else {
            let done = body.to_string().contains("approval-refused");
            sub_final(request, "completion", "approval-finished");
            if done {
                break;
            }
        }
    }
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert!(!dir.path().join("denied-side-effect").exists());
}

#[test]
fn subagent_mixed_batch_starts_async_siblings_before_waiting_for_blocking_role() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let roles = dir.path().join(".pp/agents");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("blocking.md"),"---\nname: blocking\ndescription: 同步角色\nblocking: true\ntools: [read, rg, glob]\n---\nComplete the assignment and return a result.").unwrap();
    let cli = Cli::start_with(
        dir.path(),
        Some("mixed batch"),
        &task_fixture_settings(),
        &fixture_models(&server),
    );
    let (request, _) = receive(&server);
    sub_call(
        request,
        "completion",
        "spawn",
        "task",
        json!({"context":"mixed mode","tasks":[{"role":"blocking","task":"blocking input"},{"role":"fast","task":"async input"}]}),
    );
    let mut blocking = None;
    let mut asynchronous = None;
    for _ in 0..2 {
        let (request, body) = receive(&server);
        assert!(is_child(&body));
        if body.to_string().contains("blocking input") {
            blocking = Some(request);
        } else {
            asynchronous = Some(request);
        }
    }
    final_reply(asynchronous.unwrap(), "async-result");
    assert!(
        server
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none()
    );
    final_reply(blocking.unwrap(), "blocking-result");
    let (request, body) = receive(&server);
    assert!(!is_child(&body));
    let text = body.to_string();
    assert_eq!(text.matches("blocking-result").count(), 1);
    assert_eq!(text.matches("async-result").count(), 1);
    final_reply(request, "mixed-complete");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn fallback_subagent_uses_its_role_backup_without_switching_parent() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let backup_protocol = if protocol == "completion" {
            "response"
        } else {
            "completion"
        };
        let provider = |protocol, model| {
            json!({"api_key":"fixture-key","protocol":protocol,
            "base_url":format!("http://{}/v1", server.addr()),"models":{"model":{"model":model}}})
        };
        let models = json!({"providers":{"first":provider(protocol,"main"),"second":provider(backup_protocol,"backup")}});
        let settings = json!({"tools":{"approvalMode":"yolo"},"rate_limit_retry_budget_seconds":0,"default_model":"first/model",
            "model_roles":{"fast":{"model":"first/model","effort":"none","fallback":{"model":"second/model","effort":"high"}}}});
        let cli = Cli::start_with(
            home.path(),
            Some("delegate with fallback"),
            &settings.to_string(),
            &models.to_string(),
        );
        let (request, _) = receive_sub(&server, protocol);
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"fallback context", "tasks":[{"task":"child work","role":"fast"}]}),
        );
        let mut failed_child = 0;
        let mut backup_child = 0;
        loop {
            let request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let body = request.json().unwrap();
            if is_child(&body) {
                if body["model"] == "main" {
                    failed_child += 1;
                    request
                        .respond(Response::builder().status(503).body(Body::empty()).unwrap())
                        .unwrap();
                } else {
                    assert_eq!(body["model"], "backup");
                    backup_child += 1;
                    sub_final(request, backup_protocol, "child-fallback-complete");
                }
            } else {
                assert_eq!(body["model"], "main");
                let delivered = body.to_string().contains("child-fallback-complete");
                sub_final(
                    request,
                    protocol,
                    if delivered {
                        "parent-complete"
                    } else {
                        "Waiting for child."
                    },
                );
                if delivered {
                    break;
                }
            }
        }
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert_eq!((failed_child, backup_child), (1, 1));
        assert!(String::from_utf8_lossy(&output.stdout).contains("parent-complete"));
        no_more(&server);
    }
}

#[test]
fn custom_role_roundtrip_rejects_invented_names_and_routes_each_role() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let custom_protocol = if protocol == "completion" {
            "response"
        } else {
            "completion"
        };
        let definitions = dir.path().join(".pp/agents");
        std::fs::create_dir_all(&definitions).unwrap();
        std::fs::write(definitions.join("analyst.md"), "---\nname: analyst\ndescription: configured analyst\nmodel: custom/special\nthinking: high\ntools: [read, write]\nreadonly: true\nspawns: []\n---\nconfigured-analysis-prompt-marker").unwrap();
        let provider = |protocol, model| {
            json!({"protocol":protocol,"api_key":"fixture-key",
            "base_url":format!("http://{}/v1", server.addr()),"models":{"special":{"model":model}}})
        };
        let models = json!({"providers":{"main":provider(protocol,"main-model"),"custom":provider(custom_protocol,"custom-model")}});
        let settings = json!({"tools":{"approvalMode":"yolo"},"default_model":"main/special","rate_limit_retry_budget_seconds":0,
            "model_roles":{"fast":{"model":"main/special","effort":"low"}}});
        let cli = Cli::start_with(
            dir.path(),
            Some("check configured custom agents"),
            &settings.to_string(),
            &models.to_string(),
        );
        let (request, body) = receive_sub(&server, protocol);
        let task = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                if protocol == "completion" {
                    &tool["function"]
                } else {
                    tool
                }
            })
            .find(|tool| tool["name"] == "task")
            .unwrap();
        assert_eq!(
            task.pointer("/parameters/properties/tasks/items/properties/role/enum")
                .unwrap(),
            &json!(["fast", "analyst"])
        );
        sub_call(
            request,
            protocol,
            "invalid-custom",
            "task",
            json!({"context":"atomic validation","tasks":[
                {"role":"analyst","task":"valid task"}, {"role":"scout","task":"invented task"}
            ]}),
        );
        let (request, body) = receive_sub(&server, protocol);
        assert!(!is_child(&body));
        assert!(body.to_string().contains("unknown or disabled task role"));
        sub_call(
            request,
            protocol,
            "configured-custom",
            "task",
            json!({"context":"configured tasks","tasks":[
                {"role":"analyst","task":"custom model task"},
                {"role":"fast","task":"explicit fast task"}
            ]}),
        );
        let mut seen = std::collections::BTreeSet::new();
        let mut checked_readonly = false;
        loop {
            let (request, body) = receive_raw(&server);
            if is_child(&body) {
                let model = body["model"].as_str().unwrap();
                if model == "custom-model" && seen.contains(model) {
                    let result = sub_tool_result(&body, custom_protocol, "denied-custom-write");
                    assert_eq!(result["ok"], false);
                    assert!(
                        result
                            .to_string()
                            .contains("outside this agent capability set")
                    );
                    assert!(!dir.path().join("forbidden-custom-write.txt").exists());
                    checked_readonly = true;
                    sub_final(request, custom_protocol, "configured-result-custom-model");
                    continue;
                }
                assert!(seen.insert(model.to_owned()));
                let (child_protocol, effort) = match model {
                    "custom-model" => (custom_protocol, "high"),
                    "main-model" => (protocol, "low"),
                    _ => panic!("unexpected child model: {model}"),
                };
                assert_eq!(
                    request.target(),
                    if child_protocol == "completion" {
                        "/v1/chat/completions"
                    } else {
                        "/v1/responses"
                    }
                );
                assert_eq!(
                    if child_protocol == "completion" {
                        &body["reasoning_effort"]
                    } else {
                        &body["reasoning"]["effort"]
                    },
                    effort
                );
                let tools = body["tools"].as_array().unwrap();
                assert_eq!(
                    body.to_string()
                        .contains("configured-analysis-prompt-marker"),
                    model == "custom-model"
                );
                if model == "custom-model" {
                    assert_eq!(tools.len(), 1);
                    assert!(sub_has_tool(&body, child_protocol, "read"));
                    assert!(!body.to_string().contains("readonly: true"));
                    sub_call(
                        request,
                        child_protocol,
                        "denied-custom-write",
                        "write",
                        json!({"path":"forbidden-custom-write.txt","content":"blocked"}),
                    );
                    continue;
                } else {
                    assert!(sub_has_tool(&body, child_protocol, "edit"));
                }
                sub_final(
                    request,
                    child_protocol,
                    &format!("configured-result-{model}"),
                );
            } else {
                let done = body.to_string().contains("configured-result-main-model")
                    && body.to_string().contains("configured-result-custom-model");
                sub_final(
                    request,
                    protocol,
                    if done {
                        "configured-agents-complete"
                    } else {
                        "Waiting for configured agents."
                    },
                );
                if done {
                    break;
                }
            }
        }
        assert_eq!(seen.len(), 2);
        assert!(checked_readonly);
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        no_more(&server);
    }
}
