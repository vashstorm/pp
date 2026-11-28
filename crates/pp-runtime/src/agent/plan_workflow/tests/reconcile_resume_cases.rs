use super::*;

#[test]
fn plan_missing_archive_continuation_and_restore_detach() {
    use crate::agent::tests::final_for;

    for (protocol, already_reconciled) in [("completion", false), ("response", true)] {
        let dir = tempfile::tempdir().unwrap();
        let server = pp_test_support::TestServer::start().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, &server.addr().to_string(), "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
        let document = agent.workflow.plan.as_mut().unwrap();
        let old_plan_id = document.id.clone();
        let file_name = format!("{old_plan_id}.md");
        let archive_dir = dir.path().join(".pp/done");
        if already_reconciled {
            // An already reconciled legacy session may also lose its whole archive directory
            // between two requests.
            document.status = PlanStatus::Completed;
            document.location = PlanLocation::Done;
            document.file_name = Some(file_name.clone());
        } else {
            std::fs::create_dir_all(&archive_dir).unwrap();
        }
        register_entry(&agent, &old_plan_id, &file_name, PlanLocation::Done);
        seed_session(&mut agent, EXPLORATION);
        let session_id = agent.session_id().unwrap().to_owned();

        let prompt = "Continue planning with the context from this conversation.";
        let serving = std::thread::spawn(move || {
            let request = server
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .unwrap();
            let body: serde_json::Value = request.json().unwrap();
            assert!(body.to_string().contains(EXPLORATION));
            assert!(body.to_string().contains(prompt));
            final_for(protocol, request, "We can continue planning.", true);
        });
        let outcome = agent
            .run(prompt, &CancellationToken::new(), None, &mut |_| {})
            .unwrap();
        serving.join().unwrap();

        assert_eq!(outcome, pp_core::cancellation::RunOutcome::Completed);
        assert_eq!(agent.session_id(), Some(session_id.as_str()));
        assert_eq!(agent.workflow.mode, WorkMode::Plan);
        let draft = agent.workflow.plan.as_ref().unwrap();
        assert_ne!(draft.id, old_plan_id);
        assert_eq!(draft.status, PlanStatus::Draft);
        assert_eq!(draft.origin_prompt, prompt);
        assert_eq!(journal_workflow(dir.path(), &session_id), agent.workflow);
        assert_eq!(
            registered_entry(&agent, &old_plan_id),
            (file_name.clone(), PlanLocation::Done)
        );
        assert!(!archive_dir.join(file_name).exists());
    }

    for bound in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), "response", "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Completed);
        agent.workflow.mode = WorkMode::Execute;
        let document = agent.workflow.plan.as_mut().unwrap();
        document.location = PlanLocation::Done;
        let plan_id = document.id.clone();
        agent.workflow.todos = pp_tools::todo::project_plan_todos(document, &[], false).unwrap();
        let original_todos = agent.workflow.todos.clone();
        agent.workflow.todo_plan_id = bound.then(|| plan_id.clone());
        register_entry(
            &agent,
            &plan_id,
            &format!("{plan_id}.md"),
            PlanLocation::Done,
        );
        seed_session(&mut agent, EXPLORATION);
        let session_id = agent.session_id().unwrap().to_owned();

        let snapshot = crate::agent::plan_sync::sync_restored_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &[],
        )
        .unwrap();

        assert!(snapshot.is_none());
        assert_eq!(agent.workflow.mode, WorkMode::Execute);
        assert!(agent.workflow.plan.is_none());
        assert!(agent.workflow.todo_plan_id.is_none());
        assert_eq!(
            agent.workflow.todos,
            if bound { vec![] } else { original_todos }
        );
        assert_eq!(journal_workflow(dir.path(), &session_id), agent.workflow);
    }
}

#[test]
fn plan_resume_adapts_mcp_and_reports_replay() {
    // /resume reads sessions from a global directory and the child process uses an isolated
    // HOME.
    const FIXTURE: &str = "PP_TEST_PLAN_RESUME";
    let Ok(root) = std::env::var(FIXTURE) else {
        let home = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent::plan_workflow::tests::reconcile_resume_cases::plan_resume_adapts_mcp_and_reports_replay",
                "--nocapture",
            ])
            .env("HOME", home.path())
            .env(FIXTURE, home.path())
            .status()
            .unwrap();
        assert!(status.success());
        return;
    };
    let root = std::path::Path::new(&root);
    let global = root.join(".pp/agent");
    let sessions = global.join("sessions");
    std::fs::create_dir_all(&global).unwrap();
    for protocol in ["completion", "response"] {
        // A saved Plan-mode session and a saved Execute-mode session.
        let mut planner = fixture_agent(&root.join("plan"), &sessions, protocol);
        planner.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
        seed_session(&mut planner, EXPLORATION);
        let planning_id = planner.session_id().unwrap().to_owned();
        let planning_session = serde_json::to_value(&planner.session).unwrap();
        let plan_id = planner.workflow.plan.as_ref().unwrap().id.clone();
        drop(planner);

        // Restore reconciliation resolves the plan file against the current working directory:
        // the resume target session's directory also gets a file for the same plan (a legacy
        // Plans binding moves back to Drafts under the migration rules).
        let current_plan_dir = root.join("current/.pp/plans");
        std::fs::create_dir_all(&current_plan_dir).unwrap();
        std::fs::write(current_plan_dir.join(format!("{plan_id}.md")), PLAN_CONTENT).unwrap();

        let mut executor = fixture_agent(&root.join("exec"), &sessions, protocol);
        seed_session(&mut executor, "ordinary work");
        let execute_id = executor.session_id().unwrap().to_owned();
        drop(executor);

        // A second ordinary session: when the override and resolve paths each leave Plan, the
        // target session cannot be the one that still holds the lock.
        let mut second = fixture_agent(&root.join("exec2"), &sessions, protocol);
        seed_session(&mut second, "second ordinary work");
        let second_execute_id = second.session_id().unwrap().to_owned();
        drop(second);

        // Resuming a Plan session from Execute only tops up MCP access for
        // the target mode; existing connections stay untouched.
        let mut agent = fixture_agent(&root.join("current"), &sessions, protocol);
        seed_session(&mut agent, "current");
        let preparation = agent
            .resume(&planning_id, &CancellationToken::new())
            .unwrap();
        assert_eq!(preparation.workflow.mode, WorkMode::Plan);
        assert_eq!(
            preparation.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Ready
        );
        assert_eq!(
            preparation.session_id.as_deref(),
            Some(planning_id.as_str())
        );
        assert!(preparation.replay.is_some());
        // The history matches; the system prompt is freshly composed for
        // the current working directory and Plan mode.
        let mut native = serde_json::to_value(&agent.session).unwrap();
        match protocol {
            "response" => {
                native["request"]["instructions"] =
                    planning_session["request"]["instructions"].clone();
            }
            _ => {
                native["request"]["messages"][0]["content"] =
                    planning_session["request"]["messages"][0]["content"].clone();
            }
        }
        assert_eq!(native, planning_session);

        // Using a separate model in the planning session (writing manual override `a`) must
        // survive resuming an ordinary session.
        super::handoff_cases::install_distinct_plan_model(&mut agent, &global);
        agent.switch_model("planner/one").unwrap();
        agent.set_think_level(ThinkLevel::High).unwrap();
        let preparation = agent
            .resume(&execute_id, &CancellationToken::new())
            .unwrap();
        assert_eq!(preparation.workflow.mode, WorkMode::Execute);
        assert!(preparation.workflow.plan.is_none());
        assert_eq!(agent.current_model.reference, "planner/one");
        assert_eq!(agent.think_level, ThinkLevel::High);
        assert_eq!(agent.model_source(), pp_core::model::ModelSource::Override);
        assert!(agent.role_route.is_none());
        assert_eq!(
            session_tool_count(&agent),
            agent.tools.all_definitions(WorkMode::Execute).len()
        );

        // After clearing the override, resume an ordinary session from Plan again: the model
        // returns to resolve() (the default route of `b`).
        agent.models().clear_override().unwrap();
        agent.enter_plan(&CancellationToken::new()).unwrap();
        let preparation = agent
            .resume(&second_execute_id, &CancellationToken::new())
            .unwrap();
        assert_eq!(preparation.workflow.mode, WorkMode::Execute);
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(agent.think_level, ThinkLevel::Low);
        assert_eq!(agent.model_source(), pp_core::model::ModelSource::Default);
        assert!(agent.role_route.is_none());
    }
}

#[test]
fn plan_reconcile_migrations_adoption_and_fail_closed() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
        write_plan_file(&agent, PLAN_CONTENT);
        let id = agent.workflow.plan.as_ref().unwrap().id.clone();
        let legacy_path = format!(".pp/plans/{id}.md");

        // First registration of a legacy Draft+Plans binding: keep the old name and move back to
        // Drafts, preserving bytes exactly; a pure path change does not bump the revision.
        agent.reconcile_plan_location().unwrap();
        {
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(document.revision, 1);
            assert_eq!(document.content, PLAN_CONTENT);
            assert_eq!(
                document.file_name.as_deref(),
                Some(format!("{id}.md").as_str())
            );
            assert_eq!(document.location, PlanLocation::Drafts);
        }
        assert_eq!(
            std::fs::read(dir.path().join(".pp/drafts").join(format!("{id}.md"))).unwrap(),
            PLAN_CONTENT.as_bytes()
        );
        assert!(!dir.path().join(&legacy_path).exists());
        assert_eq!(
            registered_entry(&agent, &id),
            (format!("{id}.md"), PlanLocation::Drafts)
        );

        // The new path resolved from the registry invalidates the old path token (after a
        // rename/move the old token can no longer authorize).
        agent.workflow.plan.as_mut().unwrap().status = PlanStatus::Ready;
        let stale = ReviewToken {
            session_id: agent.session_id().unwrap().to_owned(),
            plan_id: id.clone(),
            path: legacy_path,
            revision: 1,
            content: PLAN_CONTENT.to_owned(),
        };
        let error = err_message(agent.validate_plan_token(&stale, &InputQueue::default()));
        assert_eq!(error, NOT_CURRENT);

        // Reconciling again is idempotent: no journal write when the binding and registration
        // already agree.
        let before = agent.workflow.clone();
        agent.reconcile_plan_location().unwrap();
        assert_eq!(agent.workflow, before);
    }

    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
        let id = agent.workflow.plan.as_ref().unwrap().id.clone();

        // Another session already renamed/moved the plan: the registry wins and the binding
        // adopts the new identity.
        let plans = dir.path().join(".pp/plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("moved-name.md"), PLAN_CONTENT).unwrap();
        register_entry(&agent, &id, "moved-name.md", PlanLocation::Plans);
        // The old binding path <id>.md leaves a same-inode leftover link.
        std::fs::hard_link(plans.join("moved-name.md"), plans.join(format!("{id}.md"))).unwrap();

        agent.reconcile_plan_location().unwrap();
        {
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(document.file_name.as_deref(), Some("moved-name.md"));
            assert_eq!(document.location, PlanLocation::Plans);
            assert_eq!(document.status, PlanStatus::Executed);
            assert_eq!(document.content, PLAN_CONTENT);
            assert_eq!(document.revision, 1);
        }
        // The same-inode leftover is cleaned up; the registry entry stays as-is.
        assert!(!plans.join(format!("{id}.md")).exists());
        assert_eq!(
            std::fs::read(plans.join("moved-name.md")).unwrap(),
            PLAN_CONTENT.as_bytes()
        );
        assert_eq!(
            registered_entry(&agent, &id),
            ("moved-name.md".to_owned(), PlanLocation::Plans)
        );
    }

    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);

        // A legacy Executed binding is still registered in place under Plans and is neither
        // moved nor renamed.
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
        write_plan_file(&agent, PLAN_CONTENT);
        let executed_id = agent.workflow.plan.as_ref().unwrap().id.clone();
        agent.reconcile_plan_location().unwrap();
        {
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(
                document.file_name.as_deref(),
                Some(format!("{executed_id}.md").as_str())
            );
            assert_eq!(document.location, PlanLocation::Plans);
            assert_eq!(document.status, PlanStatus::Executed);
        }
        assert!(
            dir.path()
                .join(".pp/plans")
                .join(format!("{executed_id}.md"))
                .exists()
        );
        assert_eq!(
            registered_entry(&agent, &executed_id),
            (format!("{executed_id}.md"), PlanLocation::Plans)
        );

        // A historical Executed registered in Done syncs Completed read-only without
        // re-authorizing.
        let done_dir = dir.path().join(".pp/done");
        std::fs::create_dir_all(&done_dir).unwrap();
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
        let done_id = agent.workflow.plan.as_ref().unwrap().id.clone();
        std::fs::write(done_dir.join(format!("{done_id}.md")), PLAN_CONTENT).unwrap();
        register_entry(
            &agent,
            &done_id,
            &format!("{done_id}.md"),
            PlanLocation::Done,
        );
        agent.reconcile_plan_location().unwrap();
        {
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(document.status, PlanStatus::Completed);
            assert_eq!(document.location, PlanLocation::Done);
            assert_eq!(document.content, PLAN_CONTENT);
        }
        agent.workflow.mode = WorkMode::Execute;
        agent.workflow.validate().unwrap();

        // A legacy Abandoned is registered read-only in its original location.
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Abandoned);
        write_plan_file(&agent, PLAN_CONTENT);
        let abandoned_id = agent.workflow.plan.as_ref().unwrap().id.clone();
        agent.reconcile_plan_location().unwrap();
        {
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(document.location, PlanLocation::Plans);
            assert_eq!(document.status, PlanStatus::Abandoned);
        }
        assert_eq!(
            registered_entry(&agent, &abandoned_id),
            (format!("{abandoned_id}.md"), PlanLocation::Plans)
        );
    }

    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);

        // A revision-0 binding passes straight through without creating a registry file.
        agent.workflow = plan_workflow("", ORIGIN, PlanStatus::Draft);
        agent.reconcile_plan_location().unwrap();
        assert!(!dir.path().join(".pp/plan-index.json").exists());

        // Tombstone: a legacy session accessing a deleted plan gets an explicit error.
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
        write_plan_file(&agent, PLAN_CONTENT);
        let id = agent.workflow.plan.as_ref().unwrap().id.clone();
        {
            let workspace = agent.tools.plan_workspace();
            let _lock = pp_tools::plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = pp_tools::plan_catalog::load_catalog(workspace).unwrap();
            let mut catalog = snapshot.catalog;
            catalog
                .entries
                .push(pp_tools::plan_catalog::PlanCatalogEntry {
                    id: id.clone(),
                    file_name: format!("{id}.md"),
                    location: PlanLocation::Plans,
                    deleted: true,
                    deleted_file_identity: None,
                });
            pp_tools::plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog)
                .unwrap();
        }
        let error = err_message(agent.reconcile_plan_location());
        assert_eq!(
            error,
            "Plan was deleted; choose another plan from /plan-list."
        );

        // Missing body: the registration is rejected, the binding and registry stay as they
        // were, and nothing is restored from a snapshot.
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
        let missing_id = agent.workflow.plan.as_ref().unwrap().id.clone();
        let previous = agent.workflow.clone();
        let error = err_message(agent.reconcile_plan_location());
        assert!(!error.is_empty());
        assert_eq!(agent.workflow, previous);
        let workspace = agent.tools.plan_workspace();
        let _lock = pp_tools::plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = pp_tools::plan_catalog::load_catalog(workspace).unwrap();
        assert!(snapshot.catalog.active_by_id(&missing_id).is_none());
    }
}
