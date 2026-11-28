mod integration {
    use super::super::*;
    use crate::model_control::{Binding, Change, Service};
    use pp_config::config::ModelRole;
    use pp_config::config::RoleAssignment;
    use pp_core::plan::WorkMode;
    use pp_test_support::TestServer;

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
                        model: "fixture/one".into(),
                        effort: ThinkLevel::Medium,
                    }),
                )
                .unwrap();
            let control = Service::start(agent.models()).unwrap();
            agent.runtime_models = Some(Binding::main(control.registry.clone()));
            assert!(control.submit(Change::Role(
                ModelRole::Plan,
                Some(RoleAssignment {
                    fallback: None,
                    enabled: true,
                    model: "fixture/planner".into(),
                    effort: ThinkLevel::High,
                }),
            )));
            // The control thread has committed the config; the worker has not processed the refresh command yet.
            control.flush();
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
    fn model_roles_unavailable_main_only_and_apply_pending_rollback() {
        {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_settings_agent(
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

        {
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
            assert!(agent.subagents().is_none());
            assert!(agent.tools.subagent.is_none());
        }

        {
            use std::collections::VecDeque;

            let dir = tempfile::tempdir().unwrap();
            let settings = dir.path().join("settings.yml");
            let models = dir.path().join("models.yml");
            std::fs::write(&settings, "{}").unwrap();
            std::fs::write(
            &models,
            json!({"providers":{"fixture":{
                "api_key":"fixture-key","protocol":"completion","base_url":"http://127.0.0.1:1/v1",
                "models":{"one":{"model":"one","thinking":{"efforts":["low","high"],"defaultLevel":"low"}}}
            }}})
            .to_string(),
        )
        .unwrap();
            std::fs::create_dir_all(dir.path().join(".pp")).unwrap();
            std::fs::write(
            dir.path().join(".pp/hooks.yml"),
            "UserPromptSubmit:\n  - hooks:\n      - type: command\n        command: \"printf '{\\\"additionalContext\\\":\\\"extra ctx\\\"}'\"\n",
        )
        .unwrap();
            let catalog = ModelCatalog::from_files(&settings, &models).unwrap();
            let hooks = Agent::compile_hooks(&catalog, dir.path()).unwrap();
            let mut agent = Agent::new(
                catalog,
                Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
                dir.path().join("sessions"),
            )
            .unwrap();
            agent.hooks = hooks;
            // Make journal.sent fail to force the fallback branch.
            std::fs::write(dir.path().join("sessions"), b"not-a-dir").unwrap();

            let before_len = agent.session_len();
            let mut batch = VecDeque::new();
            batch.push_back(input::PendingInput {
                id: 1,
                kind: input::PendingKind::Steering,
                source: "follow".into(),
                expansion: None,
                echo_source: true,
                prepared: None,
            });
            let queue = input::InputQueue::default();
            let mut user = None;
            let err = agent
                .apply_pending(
                    batch,
                    Some(&queue),
                    &mut user,
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .unwrap_err();
            assert!(!err.to_string().is_empty());
            assert_eq!(agent.session_len(), before_len);
        }
    }

    #[test]
    fn model_roles_plan_children_send_each_roles_model_protocol_and_effort() {
        for protocol in ["completion", "response"] {
            let server = TestServer::start().unwrap();
            let address = server.addr().to_string();
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
                let request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    request.target(),
                    if protocol == "completion" {
                        "/v1/chat/completions"
                    } else {
                        "/v1/responses"
                    }
                );
                let body: Value = request.json().unwrap();
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
                        Some("read" | "rg" | "glob" | "web_search" | "lsp")
                    ));
                }
                final_for(protocol, request, "investigation complete", true);
            }
            let result = agent.tools.execute(
                "wait",
                "{}",
                "wait-roles",
                pp_tools::ToolPolicy::Execute {
                    workflow: &agent.workflow,
                },
                &Default::default(),
                &mut |_| {},
            );
            let result: Value = serde_json::from_str(&result.output).unwrap();
            assert_eq!(result["jobs"].as_array().unwrap().len(), 4);
            assert_eq!(result["pending"], false);
            assert!(
                result["jobs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|job| job["status"] == "completed")
            );
            agent.stop_subagents().unwrap();
            assert_eq!(agent.current_model.reference, "fixture/planner");
            assert_eq!(agent.think_level, ThinkLevel::High);
        }
    }

    #[test]
    fn think_override_matching_default_clears_override_and_updates_source() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.yml");
        let models = dir.path().join("models.yml");
        std::fs::write(&settings, "{}").unwrap();
        std::fs::write(
        &models,
        json!({"providers":{"fixture":{
            "api_key":"fixture-key","protocol":"completion","base_url":"http://127.0.0.1:1/v1",
            "models":{"one":{"model":"one","thinking":{"efforts":["low","high"],"defaultLevel":"low"}}}
        }}})
        .to_string(),
    )
    .unwrap();
        let catalog = ModelCatalog::from_files(&settings, &models).unwrap();
        let state = std::sync::Arc::new(pp_config::model_state::ModelState::from_catalog(catalog));
        state
            .set_override(pp_config::model_state::Selection {
                reference: "fixture/one".into(),
                effort: ThinkLevel::High,
            })
            .unwrap();
        let source =
            crate::system_prompt::Snapshot::discover(dir.path(), dir.path(), None, None, None)
                .unwrap();
        let mut agent = Agent::new_with_prompt(
            state.clone(),
            Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
            dir.path().join("sessions"),
            dir.path().join("sessions/.locks"),
            source,
            dir.path().join("sessions/.stats/stats.db"),
        )
        .unwrap();
        let service = crate::model_control::Service::start(state.clone()).unwrap();
        agent.runtime_models = Some(crate::model_control::Binding::main(
            service.registry.clone(),
        ));
        assert_eq!(agent.model_source(), pp_core::model::ModelSource::Override);
        assert!(service.submit(crate::model_control::Change::Think(ThinkLevel::Low)));
        service.flush();
        assert_eq!(state.source(), pp_core::model::ModelSource::Default);
        assert!(service.override_selection().is_none());
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.model_source(), pp_core::model::ModelSource::Default);
    }
}
