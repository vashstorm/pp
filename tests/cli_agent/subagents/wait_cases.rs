fn injected_subagent_runs(body: &Value, protocol: &str) -> Vec<Value> {
    body[if protocol == "completion" { "messages" } else { "input" }]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["role"] == "developer")
        .filter_map(|item| {
            let mut lines = item["content"].as_str()?.splitn(3, '\n');
            lines.next()?.strip_prefix("[pp-subagent-result:")?;
            lines.next()?;
            Some(serde_json::from_str(lines.next()?).unwrap())
        })
        .collect()
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
            &format!("{}task: {{idle_ttl_ms: 20}}\n", task_fixture_settings()),
            &models,
        );
        let (request, _) = receive_sub(&server, protocol);
        sub_call(
            request,
            protocol,
            "spawn",
            "task",
            json!({"context":"followup context","tasks":[{"role":"fast","task":"initial child question"}]}),
        );
        let mut child = None;
        let mut parent = None;
        while child.is_none() || parent.is_none() {
            let (request, body) = receive_sub(&server, protocol);
            if is_child(&body) {
                assert!(!sub_has_tool(&body, protocol, "task"), "{body}");
                assert!(child.replace(request).is_none());
            } else {
                assert!(parent.replace(request).is_none());
            }
        }
        sub_call(parent.unwrap(), protocol, "wait", "wait", json!({"run_ids":["r1"]}));
        sub_final(child.unwrap(), protocol, "first-child-answer");
        let (request, body) = receive_sub(&server, protocol);
        assert!(!is_child(&body));
        let waited = sub_tool_result(&body, protocol, "wait");
        assert_eq!(waited["jobs"][0]["id"], "r1");
        assert_eq!(waited["results"], json!([]));
        let injected = injected_subagent_runs(&body, protocol);
        assert_eq!(injected.len(), 1, "{body}");
        assert_eq!(injected[0]["result"]["output"], "first-child-answer");
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
                    task_fixture_settings()
                ),
                &models,
            );
            let (request, body) = receive_sub(&server, protocol);
            assert!(sub_has_tool(&body, protocol, "task"));
            let mut first = json!({"role":"fast","task":"first-leaf"});
            if custom {
                first["role"] = json!("recursive");
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
                        json!({"context":"illegal recursion","tasks":[{"role":"fast","task":"forbidden-grandchild"}]}),
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
                assert_eq!(waited["results"], json!([]));
                assert_eq!(waited["jobs"].as_array().unwrap().len(), 1, "{waited}");
                let results = injected_subagent_runs(&body, protocol);
                let id = &waited["jobs"][0]["id"];
                let run = results.iter().find(|run| &run["id"] == id).unwrap();
                assert_eq!(
                    run["result"]["output"],
                    format!("{leaf}-result")
                );
                if leaf == "first-leaf" {
                    sub_call(
                        request,
                        protocol,
                        "second-spawn",
                        "task",
                        json!({"context":"return the assigned marker","tasks":[{"role":"fast","task":"second-leaf"}]}),
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
fn slow_subagent_concurrency_limits_are_shared_across_batches() {
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
            // 32 个并发子 Agent 的诊断输出会填满未消费的管道；LiveCli 持续排空两路输出。
            let cli = LiveCli::new(Cli::start_with(
                dir.path(),
                Some("verify shared execution concurrency"),
                &format!("{}{task_settings}", task_fixture_settings()),
                &models,
            ));
            let markers = (0..limit)
                .map(|index| format!("concurrency-first-{index}"))
                .collect::<Vec<_>>();
            let tasks = markers
                .iter()
                .map(|marker| json!({"role":"fast","task":marker}))
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
                    "tasks":[{"role":"fast","task":"concurrency-extra"}]}),
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
            assert_eq!(waited["results"], json!([]));
            assert_eq!(waited["jobs"].as_array().unwrap().len(), limit + 1, "{waited}");
            let results = injected_subagent_runs(&body, protocol);
            assert_eq!(results.len(), limit + 1, "{body}");
            let outputs = results
                .iter()
                .map(|run| {
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
