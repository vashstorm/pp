use super::*;

#[test]
fn plan_progress_refresh_keeps_verified_snapshot_when_file_becomes_unreadable() {
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
        matches!(published.as_slice(), [crate::events::AgentEvent::PlanProgressUpdated { revision: 2, content, .. }] if content == &progress)
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

#[test]
fn plan_archive_moves_completed_plan_to_done_under_its_file_name() {
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
        // 进度导入不覆盖批准时冻结的任务集合。
        assert_eq!(
            document.approved_tasks.as_deref(),
            Some(approved.as_slice())
        );
        // 正文同步事件 + 纯状态归档事件；归档不递增正文 revision。
        assert!(matches!(
            published.as_slice(),
            [
                crate::events::AgentEvent::PlanProgressUpdated {
                    revision: 2,
                    status: Some(PlanStatus::Executed),
                    location: Some(PlanLocation::Plans),
                    ..
                },
                crate::events::AgentEvent::PlanProgressUpdated {
                    revision: 2,
                    status: Some(PlanStatus::Completed),
                    location: Some(PlanLocation::Done),
                    file_name: Some(name),
                    ..
                },
            ] if name == CANARY_FILE
        ));
        // 登记表指向 Done；重复归档无害。
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
}

#[test]
fn plan_archive_succeeds_without_another_body_change() {
    // 正文已在之前的进度刷新中同步：归档不需要正文再次变化。
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
            "归档不递增正文 revision"
        );
        // 进度刷新已同步、无新变化：只有归档的纯状态事件。
        assert!(matches!(
            published.as_slice(),
            [crate::events::AgentEvent::PlanProgressUpdated {
                revision: 2,
                status: Some(PlanStatus::Completed),
                ..
            }]
        ));
    }
}

#[test]
fn plan_archive_rejects_incomplete_changed_or_empty_task_sets() {
    const FIRST: &str = "Create hello.txt and read back hello.";
    const SECOND: &str = "Verify the failure boundary.";
    let two_tasks = format!("# Create hello\n\n- [ ] {FIRST}\n- [ ] {SECOND}\n");
    let approved = || vec![FIRST.to_owned(), SECOND.to_owned()];

    for protocol in ["completion", "response"] {
        // 未全部勾选：保留 Executed/Plans，不搬迁。
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

        // 零任务：任务数量与批准向量不同即漂移，同步拒绝安装并按
        // 合同报错（不是静默跳过）；文件与 Executed/Plans 保持不变。
        assert!(
            archive_error(&mut agent, "# Create hello\n\nNo tasks here.\n").contains(TASKS_CHANGED)
        );
        assert!(plans.exists() && !done.exists());
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Executed
        );

        // 删除任务后勾选剩余项：任务集合不匹配，合同错误。
        let error = archive_error(&mut agent, &format!("- [x] {FIRST}\n"));
        assert!(error.contains(TASKS_CHANGED), "{error}");

        // 增项：批准集合之外的新任务。
        let error = archive_error(
            &mut agent,
            &format!(
                "{}\n- [x] extra task\n",
                two_tasks.replace("- [ ]", "- [x]")
            ),
        );
        assert!(error.contains(TASKS_CHANGED), "{error}");

        // 改写任务正文：要求重新规划。
        let replaced = two_tasks.replace(FIRST, "Create hello2.txt and read back hello.");
        let error = archive_error(&mut agent, &replaced.replace("- [ ]", "- [x]"));
        assert!(error.contains(TASKS_CHANGED), "{error}");

        // 重排：独立夹具；集合内容相同但顺序不同也不算完成。
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
}

#[test]
fn plan_legacy_executed_builds_approved_tasks_baseline_before_disk_progress() {
    // 旧 Executed 记录缺 approved_tasks：先以当前已登记正文建立一次
    // 迁移基线并持久化，再读取新的磁盘进度；磁盘勾选状态与后续普通
    // 说明变化不覆盖基线。
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
        // 普通说明变化 + 勾选状态：基线保持不变。
        let with_notes = progress.replace(
            "Create hello.txt and read back hello.",
            "Create hello.txt and read back hello.\n\n  说明：验收已人工复核。",
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
        // 基线建立后可正常归档。
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
}

#[test]
fn plan_legacy_executed_without_file_name_restores_and_archives_after_baseline() {
    // 旧记录：无 file_name、缺 approved_tasks、走 <id>.md 兜底路径。
    // 首次进度刷新先按 ID 首次登记，再以登记正文建立迁移基线，随后
    // 正常归档到 done/<id>.md。
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
            [crate::events::AgentEvent::PlanProgressUpdated { revision: 2, .. }]
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
