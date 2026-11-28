use super::*;

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
include!("subagents/wait_cases.rs");

#[test]
fn custom_role_roundtrip_rejects_invented_names_and_routes_each_role() {
    let protocol = "response";
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
