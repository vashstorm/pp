use super::*;
use pp_config::config::ModelRole;
use pp_config::config::RoleAssignment;
use pp_core::plan::WorkMode;

#[test]
fn model_roles_plan_applies_once_and_manual_model_and_effort_survive_continuation() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        let path = dir.path().join("models.yml");
        let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        models["providers"]["fixture"]["models"]["planner"] = json!({"model":"planner"});
        std::fs::write(path, models.to_string()).unwrap();
        agent.reload_models().unwrap();
        agent
            .save_model_role(
                ModelRole::Plan,
                Some(RoleAssignment {
                    fallback: None,
                    enabled: true,
                    model: "fixture/planner".into(),
                    effort: ThinkLevel::High,
                }),
            )
            .unwrap();
        seed_session(&mut agent, "planning context marker");
        agent.enter_plan(&CancellationToken::new()).unwrap();
        assert_eq!(agent.current_model.reference, "fixture/planner");
        assert_eq!(agent.think_level, ThinkLevel::High);
        assert_eq!(agent.workflow.mode, WorkMode::Plan);
        assert!(
            serde_json::to_string(&agent.session)
                .unwrap()
                .contains("planning context marker")
        );
        agent.switch_model("fixture/one").unwrap();
        agent.set_think_level(ThinkLevel::Low).unwrap();
        agent.enter_plan(&CancellationToken::new()).unwrap();
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(agent.think_level, ThinkLevel::Low);
        assert_eq!(
            agent.model_roles()[&ModelRole::Plan].model,
            "fixture/planner"
        );
    }
}

#[test]
fn model_roles_unavailable_plan_fails_before_changing_workflow_or_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = runtime_input_agent(
        dir.path(),
        "completion",
        "127.0.0.1:1",
        "model_roles: {plan: {model: removed/model, effort: high}}\n",
    );
    let session = serde_json::to_value(&agent.session).unwrap();
    assert!(agent.enter_plan(&CancellationToken::new()).is_err());
    assert_eq!(agent.workflow.mode, WorkMode::Execute);
    assert_eq!(serde_json::to_value(&agent.session).unwrap(), session);
}

#[test]
fn model_roles_plan_children_send_each_roles_model_protocol_and_effort() {
    for protocol in ["completion", "response"] {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let address = server.server_addr().to_string();
        let dir = tempfile::tempdir().unwrap();
        let parent_protocol = if protocol == "completion" {
            "response"
        } else {
            "completion"
        };
        let mut agent = runtime_input_agent(dir.path(), parent_protocol, &address, "");
        let path = dir.path().join("models.yml");
        let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        models["providers"]["fixture"]["models"]["planner"] = json!({"model":"planner-only"});
        models["providers"]["children"] = json!({
            "api_key":"fixture-key", "protocol":protocol,
            "base_url":format!("http://{address}/v1"),
            "models":{
                "default":{"model":"child-default"}, "fast":{"model":"child-fast"},
                "slow":{"model":"child-slow"}, "vision":{"model":"child-vision"},
                "review":{"model":"child-review"}
            }
        });
        std::fs::write(path, models.to_string()).unwrap();
        agent.reload_models().unwrap();
        for (role, effort) in [
            (ModelRole::Default, ThinkLevel::Low),
            (ModelRole::Fast, ThinkLevel::Low),
            (ModelRole::Slow, ThinkLevel::High),
            (ModelRole::Vision, ThinkLevel::Medium),
            (ModelRole::Review, ThinkLevel::High),
            (ModelRole::Plan, ThinkLevel::High),
        ] {
            agent
                .save_model_role(
                    role,
                    Some(RoleAssignment {
                        enabled: true,
                        model: if role == ModelRole::Plan {
                            "fixture/planner".into()
                        } else {
                            format!("children/{}", role.name().to_lowercase())
                        },
                        effort,
                        fallback: None,
                    }),
                )
                .unwrap();
        }
        seed_session(&mut agent, "plan child routing");
        agent.enter_plan(&CancellationToken::new()).unwrap();
        agent.ensure_subagents().unwrap();
        let binding = agent.subagents().unwrap();
        binding
            .execute(
                "task",
                &json!({"context":"plan investigation", "tasks":[
                    {"role":"fast","task":"fast investigation"},
                    {"role":"slow","task":"slow investigation"},
                    {"role":"vision","task":"vision investigation"},
                    {"role":"review","task":"review investigation"}
                ]})
                .to_string(),
                true,
                &Default::default(),
            )
            .unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..4 {
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            assert_eq!(
                request.url(),
                if protocol == "completion" {
                    "/v1/chat/completions"
                } else {
                    "/v1/responses"
                }
            );
            let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
            let model = body["model"].as_str().unwrap();
            assert!(seen.insert(model.to_owned()));
            let effort = match model {
                "child-fast" => "low",
                "child-slow" | "child-review" => "high",
                "child-vision" => "medium",
                _ => panic!("unexpected model: {model}"),
            };
            assert_eq!(
                if protocol == "completion" {
                    &body["reasoning_effort"]
                } else {
                    &body["reasoning"]["effort"]
                },
                effort
            );
            for tool in body["tools"].as_array().unwrap() {
                let name = if protocol == "completion" {
                    &tool["function"]["name"]
                } else {
                    &tool["name"]
                };
                assert!(matches!(
                    name.as_str(),
                    Some("read" | "rg" | "glob" | "lsp")
                ));
            }
            final_for(protocol, request, "investigation complete", true);
        }
        let result = agent.tools.execute(
            "wait",
            "{}",
            "wait-roles",
            crate::tools::ToolPolicy::Execute {
                workflow: &agent.workflow,
            },
            &Default::default(),
            &mut |_| {},
        );
        let result: Value = serde_json::from_str(&result.output).unwrap();
        assert_eq!(result["results"].as_array().unwrap().len(), 4);
        assert_eq!(result["pending"], false);
        assert!(
            result["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|result| result["run"]["result"]["ok"] == true)
        );
        agent.stop_subagents().unwrap();
        assert_eq!(agent.current_model.reference, "fixture/planner");
        assert_eq!(agent.think_level, ThinkLevel::High);
    }
}

#[test]
fn model_roles_main_only_configuration_does_not_enable_task_in_plan() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
    let path = dir.path().join("models.yml");
    let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    models["providers"]["fixture"]["models"]["planner"] = json!({"model":"planner-only"});
    std::fs::write(path, models.to_string()).unwrap();
    agent.reload_models().unwrap();
    agent
        .save_model_role(
            ModelRole::Plan,
            Some(RoleAssignment {
                enabled: true,
                model: "fixture/planner".into(),
                effort: ThinkLevel::High,
                fallback: None,
            }),
        )
        .unwrap();
    seed_session(&mut agent, "default role routing");
    agent.enter_plan(&CancellationToken::new()).unwrap();
    agent.ensure_subagents().unwrap();
    let binding = agent.subagents().unwrap();
    assert!(binding.definitions(WorkMode::Plan).is_empty());
    agent.stop_subagents().unwrap();
}
