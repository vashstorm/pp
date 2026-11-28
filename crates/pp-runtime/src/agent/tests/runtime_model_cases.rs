mod integration {
    use super::super::*;
    use crate::model_control::Binding;
    use crate::model_control::Change;
    use crate::model_control::Report;
    use crate::model_control::Service;
    use pp_config::config::ModelRole;
    use pp_config::config::RoleAssignment;
    use pp_config::config::RoleModel;
    use pp_config::model_state::Selection;
    use pp_core::model::ModelSource;
    use pp_test_support::TestServer;
    use std::sync::Arc;

    fn fixture(dir: &std::path::Path, protocol: &str) -> Agent {
        let mut agent = runtime_input_agent(dir, protocol, "127.0.0.1:1", "");
        let path = dir.join("models.yml");
        let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        models["providers"]["other"] = json!({"api_key":"fixture", "protocol": if protocol == "completion" {"response"} else {"completion"},
        "base_url":"http://127.0.0.1:1/v1", "models":{"two":{"model":"two"}}});
        std::fs::write(path, models.to_string()).unwrap();
        agent.reload_models().unwrap();
        seed_session(&mut agent, "committed context marker");
        agent
    }
    fn role() -> Option<RoleAssignment> {
        Some(RoleAssignment {
            fallback: None,
            enabled: true,
            model: "other/two".into(),
            effort: ThinkLevel::Low,
        })
    }
    fn role_with(model: &str, effort: ThinkLevel) -> Option<RoleAssignment> {
        Some(RoleAssignment {
            fallback: None,
            enabled: true,
            model: model.into(),
            effort,
        })
    }

    /// One explicit selection: full model reference + resolved effort level.
    fn selection(reference: &str, effort: ThinkLevel) -> Selection {
        Selection {
            reference: reference.into(),
            effort,
        }
    }

    /// Resolve user input into one selection with the process-wide `ModelState`.
    fn choose(agent: &Agent, reference: &str) -> Selection {
        agent
            .models()
            .resolve_reference(reference, Some(agent.think_level()))
            .unwrap_or_else(|error| panic!("resolving {reference}: {error:#}"))
    }

    /// Start the sole writer for this Agent's process-wide model state.
    fn control(agent: &Agent) -> Service {
        Service::start(agent.models()).unwrap()
    }

    /// Thinking effort in the request body; the field sits in different places for the two protocols.
    fn request_effort<'a>(protocol: &str, body: &'a Value) -> &'a Value {
        match protocol {
            "completion" => &body["reasoning_effort"],
            _ => &body["reasoning"]["effort"],
        }
    }

    /// Restore the only top-level thinking field allowed to change in the session snapshot back to its `before` form, so a whole-value
    /// comparison equals a byte-for-byte comparison of `messages`/`input`, `tools` and `instructions`.
    fn restore_effort(protocol: &str, after: &mut Value, before: &Value) {
        let key = match protocol {
            "completion" => "reasoning_effort",
            _ => "reasoning",
        };
        match before.get(key) {
            Some(value) => after[key] = value.clone(),
            None => {
                if let Value::Object(map) = after {
                    map.remove(key);
                }
            }
        }
    }

    /// Another provider on the same protocol and loopback server as the `fixture` provider, so history protocol conversion does not interfere.
    fn add_other_provider(dir: &std::path::Path, protocol: &str, address: &str) {
        let path = dir.join("models.yml");
        let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        models["providers"]["other"] = json!({
            "api_key":"fixture-key",
            "protocol":protocol,
            "base_url":format!("http://{address}/v1"),
            "models":{"two":{"model":"two"}}
        });
        std::fs::write(path, models.to_string()).unwrap();
    }

    /// Startup model selection fixture: `fixture/one` supports only low/high with low as default; the current model first sits on
    /// `other/two` so reference assertions can tell "which model was selected". No request has been sent yet.
    fn select_model_agent(dir: &std::path::Path) -> Agent {
        let mut agent = runtime_input_agent(dir, "completion", "127.0.0.1:1", "");
        let path = dir.join("models.yml");
        let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        models["providers"]["fixture"]["models"]["one"]["thinking"] =
            json!({"efforts":["low", "high"], "defaultLevel":"low"});
        models["providers"]["other"] = json!({
            "api_key":"fixture",
            "protocol":"completion",
            "base_url":"http://127.0.0.1:1/v1",
            "models":{"two":{"model":"two"}}
        });
        std::fs::write(path, models.to_string()).unwrap();
        agent.reload_models().unwrap();
        agent.switch_model("other/two").unwrap();
        agent
    }

    #[test]
    fn select_model_applies_requested_effort_and_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = select_model_agent(dir.path());
        let sessions = dir.path().join("sessions");
        assert_eq!(agent.current_model().reference, "other/two");
        assert_eq!(agent.model_source(), ModelSource::Override);

        // No request has been sent at startup yet: selecting a model creates no session file.
        let selection = agent.select_model("one:high").unwrap();
        assert_eq!(selection.reference, "fixture/one");
        assert_eq!(selection.effort, ThinkLevel::High);
        assert_eq!(agent.current_model().reference, "fixture/one");
        assert_eq!(agent.think_level(), ThinkLevel::High);
        assert_eq!(agent.model_source(), ModelSource::Override);
        assert!(agent.session_id().is_none());
        assert!(
            !sessions.exists(),
            "startup model selection must not create session files"
        );

        // An unsupported effort fails in the catalog layer: both model and effort keep their values.
        let error = agent.select_model("one:medium").unwrap_err().to_string();
        assert!(error.contains("does not support effort medium"), "{error}");
        assert!(error.contains("low, high"), "{error}");
        assert_eq!(agent.current_model().reference, "fixture/one");
        assert_eq!(agent.think_level(), ThinkLevel::High);
        assert_eq!(agent.model_source(), ModelSource::Override);

        // An omitted effort uses the model's explicit default instead of the "keep the current level" semantics.
        let selection = agent.select_model("one").unwrap();
        assert_eq!(selection.reference, "fixture/one");
        assert_eq!(selection.effort, ThinkLevel::Low);
        assert_eq!(agent.current_model().reference, "fixture/one");
        assert_eq!(agent.think_level(), ThinkLevel::Low);
        assert!(agent.session_id().is_none());
        // The selection equals the configured default `b` exactly: no override is kept and the source returns to Default.
        assert_eq!(agent.model_source(), ModelSource::Default);
        assert!(agent.models().override_selection().is_none());
    }

    #[test]
    fn runtime_override_equal_to_default_clears_override_and_source() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = select_model_agent(dir.path());
        assert_eq!(agent.model_source(), ModelSource::Override);

        // Write a selection equal to `b` directly: no override is recorded.
        let default = agent
            .models()
            .resolve_reference("fixture/one", Some(ThinkLevel::Low))
            .unwrap();
        assert!(!agent.models().set_override(default).unwrap());
        assert_eq!(agent.models().source(), ModelSource::Default);
        assert!(agent.models().override_selection().is_none());

        // Switching to the same selection on the session side: the current model lands on `b` and the source stays Default.
        agent.switch_model("fixture/one").unwrap();
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(agent.think_level, ThinkLevel::Low);
        assert_eq!(agent.model_source(), ModelSource::Default);
        assert!(agent.models().override_selection().is_none());
    }

    #[test]
    fn runtime_save_default_keeps_override_and_resolve_follows_default() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = fixture(dir.path(), "completion");
        // Add a switchable configured default model.
        let path = dir.path().join("models.yml");
        let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        models["providers"]["fixture"]["models"]["spare"] = json!({"model": "spare"});
        std::fs::write(path, models.to_string()).unwrap();
        agent.reload_models().unwrap();
        let service = control(&agent);
        agent.runtime_models = Some(Binding::main(service.registry.clone()));

        // No override: saving the default only changes `b`, the running session does not switch actively, but resolve() follows.
        assert!(service.submit(Change::SaveDefault("fixture/spare".into())));
        service.flush();
        service.registry.reports();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(
            agent.models().default_selection().unwrap().reference,
            "fixture/spare"
        );
        assert_eq!(agent.models().resolve().unwrap().reference, "fixture/spare");

        // Session with an override: neither SaveDefault nor a DEFAULT role save changes the current model.
        assert!(service.submit(Change::Override(choose(&agent, "other/two"))));
        service.flush();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, "other/two");
        let override_selection = agent.models().override_selection().unwrap();

        assert!(service.submit(Change::SaveDefault("fixture/one".into())));
        service.flush();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, "other/two");
        assert_eq!(agent.models().resolve().unwrap(), override_selection);

        assert!(service.submit(Change::Role(
            ModelRole::Default,
            role_with("fixture/spare", ThinkLevel::High)
        )));
        service.flush();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, "other/two");
        assert_eq!(agent.model_source(), ModelSource::Override);
        assert_eq!(agent.models().resolve().unwrap(), override_selection);

        // After clearing the override the new `b` takes effect.
        assert!(service.submit(Change::ClearOverride));
        service.flush();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, "fixture/spare");
        assert_eq!(agent.think_level, ThinkLevel::High);
        assert_eq!(agent.model_source(), ModelSource::Default);

        // Without an override `b` changes again: resolve() follows immediately, the running session waits for the next decision point.
        assert!(service.submit(Change::Role(ModelRole::Default, None)));
        service.flush();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.models().resolve().unwrap().reference, "fixture/one");
        assert_eq!(agent.current_model.reference, "fixture/spare");
    }

    #[test]
    fn runtime_default_role_change_keeps_sessions_and_roles_isolate_children() {
        for protocol in ["completion", "response"] {
            let main_dir = tempfile::tempdir().unwrap();
            let child_dir = tempfile::tempdir().unwrap();
            let inherited_dir = tempfile::tempdir().unwrap();
            let nested_dir = tempfile::tempdir().unwrap();
            let mut main = fixture(main_dir.path(), protocol);
            let mut child = fixture(child_dir.path(), protocol);
            let mut inherited = fixture(inherited_dir.path(), protocol);
            let mut nested = fixture(nested_dir.path(), protocol);
            let service = Service::start(main.models()).unwrap();
            let root = Binding::main(service.registry.clone());
            main.runtime_models = Some(root.clone());
            child.runtime_models = Some(root.child(Some(ModelRole::Fast), "a1".into()));
            inherited.runtime_models = Some(root.child(None, "a2".into()));
            nested.runtime_models = Some(
                child
                    .runtime_models
                    .as_ref()
                    .unwrap()
                    .child(Some(ModelRole::Fast), "a3".into()),
            );

            // The DEFAULT role only writes the configured default `b`: no model target is published to the main session or subagents.
            assert!(service.submit(Change::Role(ModelRole::Default, role())));
            service.flush();
            for agent in [&mut main, &mut child, &mut inherited, &mut nested] {
                agent.apply_runtime_models().unwrap();
                assert_eq!(agent.current_model.reference, "fixture/one");
                assert_eq!(agent.model_source(), ModelSource::Default);
            }
            // `b` has changed (the next decision uses other/two) but the running session's model is unchanged.
            assert_eq!(main.models().resolve().unwrap().reference, "other/two");
            assert_eq!(main.current_model.reference, "fixture/one");

            // A FAST role target reaches only FAST subagents (including their nested subagents).
            assert!(service.submit(Change::Role(ModelRole::Fast, role())));
            service.flush();
            child.apply_runtime_models().unwrap();
            nested.apply_runtime_models().unwrap();
            inherited.apply_runtime_models().unwrap();
            main.apply_runtime_models().unwrap();
            assert_eq!(child.current_model.reference, "other/two");
            assert_eq!(child.think_level, ThinkLevel::Low);
            assert_eq!(child.model_source(), ModelSource::Role);
            assert_eq!(nested.current_model.reference, "other/two");
            assert_eq!(nested.think_level, ThinkLevel::Low);
            assert_eq!(inherited.current_model.reference, "fixture/one");
            assert_eq!(main.current_model.reference, "fixture/one");
            for agent in [&main, &child, &nested, &inherited] {
                assert!(
                    serde_json::to_string(&agent.session)
                        .unwrap()
                        .contains("committed context marker")
                );
            }

            // Removing the FAST role only drops the pending slot: running subagents keep their current model.
            assert!(service.submit(Change::Role(ModelRole::Fast, None)));
            service.flush();
            child.apply_runtime_models().unwrap();
            assert!(
                child
                    .runtime_models
                    .as_mut()
                    .unwrap()
                    .take_targets(false)
                    .0
                    .is_none()
            );
            assert_eq!(child.current_model.reference, "other/two");
        }
    }

    #[test]
    fn runtime_model_latest_revision_wins_and_failed_revision_is_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = fixture(dir.path(), "completion");
        let service = control(&agent);
        agent.runtime_models = Some(Binding::main(service.registry.clone()));
        assert!(service.submit(Change::Override(choose(&agent, "other/two"))));
        assert!(service.submit(Change::Override(selection("fixture/one", ThinkLevel::High))));
        service.flush();
        service.registry.reports();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(agent.think_level, ThinkLevel::High);
        assert_eq!(agent.model_source(), ModelSource::Override);

        // On an application failure (read-only journal) the current model and native history are kept and one Failed is reported.
        let snapshot = serde_json::to_value(&agent.session).unwrap();
        let path = dir
            .path()
            .join("sessions")
            .join(format!("{}.jsonl", agent.session_id().unwrap()));
        agent.journal.use_read_only_file(&path);
        assert!(service.submit(Change::Override(choose(&agent, "other/two"))));
        service.flush();
        let queued = service.registry.reports();
        let revision = queued
            .iter()
            .find_map(|report| match report {
                Report::Queued { revision, .. } => Some(*revision),
                _ => None,
            })
            .expect("a manual override is queued before it is applied");
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(serde_json::to_value(&agent.session).unwrap(), snapshot);
        assert!(
        service
            .registry
            .reports()
            .iter()
            .any(|report| matches!(report, Report::Failed { revision: failed, .. } if *failed == revision)),
        "the failed revision is reported with its own revision"
    );
        // The failed revision was already claimed: no retry and no duplicate report.
        agent.apply_runtime_models().unwrap();
        assert!(service.registry.reports().is_empty());
    }

    #[test]
    fn runtime_model_rejects_unsupported_history_without_mutating_it() {
        use async_openai::types::responses as r;
        let dir = tempfile::tempdir().unwrap();
        let mut agent = fixture(dir.path(), "response");
        if let Session::Response(request) = &mut agent.session {
            let r::InputParam::Items(input) = &mut request.input else {
                panic!("native history should be an item list")
            };
            input.push(r::InputItem::EasyMessage(r::EasyInputMessage {
                role: r::Role::User,
                content: r::EasyInputContent::ContentList(vec![r::InputContent::InputImage(
                    r::InputImageContent::default(),
                )]),
                ..Default::default()
            }));
        }
        let before = serde_json::to_value(&agent.session).unwrap();
        let service = control(&agent);
        agent.runtime_models = Some(Binding::main(service.registry.clone()));
        assert!(service.submit(Change::Override(choose(&agent, "other/two"))));
        service.flush();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
        assert!(
            service
                .registry
                .reports()
                .iter()
                .any(|report| matches!(report, Report::Failed { .. }))
        );
    }

    #[test]
    fn runtime_model_role_save_failure_does_not_publish_target() {
        let dir = tempfile::tempdir().unwrap();
        let agent = fixture(dir.path(), "completion");
        let service = control(&agent);
        let mut fast =
            Binding::main(service.registry.clone()).child(Some(ModelRole::Fast), "fast".into());
        std::fs::rename(
            dir.path().join("agent.db"),
            dir.path().join("saved-agent.db"),
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("agent.db")).unwrap();
        assert!(service.submit(Change::Role(ModelRole::Fast, role())));
        service.flush();
        assert!(fast.take_targets(false).0.is_none());
        assert!(
            service
                .registry
                .reports()
                .iter()
                .any(|report| matches!(report, Report::Failed { revision: 0, .. }))
        );
    }

    #[test]
    fn runtime_override_survives_plan_enter_leave_and_restore() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = fixture(dir.path(), "completion");
        // The override is applied to the main session through the runtime control channel (`Change::Override`).
        let service = control(&agent);
        agent.runtime_models = Some(Binding::main(service.registry.clone()));
        assert!(service.submit(Change::Override(choose(&agent, "other/two"))));
        service.flush();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.model_source(), ModelSource::Override);
        let expected = agent.current_model.reference.clone();
        let expected_level = agent.think_level;
        let models = agent.models();

        // Entering Plan: the manual override wins and no PLAN role is applied.
        agent.enter_plan(&CancellationToken::new()).unwrap();
        assert_eq!(agent.current_model.reference, expected);
        assert_eq!(agent.think_level, expected_level);
        assert_eq!(agent.model_source(), ModelSource::Override);
        // Leaving Plan: hand off by resolve(), still the same override model.
        agent.leave_plan(&CancellationToken::new(), None).unwrap();
        assert_eq!(agent.current_model.reference, expected);
        assert_eq!(agent.think_level, expected_level);
        assert_eq!(agent.model_source(), ModelSource::Override);
        assert!(agent.role_route.is_none());

        // Agent::restore also returns to resolve(): the process-wide override on the same ModelState still points at the original model.
        let id = agent.session_id().unwrap().to_owned();
        drop(agent);
        let sessions = dir.path().join("sessions");
        let restored =
            persistence::load_test(&sessions, &sessions.join(".locks"), Some(&id), true).unwrap();
        let agent = Agent::restore(
            models,
            Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
            restored,
        )
        .unwrap();
        assert_eq!(agent.current_model.reference, expected);
        assert_eq!(agent.think_level, expected_level);
        assert_eq!(agent.model_source(), ModelSource::Override);
        assert!(agent.role_route.is_none());
        assert!(
            serde_json::to_string(&agent.session)
                .unwrap()
                .contains("committed context marker")
        );
    }

    #[test]
    fn runtime_fallback_route_uses_default_role_and_returns_to_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = fixture(dir.path(), "completion");
        agent
            .save_model_role(
                ModelRole::Default,
                Some(RoleAssignment {
                    enabled: true,
                    model: "fixture/one".into(),
                    effort: ThinkLevel::Low,
                    fallback: Some(RoleModel {
                        model: "other/two".into(),
                        effort: ThinkLevel::High,
                    }),
                }),
            )
            .unwrap();
        assert_eq!(agent.model_source(), ModelSource::Default);

        let mut events = Vec::new();
        assert!(
            agent
                .activate_fallback(&mut |event| events.push(event))
                .unwrap()
        );
        assert_eq!(agent.current_model.reference, "other/two");
        assert_eq!(agent.think_level, ThinkLevel::High);
        assert_eq!(agent.model_source(), ModelSource::Fallback);
        assert!(events.iter().any(|event| matches!(
            event,
            AgentEvent::ModelRouteChanged {
                model,
                effort: ThinkLevel::High,
                source: ModelSource::Fallback,
                ..
            } if model == "other/two"
        )));
        // Already on the fallback: no second switch.
        assert!(!agent.activate_fallback(&mut |_| {}).unwrap());

        // Task end returns to resolve() (`b` = DEFAULT role fixture/one / low).
        agent.restore_resolved(&mut |_| {}).unwrap();
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(agent.think_level, ThinkLevel::Low);
        assert_eq!(agent.model_source(), ModelSource::Default);
        assert!(agent.role_route.is_none());
    }

    #[test]
    fn runtime_restore_uses_resolved_model_and_drops_checkpoint_route() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = fixture(dir.path(), "completion");
        // The old checkpoint's role route is audit-only: the restored model is decided by resolve().
        agent.role_route = Some(pp_config::config::RoleRoute {
            role: ModelRole::Default,
            primary: RoleModel {
                model: "fixture/one".into(),
                effort: ThinkLevel::Low,
            },
            fallback: Some(RoleModel {
                model: "other/two".into(),
                effort: ThinkLevel::High,
            }),
        });
        agent.checkpoint_route().unwrap();
        let models = agent.models();
        let id = agent.session_id().unwrap().to_owned();
        drop(agent);

        let sessions = dir.path().join("sessions");
        let locks = sessions.join(".locks");
        let saved = persistence::load_test(&sessions, &locks, Some(&id), true).unwrap();
        assert_eq!(
            saved.checkpoint.role_route.as_ref().map(|route| route.role),
            Some(ModelRole::Default)
        );
        let restored = Agent::restore(
            models,
            Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
            saved,
        )
        .unwrap();
        assert_eq!(restored.current_model.reference, "fixture/one");
        assert_eq!(restored.model_source(), ModelSource::Default);
        assert!(
            restored.role_route.is_none(),
            "a restore does not reinstate the checkpoint route"
        );
        drop(restored);

        let saved = persistence::load_test(&sessions, &locks, Some(&id), true).unwrap();
        assert!(
            saved.checkpoint.role_route.is_none(),
            "the old route is cleaned up after restore"
        );
    }

    #[test]
    fn fallback_checkpoint_failure_keeps_model_and_native_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = fixture(dir.path(), "completion");
        agent
            .save_model_role(
                ModelRole::Default,
                Some(RoleAssignment {
                    enabled: true,
                    model: "fixture/one".into(),
                    effort: agent.think_level,
                    fallback: Some(RoleModel {
                        model: "other/two".into(),
                        effort: ThinkLevel::Low,
                    }),
                }),
            )
            .unwrap();
        let before = serde_json::to_value(&agent.session).unwrap();
        let path = dir
            .path()
            .join("sessions")
            .join(format!("{}.jsonl", agent.session_id().unwrap()));
        agent.journal.use_read_only_file(&path);
        assert!(agent.activate_fallback(&mut |_| {}).is_err());
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(agent.model_source(), ModelSource::Default);
        assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
    }

    #[test]
    fn runtime_binding_roles_route_main_children_and_custom_children() {
        let dir = tempfile::tempdir().unwrap();
        let agent = fixture(dir.path(), "completion");
        let service = control(&agent);
        let registry = service.registry.clone();

        // `Change::Override` writes only the main slot: the main Agent gets the manual selection, subagents get nothing.
        assert!(service.submit(Change::Override(selection("other/two", ThinkLevel::None))));
        service.flush();
        let mut main = Binding::main(registry.clone());
        let (target, think) = main.take_targets(false);
        let target = target.expect("manual selection is visible to the main agent");
        assert_eq!(
            target
                .selection
                .as_ref()
                .map(|selection| selection.reference.as_str()),
            Some("other/two")
        );
        assert_eq!(target.source, ModelSource::Override);
        assert!(think.is_none());

        // Each assertion uses its own Binding: `seen` is per Binding, and a subagent consumes only its own role slot.
        let mut default_child = main.child(Some(ModelRole::Default), "child-default".into());
        let mut fast_child = main.child(Some(ModelRole::Fast), "child-fast".into());
        let mut custom_child = main.child(None, "child-custom".into());
        assert!(
            default_child.take_targets(false).0.is_none(),
            "DEFAULT is no longer a subagent role slot and does not inherit the manual override"
        );
        assert!(
            fast_child.take_targets(false).0.is_none(),
            "FAST child must not inherit the main override"
        );
        assert!(custom_child.take_targets(false).0.is_none());

        // A FAST role target writes only roles[Fast]: neither the main Agent nor other children see it.
        assert!(service.submit(Change::Role(
            ModelRole::Fast,
            role_with("other/two", ThinkLevel::Medium)
        )));
        service.flush();
        let (target, _) = fast_child.take_targets(false);
        let target = target.expect("FAST child follows the FAST role");
        let chosen = target.selection.expect("role target carries a selection");
        assert_eq!(chosen.reference, "other/two");
        assert_eq!(chosen.effort, ThinkLevel::Medium);
        assert_eq!(target.source, ModelSource::Role);
        assert!(
            main.take_targets(false).0.is_none(),
            "role targets never enter the main slot"
        );
        assert!(default_child.take_targets(false).0.is_none());
        assert!(custom_child.take_targets(false).0.is_none());

        // The DEFAULT role is the configured default `b`: it only saves config and publishes no model target.
        assert!(service.submit(Change::Role(
            ModelRole::Default,
            role_with("fixture/one", ThinkLevel::High)
        )));
        service.flush();
        // Reuse the same main Binding: a new Binding would see the previous manual target that has not expired yet.
        assert!(
            main.take_targets(false).0.is_none(),
            "DEFAULT role only writes b; it must not publish a main target"
        );
        assert!(fast_child.take_targets(false).0.is_none());

        // Only the main Binding gets the think target, and it does not masquerade as a model target that would make `pending()` true.
        assert!(service.submit(Change::Think(ThinkLevel::High)));
        service.flush();
        let (target, think) = main.take_targets(false);
        assert!(
            target.is_none(),
            "a think change must not publish a model target"
        );
        assert_eq!(
            think.expect("the main agent receives think targets").effort,
            ThinkLevel::High
        );
        let (target, think) = default_child.take_targets(false);
        assert!(target.is_none() && think.is_none());

        // `apply_think_target` must not make `pending()` true: with only the think slot registered it stays false.
        let think_dir = tempfile::tempdir().unwrap();
        let think_agent = fixture(think_dir.path(), "completion");
        let think_only = control(&think_agent);
        let mut probe = Binding::main(think_only.registry.clone());
        assert!(think_only.submit(Change::Think(ThinkLevel::High)));
        think_only.flush();
        assert!(!probe.pending(false), "pending() only tracks model targets");
        let (target, think) = probe.take_targets(false);
        assert!(target.is_none());
        assert_eq!(think.expect("think target").effort, ThinkLevel::High);
        // The same revision is not claimed twice.
        assert!(probe.take_targets(false).1.is_none());

        // Application side: the main Agent consumes no role slot; the source stays Override after the override target switches.
        let route_dir = tempfile::tempdir().unwrap();
        let mut agent = fixture(route_dir.path(), "completion");
        let routes = control(&agent);
        agent.runtime_models = Some(Binding::main(routes.registry.clone()));
        assert!(routes.submit(Change::Role(
            ModelRole::Fast,
            role_with("other/two", ThinkLevel::Low)
        )));
        routes.flush();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert!(agent.role_route.is_none());

        assert!(routes.submit(Change::Override(selection("other/two", ThinkLevel::Low))));
        routes.flush();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, "other/two");
        assert_eq!(agent.model_source(), ModelSource::Override);

        // Claimed in the same batch, applied in revision order: model is older, think is newer, so the higher revision's effort wins.
        assert!(routes.submit(Change::Override(selection("fixture/one", ThinkLevel::Low))));
        assert!(routes.submit(Change::Think(ThinkLevel::High)));
        routes.flush();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(agent.think_level, ThinkLevel::High);

        // think is older and the role target newer: the role target does not enter the main slot, so the manual level still applies.
        assert!(routes.submit(Change::Think(ThinkLevel::Low)));
        assert!(routes.submit(Change::Role(
            ModelRole::Fast,
            role_with("other/two", ThinkLevel::Medium)
        )));
        routes.flush();
        agent.apply_runtime_models().unwrap();
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(agent.think_level, ThinkLevel::Low);
    }

    #[test]
    fn runtime_think_level_applies_to_next_request_mid_task() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("marker.txt"), "marker body").unwrap();
            let server = Arc::new(TestServer::start().unwrap());
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.addr().to_string(), "");
            // Publish one config write first: the pending revision of `Change::Think` comes from ModelState.
            agent.reload_models().unwrap();
            let (tx, rx) = std::sync::mpsc::channel::<Value>();
            let serving = protocol.to_owned();
            let worker = Arc::clone(&server);
            let server_thread = std::thread::spawn(move || {
                let request = worker
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                let body: Value = request.json().unwrap();
                tx.send(body).unwrap();
                tool_call_for(&serving, request, "read", r#"{"path":"marker.txt"}"#);
                let request = worker
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                let body: Value = request.json().unwrap();
                tx.send(body).unwrap();
                final_for(&serving, request, "marker read", true);
            });
            let service = control(&agent);
            agent.runtime_models = Some(Binding::main(service.registry.clone()));
            let mut submitted = false;
            let outcome = agent
                .run(
                    "read the marker",
                    &CancellationToken::new(),
                    None,
                    &mut |event| {
                        // The boundary poll has already passed: this commit can only take effect before the next request.
                        if matches!(event, AgentEvent::ModelRequestStarted) && !submitted {
                            submitted = true;
                            assert!(service.submit(Change::Think(ThinkLevel::High)));
                            service.flush();
                        }
                    },
                )
                .unwrap();
            assert_eq!(outcome, RunOutcome::Completed);
            assert_eq!(agent.think_level(), ThinkLevel::High);
            let first = rx.recv().unwrap();
            let second = rx.recv().unwrap();
            assert_eq!(request_effort(protocol, &first), &json!("none"));
            assert_eq!(request_effort(protocol, &second), &json!("high"));
            if protocol == "response" {
                assert_eq!(second["reasoning"]["summary"], "concise");
            }
            server_thread.join().unwrap();
        }
    }

    #[test]
    fn runtime_subagent_roles_switch_at_next_request_mid_run() {
        for (role, protocol) in [
            (ModelRole::Fast, "completion"),
            (ModelRole::Fast, "response"),
            (ModelRole::Slow, "completion"),
            (ModelRole::Slow, "response"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("marker.txt"), "child marker").unwrap();
            let server = TestServer::start().unwrap();
            let address = server.addr().to_string();
            let mut agent = runtime_input_agent(dir.path(), protocol, &address, "");
            add_other_provider(dir.path(), protocol, &address);
            agent.reload_models().unwrap();
            agent
                .save_model_role(role, role_with("fixture/one", ThinkLevel::Low))
                .unwrap();
            let service = control(&agent);
            agent.runtime_models = Some(Binding::main(service.registry.clone()));
            agent.ensure_subagents().unwrap();
            let arguments = json!({
                "context": "role switch",
                "tasks": [{"role": role.name().to_lowercase(), "task": "inspect the marker"}]
            })
            .to_string();
            let binding = agent.subagents().unwrap();
            binding
                .execute("task", &arguments, true, &Default::default())
                .unwrap();

            // First turn: old model + old effort; commit the role update before replying with a tool call to avoid races.
            let request = server
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let body: Value = request.json().unwrap();
            assert_eq!(body["model"], "one");
            assert_eq!(request_effort(protocol, &body), &json!("low"));
            assert!(service.submit(Change::Role(role, role_with("other/two", ThinkLevel::High))));
            service.flush();
            tool_call_for(protocol, request, "read", r#"{"path":"marker.txt"}"#);

            // Second turn: switch to the new model and new effort within the same run.
            let request = server
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap()
                .unwrap();
            let body: Value = request.json().unwrap();
            assert_eq!(body["model"], "two");
            assert_eq!(request_effort(protocol, &body), &json!("high"));
            final_for(protocol, request, "child done", true);

            let result = agent.tools.execute(
                "wait",
                "{}",
                "wait-role-switch",
                pp_tools::ToolPolicy::Execute {
                    workflow: &agent.workflow,
                },
                &Default::default(),
                &mut |_| {},
            );
            let result: Value = serde_json::from_str(&result.output).unwrap();
            assert_eq!(result["jobs"].as_array().unwrap().len(), 1);
            assert_eq!(result["pending"], false);
            assert_eq!(result["jobs"][0]["status"], "completed");
            agent.stop_subagents().unwrap();
            assert!(
                service
                    .registry
                    .reports()
                    .iter()
                    .any(|report| matches!(report, Report::Applied { agent: Some(_), .. })),
                "the child reports its applied role target"
            );
        }
    }

    #[test]
    fn runtime_effort_prefix_and_think_failure_report_once() {
        for protocol in ["completion", "response"] {
            // Path one: the manual effort slot (`Change::Think`).
            let dir = tempfile::tempdir().unwrap();
            let mut agent = fixture(dir.path(), protocol);
            let service = control(&agent);
            agent.runtime_models = Some(Binding::main(service.registry.clone()));
            let before = serde_json::to_value(&agent.session).unwrap();
            assert!(service.submit(Change::Think(ThinkLevel::High)));
            service.flush();
            service.registry.reports();
            agent.apply_runtime_models().unwrap();
            assert_eq!(agent.think_level, ThinkLevel::High);
            let mut after = serde_json::to_value(&agent.session).unwrap();
            // The session snapshot is `{protocol, request}`; the prefix is the field inside `request`.
            assert_eq!(request_effort(protocol, &after["request"]), &json!("high"));
            restore_effort(protocol, &mut after["request"], &before["request"]);
            assert_eq!(
                after, before,
                "a manual effort change must not rewrite the request prefix"
            );

            // Path two: the role changes only effort (model unchanged) and the subagent binds FAST.
            let dir = tempfile::tempdir().unwrap();
            let mut agent = fixture(dir.path(), protocol);
            let service = control(&agent);
            agent.runtime_models = Some(
                Binding::main(service.registry.clone())
                    .child(Some(ModelRole::Fast), "cache-child".into()),
            );
            let before = serde_json::to_value(&agent.session).unwrap();
            let reference = agent.current_model.reference.clone();
            assert!(service.submit(Change::Role(
                ModelRole::Fast,
                role_with("fixture/one", ThinkLevel::High)
            )));
            service.flush();
            service.registry.reports();
            agent.apply_runtime_models().unwrap();
            assert_eq!(agent.current_model.reference, reference);
            assert_eq!(agent.think_level, ThinkLevel::High);
            let mut after = serde_json::to_value(&agent.session).unwrap();
            assert_eq!(request_effort(protocol, &after["request"]), &json!("high"));
            restore_effort(protocol, &mut after["request"], &before["request"]);
            assert_eq!(
                after, before,
                "a role effort-only change must not rewrite the request prefix"
            );
        }

        {
            let dir = tempfile::tempdir().unwrap();
            let settings = dir.path().join("settings.yml");
            let models = dir.path().join("models.yml");
            std::fs::write(&settings, "rate_limit_retry_budget_seconds: 0\n").unwrap();
            std::fs::write(
                &models,
                json!({"providers":{"fixture":{
                    "api_key":"fixture-key", "protocol":"completion",
                    "base_url":"http://127.0.0.1:1/v1",
                    "models":{"one":{"model":"one","thinking":{"efforts":["low"]}}}
                }}})
                .to_string(),
            )
            .unwrap();
            let mut agent = Agent::new(
                ModelCatalog::from_files(&settings, &models).unwrap(),
                Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
                dir.path().join("sessions"),
            )
            .unwrap();
            assert_eq!(agent.think_level, ThinkLevel::Low);
            // Publish one config write first: the pending revision of `Change::Think` comes from ModelState.
            agent.reload_models().unwrap();
            let service = control(&agent);
            agent.runtime_models = Some(Binding::main(service.registry.clone()));
            let before = serde_json::to_value(&agent.session).unwrap();
            assert!(service.submit(Change::Think(ThinkLevel::Max)));
            service.flush();
            service.registry.reports();
            agent.apply_runtime_models().unwrap();
            assert_eq!(agent.think_level, ThinkLevel::Low);
            assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
            let reports = service.registry.reports();
            assert_eq!(
                reports
                    .iter()
                    .filter(|report| matches!(report, Report::ThinkFailed { .. }))
                    .count(),
                1,
                "unsupported effort reports exactly once"
            );
            assert!(reports.iter().any(|report| matches!(
                report,
                Report::ThinkFailed {
                    effort: ThinkLevel::Max,
                    ..
                }
            )));
            // The failed revision was already claimed: no retry and no duplicate report.
            agent.apply_runtime_models().unwrap();
            assert!(service.registry.reports().is_empty());
        }
    }
}
