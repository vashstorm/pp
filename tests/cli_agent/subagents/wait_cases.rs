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
    let protocol = "response";
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

