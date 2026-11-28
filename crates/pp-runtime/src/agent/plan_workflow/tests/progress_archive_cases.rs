use super::*;

#[test]
fn plan_progress_refresh_and_archive_completion() {
    {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
        agent.workflow.mode = WorkMode::Execute;
        write_plan_file(&agent, PLAN_CONTENT);
        seed_session(&mut agent, EXPLORATION);
        let progress = PLAN_CONTENT.replace("- [ ]", "- [x]");
        write_plan_file(&agent, &progress);
        let mut published = Vec::new();
        agent
            .refresh_plan_progress(&mut |event| published.push(event))
            .unwrap();
        assert!(
            matches!(published.as_slice(), [pp_core::events::AgentEvent::PlanProgressUpdated { revision: 2, content, .. }] if content == &progress)
        );
        let previous = agent.workflow.clone();
        let path = agent.tools.cwd().join(
            agent
                .workflow
                .plan
                .as_ref()
                .unwrap()
                .path()
                .expect("legacy plan has a resolved path"),
        );
        std::fs::remove_file(&path).unwrap();
        published.clear();
        let error = agent
            .refresh_plan_progress(&mut |event| published.push(event))
            .unwrap_err();
        assert!(format!("{error:#}").contains("last synchronized progress retained"));
        assert_eq!(agent.workflow, previous);
        assert!(published.is_empty());
        assert!(!path.exists());
    }

    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let approved = vec![plan::plan_tasks(PLAN_CONTENT).remove(0).text];
        let mut agent = archive_fixture(dir.path(), protocol, PLAN_CONTENT, Some(approved.clone()));
        let progress = PLAN_CONTENT.replace("- [ ]", "- [x]");
        write_plan_file(&agent, &progress);
        let mut published = Vec::new();
        let archived = agent
            .archive_completed_plan(&mut |event| published.push(event))
            .unwrap();
        assert!(archived);
        let (plans, done) = archive_paths(dir.path());
        assert!(!plans.exists());
        assert_eq!(std::fs::read(&done).unwrap(), progress.as_bytes());
        let document = agent.workflow.plan.as_ref().unwrap();
        assert_eq!(document.status, PlanStatus::Completed);
        assert_eq!(document.location, PlanLocation::Done);
        assert_eq!(document.revision, 2);
        assert_eq!(document.file_name.as_deref(), Some(CANARY_FILE));
        // The progress import does not overwrite the task set frozen at approval.
        assert_eq!(
            document.approved_tasks.as_deref(),
            Some(approved.as_slice())
        );
        // Body sync event + pure status archival event; archival does not bump the body
        // revision.
        assert!(matches!(
            published.as_slice(),
            [
                pp_core::events::AgentEvent::PlanProgressUpdated {
                    revision: 2,
                    status: Some(PlanStatus::Executed),
                    location: Some(PlanLocation::Plans),
                    ..
                },
                pp_core::events::AgentEvent::PlanProgressUpdated {
                    revision: 2,
                    status: Some(PlanStatus::Completed),
                    location: Some(PlanLocation::Done),
                    file_name: Some(name),
                    ..
                },
            ] if name == CANARY_FILE
        ));
        // The registry points at Done; a repeated archival is harmless.
        {
            let workspace = agent.tools.plan_workspace();
            let _lock = plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let entry = snapshot.catalog.active_by_id(&document.id).unwrap();
            assert_eq!(entry.location, PlanLocation::Done);
        }
        assert!(!agent.archive_completed_plan(&mut |_| {}).unwrap());
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Completed
        );
        assert!(done.exists());
    }

    // The body was already synced by the earlier progress refresh: archival does not need the
    // body to change again.
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let approved = vec![plan::plan_tasks(PLAN_CONTENT).remove(0).text];
        let progress = PLAN_CONTENT.replace("- [ ]", "- [x]");
        let mut agent = archive_fixture(dir.path(), protocol, &progress, Some(approved.clone()));
        {
            let document = agent.workflow.plan.as_mut().unwrap();
            document.revision = 2;
            document.content = progress.clone();
        }
        let mut published = Vec::new();
        let archived = agent
            .archive_completed_plan(&mut |event| published.push(event))
            .unwrap();
        assert!(archived);
        let (plans, done) = archive_paths(dir.path());
        assert!(!plans.exists());
        assert!(done.exists());
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().revision,
            2,
            "archival does not bump the body revision"
        );
        // The progress refresh has already synced and nothing else changed: only archival's
        // pure status event.
        assert!(matches!(
            published.as_slice(),
            [pp_core::events::AgentEvent::PlanProgressUpdated {
                revision: 2,
                status: Some(PlanStatus::Completed),
                ..
            }]
        ));
    }
}

#[test]
fn plan_archive_drift_rejections_and_legacy_baselines() {
    const FIRST: &str = "Create hello.txt and read back hello.";
    const SECOND: &str = "Verify the failure boundary.";
    let two_tasks = format!("# Create hello\n\n- [ ] {FIRST}\n- [ ] {SECOND}\n");
    let approved = || vec![FIRST.to_owned(), SECOND.to_owned()];

    for protocol in ["completion", "response"] {
        // Not everything is checked: keep Executed/Plans and do not move.
        let dir = tempfile::tempdir().unwrap();
        let mut agent = archive_fixture(dir.path(), protocol, &two_tasks, Some(approved()));
        let (plans, done) = archive_paths(dir.path());
        let partial = two_tasks.replacen("- [ ]", "- [x]", 1);
        assert!(archive_skipped(&mut agent, &partial));
        assert!(plans.exists() && !done.exists());
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Executed
        );
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().location,
            PlanLocation::Plans
        );

        // Zero tasks: a task count differing from the approval vector is drift, so the sync
        // refuses to install it and fails per contract (not a silent skip); the file and
        // Executed/Plans stay unchanged.
        assert!(
            archive_error(&mut agent, "# Create hello\n\nNo tasks here.\n").contains(TASKS_CHANGED)
        );
        assert!(plans.exists() && !done.exists());
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Executed
        );

        // After deleting a task and checking the remaining one: the task set does not match, so
        // it is a contract error.
        let error = archive_error(&mut agent, &format!("- [x] {FIRST}\n"));
        assert!(error.contains(TASKS_CHANGED), "{error}");

        // Added task: a new task outside the approved set.
        let error = archive_error(
            &mut agent,
            &format!(
                "{}\n- [x] extra task\n",
                two_tasks.replace("- [ ]", "- [x]")
            ),
        );
        assert!(error.contains(TASKS_CHANGED), "{error}");

        // Rewritten task body: re-planning is required.
        let replaced = two_tasks.replace(FIRST, "Create hello2.txt and read back hello.");
        let error = archive_error(&mut agent, &replaced.replace("- [ ]", "- [x]"));
        assert!(error.contains(TASKS_CHANGED), "{error}");

        // Reordering: a separate fixture; the same set contents in a different order does not
        // count as complete.
        let dir = tempfile::tempdir().unwrap();
        let mut agent = archive_fixture(dir.path(), protocol, &two_tasks, Some(approved()));
        let (plans, done) = archive_paths(dir.path());
        let reordered = format!("- [x] {SECOND}\n- [x] {FIRST}\n");
        let error = archive_error(&mut agent, &reordered);
        assert!(error.contains(TASKS_CHANGED), "{error}");
        assert!(plans.exists() && !done.exists());
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Executed
        );
    }

    // A legacy Executed record missing approved_tasks: first establish and persist a migration
    // baseline from the currently registered body, then read the new on-disk progress; on-disk
    // check state and later ordinary note changes do not overwrite the baseline.
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = archive_fixture(dir.path(), protocol, PLAN_CONTENT, None);
        let progress = PLAN_CONTENT.replace("- [ ]", "- [x]");
        write_plan_file(&agent, &progress);
        let mut published = Vec::new();
        agent
            .refresh_plan_progress(&mut |event| published.push(event))
            .unwrap();
        let approved = vec![plan::plan_tasks(PLAN_CONTENT).remove(0).text];
        let document = agent.workflow.plan.as_ref().unwrap();
        assert_eq!(
            document.approved_tasks.as_deref(),
            Some(approved.as_slice())
        );
        let session_id = agent.session_id().unwrap().to_owned();
        let persisted = journal_workflow(dir.path(), &session_id);
        let persisted = persisted.plan.expect("plan persisted");
        assert_eq!(
            persisted.approved_tasks.as_deref(),
            Some(approved.as_slice())
        );
        // Ordinary note change + check state: the baseline stays the same.
        let with_notes = progress.replace(
            "Create hello.txt and read back hello.",
            "Create hello.txt and read back hello.\n\n  Notes: acceptance reviewed manually.",
        );
        write_plan_file(&agent, &with_notes);
        agent
            .refresh_plan_progress(&mut |event| published.push(event))
            .unwrap();
        assert_eq!(
            agent
                .workflow
                .plan
                .as_ref()
                .unwrap()
                .approved_tasks
                .as_deref(),
            Some(approved.as_slice())
        );
        // Archival proceeds normally once the baseline exists.
        let archived = agent
            .archive_completed_plan(&mut |event| published.push(event))
            .unwrap();
        assert!(archived);
        let (plans, done) = archive_paths(dir.path());
        assert!(!plans.exists());
        assert!(done.exists());
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Completed
        );
    }

    // Legacy record: no file_name, no approved_tasks, falling back to the <id>.md path.
    // The first progress refresh registers it by ID for the first time, establishes the
    // migration baseline from the registered body and then archives normally to done/<id>.md.
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        let mut workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
        workflow.mode = WorkMode::Execute;
        agent.workflow = workflow;
        seed_session(&mut agent, EXPLORATION);
        let legacy_path = {
            let document = agent.workflow.plan.as_ref().unwrap();
            dir.path().join(document.path().expect("legacy path"))
        };
        std::fs::create_dir_all(legacy_path.parent().unwrap()).unwrap();
        let progress = PLAN_CONTENT.replace("- [ ]", "- [x]");
        std::fs::write(&legacy_path, &progress).unwrap();

        let mut published = Vec::new();
        agent
            .refresh_plan_progress(&mut |event| published.push(event))
            .unwrap();
        let approved = vec![plan::plan_tasks(PLAN_CONTENT).remove(0).text];
        {
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(
                document.approved_tasks.as_deref(),
                Some(approved.as_slice())
            );
            let file_name = document.file_name.clone().expect("registered name");
            assert_eq!(file_name, format!("{}.md", document.id));
            assert_eq!(document.location, PlanLocation::Plans);
        }
        assert!(matches!(
            published.as_slice(),
            [pp_core::events::AgentEvent::PlanProgressUpdated { revision: 2, .. }]
        ));

        let archived = agent
            .archive_completed_plan(&mut |event| published.push(event))
            .unwrap();
        assert!(archived);
        let document = agent.workflow.plan.as_ref().unwrap();
        let done_path = dir
            .path()
            .join(".pp/done")
            .join(document.file_name.clone().expect("name"));
        assert!(!legacy_path.exists());
        assert_eq!(std::fs::read(&done_path).unwrap(), progress.as_bytes());
        assert_eq!(document.status, PlanStatus::Completed);
        assert_eq!(document.location, PlanLocation::Done);
    }
}
