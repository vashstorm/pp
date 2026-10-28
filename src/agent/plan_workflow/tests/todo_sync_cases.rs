use super::*;

#[test]
fn todo_sync_projects_preview_sources_without_active_and_executes_only_executed() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
        write_plan_file(&agent, PLAN_CONTENT);
        seed_session(&mut agent, EXPLORATION);
        let session_id = agent.session_id().unwrap().to_owned();
        let plan_id = agent.workflow.plan.as_ref().unwrap().id.clone();

        // 草稿预览：来源绑定当前计划，全部 pending，无 active；状态持久化。
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
        assert_eq!(agent.workflow.todos.len(), 1);
        assert_eq!(agent.workflow.todos[0].name, "Tasks");
        assert_eq!(
            task_states(&agent)[0].1,
            crate::tools::todo::TodoStatus::Pending
        );
        assert_eq!(journal_workflow(dir.path(), &session_id), agent.workflow);

        // Plan+Executed 来源会话（Fresh 批准后的规划会话）：预览保持
        // 只读，不归一化激活。
        agent.workflow.plan.as_mut().unwrap().status = PlanStatus::Executed;
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
            crate::tools::todo::TodoStatus::Pending
        );

        // Execute+Executed：执行投影归一化，激活首个未勾选项。
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
            crate::tools::todo::TodoStatus::InProgress
        );
        assert_eq!(journal_workflow(dir.path(), &session_id), agent.workflow);

        // 已用 init 脱离旧计划的独立普通清单：查看/刷新旧 Completed
        // 文档绝不替换它，也不触碰其计划文档。
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
            crate::tools::todo::TodoStatus::Completed
        );
        agent.workflow.plan.as_mut().unwrap().status = PlanStatus::Completed;
        agent.workflow.plan.as_mut().unwrap().location = PlanLocation::Done;
        agent.workflow.todo_plan_id = None;
        agent.workflow.todos = vec![crate::tools::todo::TodoPhase {
            name: "Own".into(),
            tasks: vec![crate::tools::todo::TodoItem {
                content: "standalone task".into(),
                status: crate::tools::todo::TodoStatus::Pending,
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
}

#[test]
fn plan_todo_sync_merges_external_checkboxes_and_preserves_runtime() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let (mut agent, first, _second) = executed_todo_fixture(dir.path(), protocol);
        seed_session(&mut agent, EXPLORATION);
        let session_id = agent.session_id().unwrap().to_owned();

        // 内容未变：运行态按序保留，不产生新写入或事件。
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
        assert_eq!(states[0].1, crate::tools::todo::TodoStatus::InProgress);
        assert_eq!(states[1].1, crate::tools::todo::TodoStatus::Blocked);
        assert_eq!(states[1].2.as_deref(), Some("waiting on CI"));

        // 外部勾选第一项：正文与 revision 同步，第一项转 completed，
        // 第二项保留 blocked 与原因；blocked 不自动启动。
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
            [crate::events::AgentEvent::PlanProgressUpdated { revision: 2, .. }]
        ));
        let states = task_states(&agent);
        assert_eq!(
            states[0],
            (first, crate::tools::todo::TodoStatus::Completed, None)
        );
        assert_eq!(states[1].1, crate::tools::todo::TodoStatus::Blocked);
        assert_eq!(states[1].2.as_deref(), Some("waiting on CI"));
        assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, 2);
        assert_eq!(agent.workflow.plan.as_ref().unwrap().content, progress);
        assert_eq!(journal_workflow(dir.path(), &session_id), agent.workflow);
    }
}

#[test]
fn plan_todo_sync_refuses_approved_vector_drift_and_keeps_trusted_runtime() {
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

        // 改写、增项、删除、重排（即使全部未勾选）都在同步处立即拒绝；
        // 最后可信的正文、revision 与运行态原样保留，磁盘不被覆盖。
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

        // 恢复批准任务集合后同步恢复正常。
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
}

#[test]
fn plan_todo_sync_rolls_back_on_journal_failure_and_keeps_completed_readonly() {
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
        // journal 只读：同步失败回滚全部内存状态，不假装已同步。
        assert!(error.contains("last synchronized progress retained"));
        assert_eq!(agent.workflow, previous);
        drop(error);

        // 已结束计划仍以实际 Markdown 为准，始终只读，不恢复执行态。
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
            crate::tools::todo::TodoStatus::Completed
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
        assert_eq!(
            task_states(&agent)[0].1,
            crate::tools::todo::TodoStatus::Pending
        );
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Completed
        );
        let trusted = agent.workflow.clone();
        std::fs::remove_file(&done_path).unwrap();
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
    // prepare_resume 从全局 sessions 目录读取，因此用临时 HOME 重入
    // 自身（与 plan_resume 相同模式）。
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
        // 正常缓存已经可信，且没有快照事件：resume 不能复制工具卡片内容。
        let unchanged_dir = root.join("unchanged-exec");
        std::fs::create_dir_all(&unchanged_dir).unwrap();
        let (mut unchanged, _, _) = executed_todo_fixture(&unchanged_dir, protocol);
        unchanged.journal = SessionJournal::new(sessions.clone());
        seed_session(&mut unchanged, "already synchronized runtime");
        let unchanged_id = unchanged.session_id().unwrap().to_owned();
        let unchanged_document = unchanged.workflow.plan.as_ref().unwrap().clone();
        let unchanged_current = root.join("unchanged-current");
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
        assert_eq!(
            task_states(&resumed)[1].1,
            crate::tools::todo::TodoStatus::Blocked
        );
        drop(resumed);
        std::fs::create_dir_all(root.join("exec")).unwrap();
        let (mut source, _first, _second) = executed_todo_fixture(&root.join("exec"), protocol);
        source.journal = SessionJournal::new(sessions.clone());
        // 源会话缺 todo 字段（todos 为空、来源为空）：恢复时初始化。
        source.workflow.todos = Vec::new();
        source.workflow.todo_plan_id = None;
        seed_session(&mut source, EXPLORATION);
        let executed_id = source.session_id().unwrap().to_owned();
        let plan_id = source.workflow.plan.as_ref().unwrap().id.clone();
        // 磁盘进度：第一项已勾选（执行会话目录里准备实际文件）。
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
        // 恢复时按实际 Markdown 初始化执行投影：第一项 completed，
        // 第二项归一化激活；快照随交接发布一次。
        let states = task_states(&agent);
        assert_eq!(states[0].1, crate::tools::todo::TodoStatus::Completed);
        assert_eq!(states[1].1, crate::tools::todo::TodoStatus::InProgress);
        assert_eq!(
            agent.workflow.todo_plan_id.as_deref(),
            Some(plan_id.as_str())
        );
        assert!(matches!(
            &preparation.todo_snapshot,
            Some(crate::events::AgentEvent::TodoSnapshot { plan_id: snapshot_plan, revision: 2, phases, status: PlanStatus::Executed })
                if snapshot_plan == &plan_id && phases == &agent.workflow.todos
        ));
        assert_eq!(agent.workflow.plan.as_ref().unwrap().content, progress);

        // 再次恢复：replay 已包含相同快照，不重复携带。
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
