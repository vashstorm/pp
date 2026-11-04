use super::*;

#[test]
fn plan_resume_adapts_mcp_and_reports_replay() {
    // prepare_resume loads from the global sessions directory, so the
    // test re-executes itself with HOME pointed at a temporary home.
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

        // 恢复协调按当前工作目录解析计划文件：resume 目标会话的目录
        // 里也准备同一计划的文件（旧 Plans 绑定按迁移规则迁回 Drafts）。
        let current_plan_dir = root.join("current/.pp/plans");
        std::fs::create_dir_all(&current_plan_dir).unwrap();
        std::fs::write(current_plan_dir.join(format!("{plan_id}.md")), PLAN_CONTENT).unwrap();

        let mut executor = fixture_agent(&root.join("exec"), &sessions, protocol);
        seed_session(&mut executor, "ordinary work");
        let execute_id = executor.session_id().unwrap().to_owned();
        drop(executor);

        // Resuming a Plan session from Execute stops the managed registry
        // before the planning session takes over.
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

        // Resuming an Execute session from Plan reconnects ordinary MCP.
        let preparation = agent
            .resume(&execute_id, &CancellationToken::new())
            .unwrap();
        assert_eq!(preparation.workflow.mode, WorkMode::Execute);
        assert!(preparation.workflow.plan.is_none());
        assert_eq!(
            session_tool_count(&agent),
            agent.tools.all_definitions(WorkMode::Execute).len()
        );
    }
}

#[test]
fn plan_reconcile_migrates_legacy_draft_to_drafts_and_moves_the_file() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
        write_plan_file(&agent, PLAN_CONTENT);
        let id = agent.workflow.plan.as_ref().unwrap().id.clone();
        let legacy_path = format!(".pp/plans/{id}.md");

        // 旧 Draft+Plans 绑定首次登记：保留旧名迁回 Drafts，字节精确保留，
        // 纯路径变化不递增 revision。
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

        // 登记解析出的新路径使旧 path token 失效（重命名/移位后旧 token
        // 不能继续授权）。
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

        // 再次协调是幂等的：绑定与登记一致时不再写 journal。
        let before = agent.workflow.clone();
        agent.reconcile_plan_location().unwrap();
        assert_eq!(agent.workflow, before);
    }
}

#[test]
fn plan_reconcile_adopts_registry_and_cleans_confirmed_leftovers() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
        let id = agent.workflow.plan.as_ref().unwrap().id.clone();

        // 另一会话已把计划重命名/移位：登记优先，绑定采纳新身份。
        let plans = dir.path().join(".pp/plans");
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("moved-name.md"), PLAN_CONTENT).unwrap();
        register_entry(&agent, &id, "moved-name.md", PlanLocation::Plans);
        // 旧绑定路径 <id>.md 留下同 inode 残留链接。
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
        // 同 inode 残留被清理；登记表条目保持原样。
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
}

#[test]
fn plan_reconcile_registers_legacy_states_in_place_and_syncs_done() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);

        // 旧 Executed 绑定仍在 Plans 原位登记，不搬迁、不改名。
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

        // 登记到 Done 的历史 Executed 只读同步 Completed，不重新授权。
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

        // 旧 Abandoned 保留原位置只读登记。
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
}

#[test]
fn plan_reconcile_refuses_tombstones_and_missing_files_without_registration() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);

        // revision-0 绑定直接通过，不建立登记文件。
        agent.workflow = plan_workflow("", ORIGIN, PlanStatus::Draft);
        agent.reconcile_plan_location().unwrap();
        assert!(!dir.path().join(".pp/plan-index.json").exists());

        // 墓碑：旧会话访问已删除的计划得到明确错误。
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
        write_plan_file(&agent, PLAN_CONTENT);
        let id = agent.workflow.plan.as_ref().unwrap().id.clone();
        {
            let workspace = agent.tools.plan_workspace();
            let _lock = crate::tools::plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = crate::tools::plan_catalog::load_catalog(workspace).unwrap();
            let mut catalog = snapshot.catalog;
            catalog
                .entries
                .push(crate::tools::plan_catalog::PlanCatalogEntry {
                    id: id.clone(),
                    file_name: format!("{id}.md"),
                    location: PlanLocation::Plans,
                    deleted: true,
                    deleted_file_identity: None,
                });
            crate::tools::plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog)
                .unwrap();
        }
        let error = err_message(agent.reconcile_plan_location());
        assert_eq!(
            error,
            "Plan was deleted; choose another plan from /plan-list."
        );

        // 正文缺失：登记被拒绝，绑定与登记表保持原状，不从快照恢复。
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
        let missing_id = agent.workflow.plan.as_ref().unwrap().id.clone();
        let previous = agent.workflow.clone();
        let error = err_message(agent.reconcile_plan_location());
        assert!(!error.is_empty());
        assert_eq!(agent.workflow, previous);
        let workspace = agent.tools.plan_workspace();
        let _lock = crate::tools::plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = crate::tools::plan_catalog::load_catalog(workspace).unwrap();
        assert!(snapshot.catalog.active_by_id(&missing_id).is_none());
    }
}
