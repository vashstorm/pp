use super::*;

#[test]
fn todo_sync_reset_legacy_cache_and_rollback() {
    for protocol in ["completion", "response"] {
        for mode in [WorkMode::Execute, WorkMode::Plan] {
            for bound in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let (mut agent, _, _) = executed_todo_fixture(dir.path(), protocol);
                let document = agent.workflow.plan.as_ref().unwrap().clone();
                let path = dir.path().join(document.path().unwrap());
                let bytes = std::fs::read(&path).unwrap();
                agent.workflow.mode = mode;
                if !bound {
                    agent.workflow.plan = None;
                    agent.workflow.todo_plan_id = None;
                }
                seed_session(&mut agent, "old task");
                let previous_id = agent.session_id().unwrap().to_owned();
                let previous = agent.workflow.clone();

                agent.reset();

                let expected = WorkflowState {
                    mode,
                    ..Default::default()
                };
                assert_eq!(agent.workflow, expected);
                assert!(agent.session_id().is_none());
                assert_eq!(std::fs::read(&path).unwrap(), bytes);
                assert_eq!(journal_workflow(dir.path(), &previous_id), previous);

                seed_session(&mut agent, "new task");
                let id = agent.session_id().unwrap();
                assert_ne!(id, previous_id);
                assert_eq!(journal_workflow(dir.path(), id), expected);
            }
        }
    }

    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
        write_plan_file(&agent, PLAN_CONTENT);
        seed_session(&mut agent, EXPLORATION);
        let session_id = agent.session_id().unwrap().to_owned();
        let plan_id = agent.workflow.plan.as_ref().unwrap().id.clone();

        // A legacy draft carried a preview cache; it must be cleared and persisted after the
        // sync.
        agent.workflow.todos =
            pp_tools::todo::project_plan_todos(agent.workflow.plan.as_ref().unwrap(), &[], false)
                .unwrap();
        agent.workflow.todo_plan_id = Some(plan_id.clone());
        sync_plan_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &mut |_| {},
            false,
        )
        .unwrap();
        assert!(agent.workflow.todos.is_empty());
        assert!(agent.workflow.todo_plan_id.is_none());
        assert_eq!(journal_workflow(dir.path(), &session_id), agent.workflow);

        // The source session after a Fresh approval is still in Plan and does not restore the
        // execution TODO.
        agent.workflow.plan.as_mut().unwrap().status = PlanStatus::Executed;
        sync_plan_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &mut |_| {},
            false,
        )
        .unwrap();
        assert!(agent.workflow.todos.is_empty());
        assert!(agent.workflow.todo_plan_id.is_none());

        // Execute+Executed: the execution projection is normalized and the first unchecked item
        // is activated.
        agent.workflow.mode = WorkMode::Execute;
        sync_plan_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &mut |_| {},
            false,
        )
        .unwrap();
        assert_eq!(
            task_states(&agent)[0].1,
            pp_core::todo::TodoStatus::InProgress
        );
        assert_eq!(journal_workflow(dir.path(), &session_id), agent.workflow);

        // A standalone plain list detached from the old plan via init: viewing/refreshing the old
        // Completed document never replaces it and never touches its plan document.
        let content = PLAN_CONTENT.replace("- [ ]", "- [x]");
        write_plan_file(&agent, &content);
        sync_plan_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &mut |_| {},
            false,
        )
        .unwrap();
        assert_eq!(
            task_states(&agent)[0].1,
            pp_core::todo::TodoStatus::Completed
        );
        agent.workflow.plan.as_mut().unwrap().status = PlanStatus::Completed;
        agent.workflow.plan.as_mut().unwrap().location = PlanLocation::Done;
        agent.workflow.todo_plan_id = None;
        agent.workflow.todos = vec![pp_core::todo::TodoPhase {
            name: "Own".into(),
            tasks: vec![pp_core::todo::TodoItem {
                content: "standalone task".into(),
                status: pp_core::todo::TodoStatus::Pending,
                blocker: None,
            }],
        }];
        sync_plan_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &mut |_| {},
            false,
        )
        .unwrap();
        assert_eq!(agent.workflow.todos[0].name, "Own");
        assert_eq!(agent.workflow.todo_plan_id, None);
    }

    for protocol in ["completion", "response"] {
        for bound in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
            write_plan_file(&agent, PLAN_CONTENT);
            agent.workflow.todos = pp_tools::todo::project_plan_todos(
                agent.workflow.plan.as_ref().unwrap(),
                &[],
                false,
            )
            .unwrap();
            agent.workflow.todo_plan_id = Some(agent.workflow.plan.as_ref().unwrap().id.clone());
            if !bound {
                agent.workflow.plan = None;
            }
            seed_session(&mut agent, "legacy planning cache");
            let id = agent.session_id().unwrap().to_owned();
            let snapshot = crate::agent::plan_sync::sync_restored_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &[],
            )
            .unwrap();
            assert!(snapshot.is_none());
            assert!(agent.workflow.todos.is_empty());
            assert!(agent.workflow.todo_plan_id.is_none());
            assert_eq!(agent.workflow.plan.is_some(), bound);
            assert_eq!(journal_workflow(dir.path(), &id), agent.workflow);
        }
    }

    {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), "response", "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
        agent.workflow.todos =
            pp_tools::todo::project_plan_todos(agent.workflow.plan.as_ref().unwrap(), &[], false)
                .unwrap();
        agent.workflow.todo_plan_id = Some(agent.workflow.plan.as_ref().unwrap().id.clone());
        seed_session(&mut agent, "legacy planning cache");
        let previous = agent.workflow.clone();
        agent.journal.use_read_only_file(
            &dir.path()
                .join("sessions")
                .join(format!("{}.jsonl", agent.session_id().unwrap())),
        );
        assert!(
            sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |_| {},
                true,
            )
            .is_err()
        );
        assert_eq!(agent.workflow, previous);
    }
}

#[test]
fn plan_todo_sync_checkboxes_drift_journal_failure_and_readonly() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let (mut agent, first, _second) = executed_todo_fixture(dir.path(), protocol);
        seed_session(&mut agent, EXPLORATION);
        let session_id = agent.session_id().unwrap().to_owned();

        // Content unchanged: runtime state is kept in order and produces no new write or event.
        let mut published = Vec::new();
        sync_plan_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &mut |event| published.push(event),
            false,
        )
        .unwrap();
        assert!(published.is_empty());
        let states = task_states(&agent);
        assert_eq!(states[0].1, pp_core::todo::TodoStatus::InProgress);
        assert_eq!(states[1].1, pp_core::todo::TodoStatus::Blocked);
        assert_eq!(states[1].2.as_deref(), Some("waiting on CI"));

        // The first item is checked externally: body and revision sync, the first item becomes
        // completed, the second keeps blocked and its reason; blocked is not started
        // automatically.
        let progress = agent
            .workflow
            .plan
            .as_ref()
            .unwrap()
            .content
            .replacen("- [ ]", "- [x]", 1);
        write_plan_file(&agent, &progress);
        sync_plan_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &mut |event| published.push(event),
            false,
        )
        .unwrap();
        assert!(matches!(
            published.as_slice(),
            [pp_core::events::AgentEvent::PlanProgressUpdated { revision: 2, .. }]
        ));
        let states = task_states(&agent);
        assert_eq!(
            states[0],
            (first, pp_core::todo::TodoStatus::Completed, None)
        );
        assert_eq!(states[1].1, pp_core::todo::TodoStatus::Blocked);
        assert_eq!(states[1].2.as_deref(), Some("waiting on CI"));
        assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, 2);
        assert_eq!(agent.workflow.plan.as_ref().unwrap().content, progress);
        assert_eq!(journal_workflow(dir.path(), &session_id), agent.workflow);
    }

    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let (mut agent, first, second) = executed_todo_fixture(dir.path(), protocol);
        seed_session(&mut agent, EXPLORATION);
        let document = agent.workflow.plan.as_ref().unwrap();
        let trusted = (document.content.clone(), document.revision);
        let trusted_path = agent
            .tools
            .cwd()
            .join(document.path().expect("saved plan path"));
        let trusted_todos = agent.workflow.todos.clone();

        // Rewrites, additions, deletions and reordering (even with nothing checked) are rejected
        // immediately at the sync point; the last trusted body, revision and runtime state are
        // kept as-is and the disk is not overwritten.
        let drifted = [
            format!("# Create hello\n\n- [ ] Rewritten task.\n- [ ] {second}\n"),
            format!("# Create hello\n\n- [ ] {first}\n- [ ] {second}\n- [ ] extra\n"),
            format!("# Create hello\n\n- [ ] {first}\n"),
            format!("- [ ] {second}\n- [ ] {first}\n"),
        ];
        for drift in &drifted {
            std::fs::write(&trusted_path, drift).unwrap();
            let error = err_message(sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |_| {},
                false,
            ));
            assert!(error.contains(TASKS_CHANGED), "{error}");
            assert_eq!(agent.workflow.plan.as_ref().unwrap().content, trusted.0);
            assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, trusted.1);
            assert_eq!(agent.workflow.todos, trusted_todos);
            assert_eq!(
                agent.workflow.todo_plan_id.as_deref(),
                Some(agent.workflow.plan.as_ref().unwrap().id.as_str())
            );
        }

        // Sync returns to normal once the approved task set is restored.
        std::fs::write(&trusted_path, trusted.0.as_bytes()).unwrap();
        sync_plan_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &mut |_| {},
            false,
        )
        .unwrap();
        assert_eq!(agent.workflow.todos, trusted_todos);
    }

    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let (mut agent, _first, _second) = executed_todo_fixture(dir.path(), protocol);
        seed_session(&mut agent, EXPLORATION);
        let progress = agent
            .workflow
            .plan
            .as_ref()
            .unwrap()
            .content
            .replacen("- [ ]", "- [x]", 1);
        write_plan_file(&agent, &progress);
        let previous = agent.workflow.clone();
        let session_id = agent.session_id().unwrap().to_owned();
        agent.journal.use_read_only_file(
            &dir.path()
                .join("sessions")
                .join(format!("{session_id}.jsonl")),
        );
        let error = err_message(sync_plan_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &mut |_| {},
            false,
        ));
        // Journal read-only: a failed sync rolls back all in-memory state and does not pretend
        // the sync happened.
        assert!(error.contains("last synchronized progress retained"));
        assert_eq!(agent.workflow, previous);
        drop(error);

        // A finished plan still follows the actual Markdown, stays read-only and never returns to
        // the executing state.
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        let done_content = PLAN_CONTENT.replace("- [ ]", "- [x]");
        agent.workflow = plan_workflow(&done_content, ORIGIN, PlanStatus::Completed);
        agent.workflow.mode = WorkMode::Execute;
        let plan_id = agent.workflow.plan.as_ref().unwrap().id.clone();
        let done_file = format!("{plan_id}.md");
        {
            let document = agent.workflow.plan.as_mut().unwrap();
            document.location = PlanLocation::Done;
            document.revision = 2;
            document.file_name = Some(done_file.clone());
        }
        agent.workflow.todo_plan_id = Some(plan_id.clone());
        register_entry(&agent, &plan_id, &done_file, PlanLocation::Done);
        let done_path = dir
            .path()
            .join(".pp/done")
            .join(format!("{}.md", agent.workflow.plan.as_ref().unwrap().id));
        std::fs::create_dir_all(done_path.parent().unwrap()).unwrap();
        std::fs::write(&done_path, &done_content).unwrap();
        sync_plan_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &mut |_| {},
            false,
        )
        .unwrap();
        assert_eq!(
            agent.workflow.todo_plan_id.as_deref(),
            Some(plan_id.as_str())
        );
        assert_eq!(
            task_states(&agent)[0].1,
            pp_core::todo::TodoStatus::Completed
        );
        std::fs::write(&done_path, done_content.replace("[x]", "[ ]")).unwrap();
        sync_plan_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &mut |_| {},
            false,
        )
        .unwrap();
        assert_eq!(task_states(&agent)[0].1, pp_core::todo::TodoStatus::Pending);
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Completed
        );
        let trusted = agent.workflow.clone();
        // When the archived file exists but its body is invalid, keep the verified progress and
        // report the read failure.
        std::fs::write(&done_path, b"\xff\xfe").unwrap();
        assert!(
            sync_plan_todos(
                &agent.tools,
                &mut agent.journal,
                &mut agent.workflow,
                &mut |_| {},
                false
            )
            .is_err()
        );
        assert_eq!(agent.workflow, trusted);
    }
}

#[test]
fn todo_resume_syncs_checkboxes_and_carries_snapshot_once() {
    // prepare_resume reads from the global sessions directory, so re-enter ourselves with a
    // temporary HOME (the same pattern as plan_resume).
    const FIXTURE: &str = "PP_TEST_TODO_RESUME";
    let Ok(root) = std::env::var(FIXTURE) else {
        let home = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "agent::plan_workflow::tests::todo_sync_cases::todo_resume_syncs_checkboxes_and_carries_snapshot_once",
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
    let sessions = root.join(".pp/agent/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    for protocol in ["completion", "response"] {
        // The cache is already trusted and there is no snapshot event: resume must not copy the
        // tool card contents.
        for concurrent in [false, true] {
            let unchanged_dir = root.join(format!("unchanged-exec-{concurrent}"));
            std::fs::create_dir_all(&unchanged_dir).unwrap();
            let (mut unchanged, _, _) = executed_todo_fixture(&unchanged_dir, protocol);
            if concurrent {
                unchanged.workflow.todos[0].tasks[1].status = pp_core::todo::TodoStatus::InProgress;
                unchanged.workflow.todos[0].tasks[1].blocker = None;
            }
            let expected_states = task_states(&unchanged);
            unchanged.journal = SessionJournal::new(sessions.clone());
            seed_session(&mut unchanged, "already synchronized runtime");
            let unchanged_id = unchanged.session_id().unwrap().to_owned();
            let unchanged_document = unchanged.workflow.plan.as_ref().unwrap().clone();
            let unchanged_current = root.join(format!("unchanged-current-{concurrent}"));
            std::fs::create_dir_all(unchanged_current.join(".pp/plans")).unwrap();
            std::fs::write(
                unchanged_current.join(unchanged_document.path().unwrap()),
                &unchanged_document.content,
            )
            .unwrap();
            drop(unchanged);
            let mut resumed = fixture_agent(&unchanged_current, &sessions, protocol);
            seed_session(&mut resumed, "current");
            let preparation = resumed
                .resume(&unchanged_id, &CancellationToken::new())
                .unwrap();
            assert!(preparation.todo_snapshot.is_none());
            assert_eq!(task_states(&resumed), expected_states);
            drop(resumed);
        }
        std::fs::create_dir_all(root.join("exec")).unwrap();
        let (mut source, _first, _second) = executed_todo_fixture(&root.join("exec"), protocol);
        source.journal = SessionJournal::new(sessions.clone());
        // The source session lacks the todo fields (empty todos, empty source): initialize them
        // on resume.
        source.workflow.todos = Vec::new();
        source.workflow.todo_plan_id = None;
        seed_session(&mut source, EXPLORATION);
        let executed_id = source.session_id().unwrap().to_owned();
        let plan_id = source.workflow.plan.as_ref().unwrap().id.clone();
        // On-disk progress: the first item is already checked (prepare the actual file in the
        // execution session directory).
        let progress = source
            .workflow
            .plan
            .as_ref()
            .unwrap()
            .content
            .replacen("- [ ]", "- [x]", 1);
        let current_dir = root.join("current");
        std::fs::create_dir_all(current_dir.join(".pp/plans")).unwrap();
        std::fs::write(
            current_dir.join(".pp/plans").join(format!("{plan_id}.md")),
            &progress,
        )
        .unwrap();
        drop(source);

        let mut agent = fixture_agent(&current_dir, &sessions, protocol);
        seed_session(&mut agent, "current");
        let preparation = agent
            .resume(&executed_id, &CancellationToken::new())
            .unwrap();
        // Resume initializes the execution projection from the actual Markdown: the first item
        // completed, the second normalized to active; the snapshot is published once with the
        // handoff.
        let states = task_states(&agent);
        assert_eq!(states[0].1, pp_core::todo::TodoStatus::Completed);
        assert_eq!(states[1].1, pp_core::todo::TodoStatus::InProgress);
        assert_eq!(
            agent.workflow.todo_plan_id.as_deref(),
            Some(plan_id.as_str())
        );
        assert!(matches!(
            &preparation.todo_snapshot,
            Some(pp_core::events::AgentEvent::TodoSnapshot { plan_id: snapshot_plan, revision: 2, phases, status: PlanStatus::Executed })
                if snapshot_plan == &plan_id && phases == &agent.workflow.todos
        ));
        assert_eq!(agent.workflow.plan.as_ref().unwrap().content, progress);

        // Resuming again: replay already contains the same snapshot, so it is not carried again.
        drop(agent);
        let mut agent = fixture_agent(&current_dir, &sessions, protocol);
        seed_session(&mut agent, "current");
        let preparation = agent
            .resume(&executed_id, &CancellationToken::new())
            .unwrap();
        assert!(preparation.todo_snapshot.is_none());
        assert_eq!(
            agent.workflow.todo_plan_id.as_deref(),
            Some(plan_id.as_str())
        );
    }
}
