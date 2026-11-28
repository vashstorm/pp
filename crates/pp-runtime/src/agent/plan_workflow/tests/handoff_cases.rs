use super::*;

pub(super) fn install_distinct_plan_model(agent: &mut Agent, root: &std::path::Path) {
    let path = root.join("models.yml");
    let mut models: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    models["providers"]["fixture"]["models"]["spare"] = json!({"model":"default-spare"});
    models["providers"]["planner"] = json!({
        "api_key":"fixture-key", "protocol":"response",
        "base_url":"http://127.0.0.1:1/v1", "models":{"one":{"model":"planner-model"}}
    });
    std::fs::write(path, models.to_string()).unwrap();
    agent.reload_models().unwrap();
    for (role, model, effort, fallback) in [
        (
            ModelRole::Default,
            "fixture/one",
            ThinkLevel::Low,
            Some("fixture/spare"),
        ),
        (ModelRole::Plan, "planner/one", ThinkLevel::High, None),
    ] {
        agent
            .save_model_role(
                role,
                Some(pp_config::config::RoleAssignment {
                    enabled: true,
                    model: model.into(),
                    effort,
                    fallback: fallback.map(|model| pp_config::config::RoleModel {
                        model: model.into(),
                        effort: ThinkLevel::Medium,
                    }),
                }),
            )
            .unwrap();
    }
}

#[test]
fn plan_role_save_switches_current_session_and_respects_manual_override() {
    use crate::model_control::{Binding, Change, Service};
    use pp_config::config::{RoleAssignment, RoleModel};

    let dir = tempfile::tempdir().unwrap();
    let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
    install_distinct_plan_model(&mut agent, dir.path());
    let control = Service::start(agent.models()).unwrap();
    agent.runtime_models = Some(Binding::main(control.registry.clone()));
    agent.enter_plan(&CancellationToken::new()).unwrap();
    seed_session(&mut agent, EXPLORATION);

    let fallback = RoleModel {
        model: "planner/one".into(),
        effort: ThinkLevel::Medium,
    };
    assert!(control.submit(Change::Role(
        ModelRole::Plan,
        Some(RoleAssignment {
            enabled: true,
            model: "fixture/spare".into(),
            effort: ThinkLevel::Low,
            fallback: Some(fallback.clone()),
        }),
    )));
    control.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "fixture/spare");
    assert_eq!(agent.think_level, ThinkLevel::Low);
    assert!(matches!(agent.session, Session::Completion(_)));
    assert_eq!(user_texts(&agent), [EXPLORATION]);
    assert_eq!(
        agent.role_fallback(),
        Some(("fixture/spare".into(), fallback))
    );

    agent.switch_model("planner/one").unwrap();
    assert!(control.submit(Change::Role(
        ModelRole::Plan,
        Some(RoleAssignment {
            enabled: true,
            model: "fixture/one".into(),
            effort: ThinkLevel::High,
            fallback: None,
        }),
    )));
    control.flush();
    agent.apply_runtime_models().unwrap();
    assert_eq!(agent.current_model.reference, "planner/one");
    assert_eq!(agent.model_source(), pp_core::model::ModelSource::Override);
}

#[test]
fn plan_exit_restore_and_override_routing() {
    {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
        install_distinct_plan_model(&mut agent, dir.path());
        let cancellation = CancellationToken::new();
        agent.enter_plan(&cancellation).unwrap();
        assert_eq!(agent.current_model.reference, "planner/one");
        assert_eq!(agent.think_level, ThinkLevel::High);
        seed_session(&mut agent, EXPLORATION);
        let id = agent.session_id().unwrap().to_owned();
        let sessions = agent.journal.directory().to_owned();
        let locks = agent.journal.lock_dir().to_owned();

        agent.leave_plan(&cancellation, None).unwrap();
        assert!(matches!(agent.session, Session::Completion(_)));
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(agent.think_level, ThinkLevel::Low);
        assert_eq!(agent.model_source(), pp_core::model::ModelSource::Default);
        assert!(
            agent.role_route.is_none(),
            "leaving Plan does not keep the role route"
        );
        assert_eq!(user_texts(&agent), [EXPLORATION]);
        assert!(
            !agent
                .session
                .tool_names()
                .contains(&"plan_submit".to_owned())
        );
        drop(agent);

        // The checkpoint records only the effective model; role routing no longer takes part in
        // default model derivation.
        let restored = persistence::load_test(&sessions, &locks, Some(&id), true).unwrap();
        assert_eq!(restored.checkpoint.workflow.mode, WorkMode::Execute);
        assert_eq!(restored.checkpoint.model_reference, "fixture/one");
        assert_eq!(restored.checkpoint.think_level, ThinkLevel::Low);
        assert!(restored.checkpoint.role_route.is_none());
    }

    for has_plan_route in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
        install_distinct_plan_model(&mut agent, dir.path());
        agent.enter_plan(&CancellationToken::new()).unwrap();
        agent.workflow = WorkflowState::default();
        if !has_plan_route {
            // The manual selection writes process-level override `a`: it must still apply after
            // restore.
            agent.switch_model("planner/one").unwrap();
        }
        seed_session(&mut agent, EXPLORATION);
        agent.checkpoint_route().unwrap();
        let id = agent.session_id().unwrap().to_owned();
        let sessions = agent.journal.directory().to_owned();
        let locks = agent.journal.lock_dir().to_owned();
        let models = agent.models();
        drop(agent);

        // The old checkpoint still carries the PLAN route (audit only); the restored model =
        // resolve().
        let restored = persistence::load_test(&sessions, &locks, Some(&id), true).unwrap();
        assert_eq!(
            restored
                .checkpoint
                .role_route
                .as_ref()
                .map(|route| route.role),
            has_plan_route.then_some(ModelRole::Plan)
        );
        let tools = Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap();
        let agent = Agent::restore(models, tools, restored).unwrap();
        if has_plan_route {
            // No override: `b` = DEFAULT role fixture/one.
            assert_eq!(agent.current_model.reference, "fixture/one");
            assert_eq!(agent.model_source(), pp_core::model::ModelSource::Default);
        } else {
            // The manual planner/one override is still the same model after restore.
            assert_eq!(agent.current_model.reference, "planner/one");
            assert_eq!(agent.model_source(), pp_core::model::ModelSource::Override);
        }
        assert_eq!(user_texts(&agent), [EXPLORATION]);
        assert!(
            agent.role_route.is_none(),
            "restore does not reapply the checkpoint route"
        );
        drop(agent);
        let saved = persistence::load_test(&sessions, &locks, Some(&id), true).unwrap();
        assert_eq!(
            saved.checkpoint.model_reference,
            if has_plan_route {
                "fixture/one"
            } else {
                "planner/one"
            }
        );
        assert!(
            saved.checkpoint.role_route.is_none(),
            "old route cleared after restore"
        );
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
        install_distinct_plan_model(&mut agent, dir.path());
        // Manual override fixture/spare, distinct from the DEFAULT role (fixture/one) and the
        // PLAN role (planner/one).
        agent.switch_model("fixture/spare").unwrap();
        assert_eq!(agent.model_source(), pp_core::model::ModelSource::Override);
        let expected = agent.current_model.reference.clone();
        let expected_level = agent.think_level;

        // Entering Plan: the manual override wins over the PLAN role.
        agent.enter_plan(&CancellationToken::new()).unwrap();
        assert_eq!(agent.current_model.reference, expected);
        assert_eq!(agent.think_level, expected_level);
        assert_eq!(agent.model_source(), pp_core::model::ModelSource::Override);
        assert_eq!(agent.workflow.mode, WorkMode::Plan);

        // Approval hands over through resolve() and does not switch back to the configured
        // default.
        let token = ready_plan_token(&mut agent);
        let approval = agent
            .approve_plan(
                token,
                ExecutionContext::Keep,
                &InputQueue::default(),
                &CancellationToken::new(),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(approval.workflow.mode, WorkMode::Execute);
        assert_eq!(agent.current_model.reference, expected);
        assert_eq!(agent.think_level, expected_level);
        assert_eq!(agent.model_source(), pp_core::model::ModelSource::Override);

        // Entering and leaving Plan again: the same manual override stays effective.
        agent.enter_plan(&CancellationToken::new()).unwrap();
        assert_eq!(agent.current_model.reference, expected);
        agent.leave_plan(&CancellationToken::new(), None).unwrap();
        assert_eq!(agent.current_model.reference, expected);
        assert_eq!(agent.model_source(), pp_core::model::ModelSource::Override);
    }
}

#[test]
fn plan_toggle_leave_and_save_boundaries() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);
        let session_id = agent.session_id().unwrap().to_owned();

        // Entering Plan only stands by: Plan+None, no document allocated and no file created.
        let workflow = agent.enter_plan(&CancellationToken::new()).unwrap();
        assert_eq!(workflow.mode, WorkMode::Plan);
        assert!(workflow.plan.is_none());
        assert_eq!(journal_workflow(dir.path(), &session_id), workflow);

        // Tasks were committed but revision-0 is not saved yet: leaving is rejected and the
        // workflow is unchanged.
        let mut unsaved = pp_core::plan::PlanDocument::new().unwrap();
        unsaved.origin_prompt = "first task".into();
        agent.workflow.plan = Some(unsaved);
        assert_eq!(
            err_message(agent.leave_plan(&CancellationToken::new(), None)),
            "Confirm discarding the unsaved draft before leaving Plan mode."
        );
        assert_eq!(agent.workflow.mode, WorkMode::Plan);
        assert!(agent.workflow.plan.is_some());

        let id = agent.workflow.plan.as_ref().unwrap().id.clone();
        let previous = agent.workflow.clone();
        assert_eq!(
            err_message(agent.leave_plan(&CancellationToken::new(), Some("stale-id"))),
            "Plan changed; confirm leaving again."
        );
        assert_eq!(agent.workflow, previous);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(agent.leave_plan(&cancelled, Some(&id)).is_err());
        assert_eq!(agent.workflow, previous);
        let preparation = agent
            .leave_plan(&CancellationToken::new(), Some(&id))
            .unwrap();
        assert_eq!(preparation.workflow, WorkflowState::default());
        assert_eq!(preparation.session_id.as_deref(), Some(session_id.as_str()));
        assert!(preparation.replay.is_none());
        assert_eq!(
            journal_workflow(dir.path(), &session_id),
            preparation.workflow
        );

        // Leaving with a saved draft releases the binding instead of marking Abandoned; the file
        // stays on disk.
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
        write_plan_file(&agent, PLAN_CONTENT);
        let path = plan_path(
            dir.path(),
            &agent.workflow.plan.as_ref().unwrap().id.clone(),
        );
        // The earlier confirmation cannot be used for an already saved document.
        assert_eq!(
            err_message(agent.leave_plan(
                &CancellationToken::new(),
                Some(&agent.workflow.plan.as_ref().unwrap().id.clone())
            )),
            "Plan changed; confirm leaving again."
        );
        let preparation = agent.leave_plan(&CancellationToken::new(), None).unwrap();
        assert_eq!(preparation.workflow.mode, WorkMode::Execute);
        assert!(preparation.workflow.plan.is_none());
        assert!(preparation.replay.is_none());
        assert_eq!(preparation.session_id.as_deref(), Some(session_id.as_str()));
        assert_eq!(
            journal_workflow(dir.path(), &session_id),
            preparation.workflow
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), PLAN_CONTENT);

        // Leaving in Execute mode is refused.
        assert_eq!(
            err_message(agent.leave_plan(&CancellationToken::new(), None)),
            NOT_PLAN
        );
    }

    for protocol in ["completion", "response"] {
        for scenario in ["ready", "draft", "changed", "pending", "readonly"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(
                PLAN_CONTENT,
                ORIGIN,
                if scenario == "draft" {
                    PlanStatus::Draft
                } else {
                    PlanStatus::Ready
                },
            );
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let token = agent.read_plan().unwrap().token;
            let path = dir.path().join(&token.path);
            let queue = InputQueue::default();
            if scenario == "changed" {
                std::fs::write(&path, format!("{PLAN_CONTENT}\nExternal change\n")).unwrap();
            } else if scenario == "pending" {
                queue.begin_run().unwrap();
                queue
                    .enqueue(
                        crate::agent::input::PendingKind::Steering,
                        "late question".into(),
                        None,
                        false,
                    )
                    .unwrap();
            } else if scenario == "readonly" {
                let journal = dir
                    .path()
                    .join("sessions")
                    .join(format!("{}.jsonl", token.session_id));
                agent.journal.use_read_only_file(&journal);
            }
            let bytes = std::fs::read(&path).unwrap();
            let result = agent.save_plan_and_leave(token, &queue, &CancellationToken::new());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            if matches!(scenario, "ready" | "draft") {
                let preparation = result.unwrap();
                assert_eq!(preparation.workflow.mode, WorkMode::Execute);
                assert!(preparation.workflow.plan.is_none());
                assert!(preparation.replay.is_none());
            } else {
                assert!(result.is_err(), "{scenario}");
                assert_eq!(agent.workflow.mode, WorkMode::Plan);
                assert!(agent.workflow.plan.is_some());
            }
            assert_eq!(user_texts(&agent).len(), 1);
        }
    }
}

#[test]
fn plan_keep_and_fresh_approval_handoff() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
        write_plan_file(&agent, PLAN_CONTENT);
        seed_session(&mut agent, EXPLORATION);
        let session_id = agent.session_id().unwrap().to_owned();
        let token = agent.read_plan().unwrap().token;

        let preparation = agent
            .approve_plan(
                token.clone(),
                ExecutionContext::Keep,
                &crate::agent::input::InputQueue::default(),
                &CancellationToken::new(),
                &mut |_| {},
            )
            .unwrap();
        assert_eq!(preparation.workflow.mode, WorkMode::Execute);
        assert_eq!(
            preparation.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Executed
        );
        assert!(preparation.workflow.review.is_none());
        assert!(preparation.replay.is_none());
        assert_eq!(preparation.session_id.as_deref(), Some(session_id.as_str()));

        // Exactly one execution message was appended, containing the
        // origin and the full approved plan.
        let texts = user_texts(&agent);
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[0], EXPLORATION);
        assert!(texts[1].contains(ORIGIN));
        assert!(texts[1].contains(PLAN_CONTENT));
        // Tool definitions and system instructions follow Execute.
        assert_eq!(
            session_tool_count(&agent),
            agent.tools.all_definitions(WorkMode::Execute).len()
        );
        let native = serde_json::to_value(&agent.session).unwrap();
        let instructions = match &native["request"]["instructions"] {
            serde_json::Value::String(text) => text.clone(),
            _ => native["request"]["messages"][0]["content"]
                .as_str()
                .unwrap()
                .to_owned(),
        };
        assert!(instructions.contains("EXECUTE MODE"));
        assert!(!instructions.contains("PLAN MODE"));
        // The persisted state matches the handoff.
        assert_eq!(
            journal_workflow(dir.path(), &session_id),
            preparation.workflow
        );
        // Approval handoff: the execution prompt points at the plans path; the Drafts source
        // link has been cleaned up; the registry entry points at Plans.
        let id = agent.workflow.plan.as_ref().unwrap().id.clone();
        assert!(texts[1].contains(&format!(".pp/plans/{id}.md")));
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().location,
            PlanLocation::Plans
        );
        assert!(
            !dir.path()
                .join(".pp/drafts")
                .join(format!("{id}.md"))
                .exists()
        );
        {
            let workspace = agent.tools.plan_workspace();
            let _lock = pp_tools::plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = pp_tools::plan_catalog::load_catalog(workspace).unwrap();
            let entry = snapshot.catalog.active_by_id(&id).unwrap();
            assert_eq!(entry.file_name, format!("{id}.md"));
            assert_eq!(entry.location, PlanLocation::Plans);
        }

        // A second approval is refused; the session token is spent.
        let error = err_message(agent.approve_plan(
            token,
            ExecutionContext::Keep,
            &crate::agent::input::InputQueue::default(),
            &CancellationToken::new(),
            &mut |_| {},
        ));
        assert_eq!(error, NOT_CURRENT);
        assert_eq!(user_texts(&agent).len(), 2);
        drop(preparation);

        // A journal write failure rolls back the whole handoff.
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
        write_plan_file(&agent, PLAN_CONTENT);
        seed_session(&mut agent, EXPLORATION);
        let token = agent.read_plan().unwrap().token;
        let id = agent.session_id().unwrap().to_owned();
        let path = dir.path().join("sessions").join(format!("{id}.jsonl"));
        agent.journal.use_read_only_file(&path);
        let error = err_message(agent.approve_plan(
            token,
            ExecutionContext::Keep,
            &crate::agent::input::InputQueue::default(),
            &CancellationToken::new(),
            &mut |_| {},
        ));
        // After the journal becomes read-only, approval fails at the first persistence point:
        // the workflow stays Plan/Ready, the published target link is kept, and the source link
        // plus registry/binding state are preserved as-is.
        assert_eq!(agent.workflow.mode, WorkMode::Plan);
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Ready
        );
        assert_eq!(user_texts(&agent), vec![EXPLORATION.to_owned()]);
        let id = agent.workflow.plan.as_ref().unwrap().id.clone();
        assert_eq!(
            std::fs::read(plan_path(dir.path(), &id)).unwrap(),
            PLAN_CONTENT.as_bytes()
        );
        drop(error);
    }

    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
        agent.workflow.review = Some(pp_core::plan::PlanReviewReport {
            plan_id: agent.workflow.plan.as_ref().unwrap().id.clone(),
            revision: 1,
            model_reference: "fixture/one".into(),
            content: "- [ ] review finding\n".into(),
        });
        write_plan_file(&agent, PLAN_CONTENT);
        seed_session(&mut agent, EXPLORATION);
        let planning_id = agent.session_id().unwrap().to_owned();
        let token = agent.read_plan().unwrap().token;

        let preparation = agent
            .approve_plan(
                token,
                ExecutionContext::Fresh,
                &crate::agent::input::InputQueue::default(),
                &CancellationToken::new(),
                &mut |_| {},
            )
            .unwrap();
        // not migrate to the execution session.
        // A fresh session with an empty replay was handed over.
        assert!(matches!(&preparation.replay, Some(replay) if replay.is_empty()));
        let execution_id = preparation.session_id.clone().unwrap();
        assert_ne!(execution_id, planning_id);
        assert_eq!(preparation.workflow.mode, WorkMode::Execute);
        assert_eq!(
            preparation.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Executed
        );
        assert!(preparation.workflow.review.is_none());

        // The fresh history carries only the execution prompt: no origin,
        // no planning chat, no review report.
        let texts = user_texts(&agent);
        assert_eq!(texts.len(), 1);
        assert!(texts[0].contains(PLAN_CONTENT));
        assert!(!texts[0].contains("ORIGIN-MARKER"));
        assert!(!texts[0].contains("EXPLORATION-MARKER"));
        assert!(!texts[0].contains("review finding"));
        assert_eq!(
            session_tool_count(&agent),
            agent.tools.all_definitions(WorkMode::Execute).len()
        );

        // Both journals exist: the planning session stays in Plan mode
        // with an Executed, read-only plan binding.
        let source = journal_workflow(dir.path(), &planning_id);
        assert_eq!(source.mode, WorkMode::Plan);
        assert_eq!(source.plan.as_ref().unwrap().status, PlanStatus::Executed);
        assert_eq!(
            journal_workflow(dir.path(), &execution_id),
            preparation.workflow
        );
        // Both bindings point at the same Plans path; the Drafts source link has been cleaned up.
        let id = source.plan.as_ref().unwrap().id.clone();
        assert!(texts[0].contains(&format!(".pp/plans/{id}.md")));
        assert!(
            !dir.path()
                .join(".pp/drafts")
                .join(format!("{id}.md"))
                .exists()
        );
        drop(preparation);

        // Source handoff failure keeps the planning session intact.
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
        write_plan_file(&agent, PLAN_CONTENT);
        seed_session(&mut agent, EXPLORATION);
        let token = agent.read_plan().unwrap().token;
        let id = agent.session_id().unwrap().to_owned();
        let path = dir.path().join("sessions").join(format!("{id}.jsonl"));
        agent.journal.use_read_only_file(&path);
        let error = err_message(agent.approve_plan(
            token,
            ExecutionContext::Fresh,
            &crate::agent::input::InputQueue::default(),
            &CancellationToken::new(),
            &mut |_| {},
        ));
        // After the journal becomes read-only, approval fails at the first persistence point:
        // the workflow stays Plan/Ready, the published target link is kept and the source
        // planning session was not replaced.
        assert_eq!(agent.workflow.mode, WorkMode::Plan);
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Ready
        );
        assert_eq!(user_texts(&agent), vec![EXPLORATION.to_owned()]);
        let id = agent.workflow.plan.as_ref().unwrap().id.clone();
        assert_eq!(
            std::fs::read(plan_path(dir.path(), &id)).unwrap(),
            PLAN_CONTENT.as_bytes()
        );
        drop(error);
    }
}

include!("mcp_handoff_cases.rs");
