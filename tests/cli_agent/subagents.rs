use super::*;

#[test]
fn subagent_plan_role_is_hidden_and_rejected_in_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let models = if protocol == "completion" {
            fixture_models(&server)
        } else {
            response_models(&server, Some("response"))
        };
        let settings = json!({"model_roles":{
            "plan":{"model":"fixture/primary","effort":"high"},
            "fast":{"model":"fixture/primary","effort":"low"}
        }});
        let cli = Cli::start_with(
            home.path(),
            Some("check reserved plan role"),
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
            &json!(["default", "fast"])
        );
        assert!(!task["description"].as_str().unwrap().contains("plan="));
        sub_call(
            request,
            protocol,
            "reserved-plan",
            "task",
            json!({"context":"no dispatch on invalid batch","tasks":[
                {"role":"fast","task":"valid child"},
                {"role":"plan","task":"invalid child"}
            ]}),
        );
        let (request, body) = receive_sub(&server, protocol);
        assert!(!is_child(&body));
        assert!(
            body.to_string().contains("PLAN is reserved for plan mode"),
            "{body}"
        );
        sub_final(request, protocol, "reserved plan role rejected");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
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
            "tools:\n  artifactSpillThreshold: 16\n",
            &models,
        );
        let (request, _) = receive_sub(&server, protocol);
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"produce and inspect child logs","tasks":[{"task":"produce a log"}]}),
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
                assert!(!sub_has_tool(&body, protocol, "task"), "{body}");
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
                assert!(!sub_has_tool(&body, protocol, "task"), "{body}");
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
fn subagent_task_is_hidden_and_recursive_dispatch_is_rejected() {
    for protocol in ["completion", "response"] {
        for custom in [false, true] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            if custom {
                let agents = dir.path().join(".pp/agents");
                std::fs::create_dir_all(&agents).unwrap();
                std::fs::write(
                    agents.join("recursive.md"),
                    "---\nname: recursive\ndescription: recursion restriction fixture\ntools: [read, bash, task, wait]\nspawns: [\"*\"]\n---\nComplete the assigned work.\n",
                )
                .unwrap();
            }
            let models = if protocol == "completion" {
                fixture_models(&server)
            } else {
                response_models(&server, Some("response"))
            };
            let cli = Cli::start_with(
                dir.path(),
                Some("verify leaf-only dispatch"),
                &format!(
                    "{}task: {{max_concurrency: 1, max_recursion_depth: 8}}\n",
                    fixture_settings()
                ),
                &models,
            );
            let (request, body) = receive_sub(&server, protocol);
            assert!(sub_has_tool(&body, protocol, "task"));
            let mut first = json!({"task":"first-leaf"});
            if custom {
                first["agent"] = json!("recursive");
            }
            sub_call(
                request,
                protocol,
                "first-spawn",
                "task",
                json!({"context":"return the assigned marker","tasks":[first]}),
            );
            for (leaf, wait_id) in [
                ("first-leaf", "leaf-wait"),
                ("second-leaf", "second-leaf-wait"),
            ] {
                let mut child = None;
                let mut parent = None;
                while child.is_none() || parent.is_none() {
                    let (request, body) = receive_sub(&server, protocol);
                    if is_child(&body) {
                        assert!(child.is_none());
                        assert!(!sub_has_tool(&body, protocol, "task"), "{body}");
                        child = Some(request);
                    } else {
                        assert!(parent.is_none());
                        parent = Some(request);
                    }
                }
                sub_call(parent.unwrap(), protocol, wait_id, "wait", json!({}));
                let request = child.unwrap();
                if leaf == "first-leaf" {
                    sub_call(
                        request,
                        protocol,
                        "forbidden-task",
                        "task",
                        json!({"context":"illegal recursion","tasks":[{"task":"forbidden-grandchild"}]}),
                    );
                    let (request, body) = receive_sub(&server, protocol);
                    assert!(is_child(&body), "{body}");
                    assert_eq!(
                        sub_tool_result(&body, protocol, "forbidden-task")["ok"],
                        false
                    );
                    sub_final(request, protocol, "first-leaf-result");
                } else {
                    sub_final(request, protocol, "second-leaf-result");
                }
                let (request, body) = receive_sub(&server, protocol);
                assert!(!is_child(&body));
                assert!(sub_has_tool(&body, protocol, "task"));
                let waited = sub_tool_result(&body, protocol, wait_id);
                assert_eq!(waited["ok"], true, "{waited}");
                assert_eq!(waited["pending"], false, "{waited}");
                let results = waited["results"].as_array().unwrap();
                assert_eq!(results.len(), 1, "{waited}");
                assert_eq!(
                    results[0]["run"]["result"]["output"],
                    format!("{leaf}-result")
                );
                if leaf == "first-leaf" {
                    sub_call(
                        request,
                        protocol,
                        "second-spawn",
                        "task",
                        json!({"context":"return the assigned marker","tasks":[{"task":"second-leaf"}]}),
                    );
                } else {
                    sub_final(request, protocol, "leaf-restriction-verified");
                }
            }
            let output = cli.finish();
            assert!(output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("leaf-restriction-verified"),
                "{output:?}"
            );
            let registry = std::fs::read_dir(config_path(dir.path(), "sessions"))
                .unwrap()
                .flatten()
                .find(|entry| entry.path().extension().is_some_and(|s| s == "subagents"))
                .unwrap()
                .path()
                .join("registry.json");
            let registry: Value =
                serde_json::from_slice(&std::fs::read(registry).unwrap()).unwrap();
            let agents = registry["agents"].as_object().unwrap();
            assert_eq!(agents.len(), 2, "{registry}");
            for agent in agents.values() {
                assert_eq!(agent["parent"], "Main", "{agent}");
            }
            let runs = registry["runs"].as_object().unwrap();
            assert_eq!(runs.len(), 2, "{registry}");
            for run in runs.values() {
                assert_eq!(run["status"], "completed", "{run}");
                assert_eq!(run["result"]["ok"], true, "{run}");
            }
            no_more(&server);
        }
    }
}

#[test]
fn subagent_concurrency_limits_are_shared_across_batches() {
    for protocol in ["completion", "response"] {
        for (task_settings, limit) in [
            ("", 4),
            ("task: {max_concurrency: 1}\n", 1),
            ("task: {max_concurrency: 2}\n", 2),
            ("task: {max_concurrency: 32}\n", 32),
        ] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let models = if protocol == "completion" {
                fixture_models(&server)
            } else {
                response_models(&server, Some("response"))
            };
            let cli = Cli::start_with(
                dir.path(),
                Some("verify shared execution concurrency"),
                &format!("{}{task_settings}", fixture_settings()),
                &models,
            );
            let markers = (0..limit)
                .map(|index| format!("concurrency-first-{index}"))
                .collect::<Vec<_>>();
            let tasks = markers
                .iter()
                .map(|marker| json!({"task":marker}))
                .collect::<Vec<_>>();
            let (request, body) = receive_sub(&server, protocol);
            assert!(!is_child(&body));
            sub_call(
                request,
                protocol,
                "concurrency-first",
                "task",
                json!({"context":"Return only your assigned task marker.","tasks":tasks}),
            );
            let mut children = std::collections::BTreeMap::new();
            let mut parent = None;
            while children.len() < limit || parent.is_none() {
                let (request, body) = receive_sub(&server, protocol);
                if is_child(&body) {
                    // 优先最长标记，避免 first-1 抢先匹配 first-10。
                    let text = body.to_string();
                    let marker = markers
                        .iter()
                        .filter(|marker| text.contains(marker.as_str()))
                        .max_by_key(|marker| marker.len())
                        .unwrap_or_else(|| panic!("unknown child marker: {body}"));
                    assert!(
                        children.insert(marker.clone(), request).is_none(),
                        "duplicate child request: {body}"
                    );
                } else {
                    assert!(parent.is_none(), "unexpected parent request: {body}");
                    parent = Some(request);
                }
            }
            // Main 不占 child 名额，满额时仍能派发下一批。
            sub_call(
                parent.unwrap(),
                protocol,
                "concurrency-extra",
                "task",
                json!({"context":"Return only your assigned task marker.",
                    "tasks":[{"task":"concurrency-extra"}]}),
            );
            let (request, body) = receive_sub(&server, protocol);
            assert!(!is_child(&body), "child exceeded limit {limit}: {body}");
            sub_call(request, protocol, "concurrency-wait", "wait", json!({}));
            no_more(&server);

            let (marker, request) = children.pop_first().unwrap();
            let registry = std::fs::read_dir(config_path(dir.path(), "sessions"))
                .unwrap()
                .flatten()
                .find(|e| e.path().extension().is_some_and(|x| x == "subagents"))
                .unwrap()
                .path()
                .join("registry.json");
            let assert_running_limit = || {
                let state: Value =
                    serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
                for kind in ["agents", "runs"] {
                    let entries = state[kind].as_object().unwrap();
                    assert_eq!(
                        entries
                            .values()
                            .filter(|r| r["status"] == "running")
                            .count(),
                        limit,
                        "{state}"
                    );
                    assert_eq!(
                        entries.values().filter(|r| r["status"] == "queued").count(),
                        1,
                        "{state}"
                    );
                }
            };
            assert_running_limit();
            // 同一 Run 的工具和后续请求必须继续持有名额，不能让排队 Agent 插入。
            sub_call(
                request,
                protocol,
                "concurrency-step",
                "bash",
                json!({"command":"printf lifecycle-step"}),
            );
            let (request, body) = receive_sub(&server, protocol);
            assert!(is_child(&body), "{body}");
            assert!(body.to_string().contains(&marker), "{body}");
            assert!(!body.to_string().contains("concurrency-extra"), "{body}");
            assert_eq!(
                sub_tool_result(&body, protocol, "concurrency-step")["ok"],
                true,
                "{body}"
            );
            assert_running_limit();
            no_more(&server);
            sub_final(request, protocol, &marker);
            let (extra, body) = receive_sub(&server, protocol);
            assert!(is_child(&body), "{body}");
            assert!(body.to_string().contains("concurrency-extra"), "{body}");
            no_more(&server);
            for (marker, request) in children {
                sub_final(request, protocol, &marker);
            }
            sub_final(extra, protocol, "concurrency-extra");

            let (request, body) = receive_sub(&server, protocol);
            assert!(!is_child(&body), "{body}");
            let waited = sub_tool_result(&body, protocol, "concurrency-wait");
            assert_eq!(waited["ok"], true, "{waited}");
            assert_eq!(waited["pending"], false, "{waited}");
            let results = waited["results"].as_array().unwrap();
            assert_eq!(results.len(), limit + 1, "{waited}");
            let outputs = results
                .iter()
                .map(|result| {
                    let run = &result["run"];
                    assert_eq!(run["status"], "completed", "{run}");
                    assert_eq!(run["result"]["ok"], true, "{run}");
                    run["result"]["output"].as_str().unwrap().to_owned()
                })
                .collect::<std::collections::BTreeSet<_>>();
            let expected = markers
                .into_iter()
                .chain(std::iter::once("concurrency-extra".into()))
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(outputs, expected, "{protocol}, limit={limit}");
            sub_final(request, protocol, "concurrency-limit-verified");
            let output = cli.finish();
            assert!(output.status.success(), "{output:?}");
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("concurrency-limit-verified"),
                "{output:?}"
            );
            no_more(&server);
        }
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
            json!({"context":"readonly","tasks":[{"readonly":true,"task":"inspect only"}]}),
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

#[test]
fn custom_agent_roundtrip_rejects_invented_names_and_respects_model_role_precedence() {
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
        std::fs::write(definitions.join("analyst.md"), "---\nname: analyst\ndescription: configured analyst\nmodel: custom/special\nthinking: high\ntools: [read]\nspawns: []\n---\nconfigured-analysis-prompt-marker").unwrap();
        let provider = |protocol, model| {
            json!({"protocol":protocol,"api_key":"fixture-key",
            "base_url":format!("http://{}/v1", server.server_addr()),"models":{"special":{"model":model}}})
        };
        let models = json!({"providers":{"main":provider(protocol,"main-model"),"custom":provider(custom_protocol,"custom-model")}});
        let settings = json!({"default_model":"main/special","rate_limit_retry_budget_seconds":0,
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
            task.pointer("/parameters/properties/tasks/items/properties/agent/enum")
                .unwrap(),
            &json!(["analyst"])
        );
        sub_call(
            request,
            protocol,
            "invalid-custom",
            "task",
            json!({"context":"atomic validation","tasks":[
                {"agent":"analyst","task":"valid task"}, {"agent":"scout","task":"invented task"}
            ]}),
        );
        let (request, body) = receive_sub(&server, protocol);
        assert!(!is_child(&body));
        assert!(
            body.to_string()
                .contains("unknown or disabled custom agent")
        );
        sub_call(
            request,
            protocol,
            "configured-custom",
            "task",
            json!({"context":"configured tasks","tasks":[
                {"agent":"analyst","task":"custom model task"},
                {"agent":"analyst","role":"fast","task":"explicit fast task"}
            ]}),
        );
        let mut seen = std::collections::BTreeSet::new();
        loop {
            let (request, body) = receive_raw(&server);
            if is_child(&body) {
                let model = body["model"].as_str().unwrap();
                assert!(seen.insert(model.to_owned()));
                let (child_protocol, effort) = match model {
                    "custom-model" => (custom_protocol, "high"),
                    "main-model" => (protocol, "low"),
                    _ => panic!("unexpected child model: {model}"),
                };
                assert_eq!(
                    request.url(),
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
                assert!(
                    body.to_string()
                        .contains("configured-analysis-prompt-marker")
                );
                let tools = body["tools"].as_array().unwrap();
                assert_eq!(tools.len(), 1);
                assert_eq!(
                    if child_protocol == "completion" {
                        &tools[0]["function"]["name"]
                    } else {
                        &tools[0]["name"]
                    },
                    "read"
                );
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
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        no_more(&server);
    }
}
