use super::*;
fn receive_sub(server: &Server, protocol: &str) -> (Request, Value) {
    receive_at(
        server,
        if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        },
    )
}
fn sub_call(request: Request, protocol: &str, id: &str, name: &str, args: Value) {
    if protocol == "completion" {
        calls_reply(request, json!([call(id, name, args)]));
    } else {
        response_reply(request, json!([response_call(id, name, args)]));
    }
}
fn sub_final(request: Request, protocol: &str, text: &str) {
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
            fixture_settings(),
            &models,
        );
        let (request, body) = receive_sub(&server, protocol);
        assert!(body["tools"].to_string().contains("task"));
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"shared-context-marker","tasks":[{"task":"child-one"},{"task":"child-two"}]}),
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
            fixture_settings(),
            &models,
        );
        let (request, _) = receive_sub(&server, protocol);
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"structured","tasks":[{"task":"Return a count","output_schema":{"type":"object","properties":{"count":{"type":"integer"}},"required":["count"],"additionalProperties":false}}]}),
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

#[test]
fn subagent_followup_cold_revives_history_and_wait_consumes_only_once() {
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
            Some("parent followup task"),
            &format!("{}task: {{idle_ttl_ms: 20}}\n", fixture_settings()),
            &models,
        );
        let (request, _) = receive_sub(&server, protocol);
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"followup context","tasks":[{"task":"initial child question"}]}),
        );
        let mut answered = false;
        let mut waited = false;
        while !answered || !waited {
            let (request, body) = receive_sub(&server, protocol);
            if is_child(&body) {
                assert!(!answered);
                sub_final(request, protocol, "first-child-answer");
                answered = true;
            } else {
                sub_call(request, protocol, "wait", "wait", json!({"run_ids":["r1"]}));
                waited = true;
            }
        }
        let (request, body) = receive_sub(&server, protocol);
        assert!(!is_child(&body));
        assert_eq!(
            body.to_string().matches("first-child-answer").count(),
            1,
            "{body}"
        );
        let registry = std::fs::read_dir(config_path(dir.path(), "sessions"))
            .unwrap()
            .flatten()
            .find(|e| e.path().extension().is_some_and(|s| s == "subagents"))
            .unwrap()
            .path()
            .join("registry.json");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let value: Value = serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
            if value["agents"]["a1"]["status"] == "parked" {
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        sub_call(
            request,
            protocol,
            "follow",
            "write",
            json!({"path":"agent://a1","content":"followup-question"}),
        );
        let mut second = false;
        loop {
            let (request, body) = receive_sub(&server, protocol);
            if is_child(&body) {
                assert!(!second);
                let text = body.to_string();
                assert!(
                    text.contains("first-child-answer") && text.contains("followup-question"),
                    "{text}"
                );
                second = true;
                sub_final(request, protocol, "second-child-answer");
            } else {
                let done = body.to_string().contains("second-child-answer");
                sub_final(
                    request,
                    protocol,
                    if done {
                        "followup-complete"
                    } else {
                        "Waiting."
                    },
                );
                if done {
                    break;
                }
            }
        }
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        let value: Value = serde_json::from_slice(&std::fs::read(registry).unwrap()).unwrap();
        assert!(value["agents"]["a1"]["generation"].as_u64().unwrap() >= 3);
        assert_eq!(value["runs"].as_object().unwrap().len(), 2);
    }
}

#[test]
fn subagent_recursive_wait_releases_the_single_execution_slot() {
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
            Some("recursive task"),
            &format!("{}task: {{max_concurrency: 1}}\n", fixture_settings()),
            &models,
        );
        let (request, _) = receive_sub(&server, protocol);
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"root background","tasks":[{"task":"outer task"}]}),
        );
        let mut nested = false;
        loop {
            let (request, body) = receive_sub(&server, protocol);
            if is_child(&body) {
                let system = if protocol == "completion" {
                    body["messages"][0].to_string()
                } else {
                    body["instructions"].to_string()
                };
                if system.contains("subagent a2.") {
                    assert!(
                        !body["tools"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|t| t["name"] == "task" || t["function"]["name"] == "task")
                    );
                    sub_final(request, protocol, "nested-leaf");
                } else if !nested {
                    nested = true;
                    sub_call(
                        request,
                        protocol,
                        "nested",
                        "task",
                        json!({"context":"nested background","tasks":[{"task":"leaf task"}]}),
                    );
                } else if body.to_string().contains("nested-leaf") {
                    sub_final(request, protocol, "outer-child-result");
                } else {
                    sub_call(
                        request,
                        protocol,
                        "nested-wait",
                        "wait",
                        json!({"run_ids":["r2"]}),
                    );
                }
            } else {
                let done = body.to_string().contains("outer-child-result");
                sub_final(
                    request,
                    protocol,
                    if done {
                        "recursive-complete"
                    } else {
                        "Waiting."
                    },
                );
                if done {
                    break;
                }
            }
        }
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
    }
}

#[test]
fn subagent_readonly_capabilities_reject_unadvertised_writes() {
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
            Some("readonly investigation"),
            fixture_settings(),
            &models,
        );
        let (request, _) = receive_sub(&server, protocol);
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"readonly","tasks":[{"agent":"scout","task":"inspect only"}]}),
        );
        let mut attempted = false;
        loop {
            let (request, body) = receive_sub(&server, protocol);
            if is_child(&body) {
                if !attempted {
                    attempted = true;
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
                } else {
                    assert!(body.to_string().contains("capability set"));
                    sub_final(request, protocol, "write-was-blocked");
                }
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
    }
}

#[test]
fn subagent_inherits_headless_approval_and_does_not_start_denied_shell() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let settings = format!(
        "{}tools:\n  approvalMode: write\n  approval: {{task: allow}}\n",
        fixture_settings()
    );
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
        json!({"context":"approval inheritance","tasks":[{"task":"run a command"}]}),
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
    std::fs::write(roles.join("blocking.md"),"---\nname: blocking\ndescription: 同步角色\nblocking: true\ntools: [read, grep, glob]\n---\nComplete the assignment and return a result.").unwrap();
    let cli = Cli::start(&server, dir.path(), Some("mixed batch"));
    let (request, _) = receive(&server);
    sub_call(
        request,
        "completion",
        "spawn",
        "task",
        json!({"context":"mixed mode","tasks":[{"agent":"blocking","task":"blocking input"},{"task":"async input"}]}),
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
            "base_url":format!("http://{}/v1", server.server_addr()),"models":{"model":{"model":model}}})
        };
        let models = json!({"providers":{"first":provider(protocol,"main"),"second":provider(backup_protocol,"backup")}});
        let settings = json!({"rate_limit_retry_budget_seconds":0,"default_model":"first/model",
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
            let mut request = server
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
            if is_child(&body) {
                if body["model"] == "main" {
                    failed_child += 1;
                    request.respond(Response::empty(503)).unwrap();
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
