use super::*;

#[test]
fn plan_toggle_enters_standby_and_leave_unbinds_without_abandoning() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);
        let session_id = agent.session_id().unwrap().to_owned();

        // 进入 Plan 只待命：Plan+None，不分配文档也不创建文件。
        let workflow = agent.enter_plan(&CancellationToken::new()).unwrap();
        assert_eq!(workflow.mode, WorkMode::Plan);
        assert!(workflow.plan.is_none());
        assert_eq!(journal_workflow(dir.path(), &session_id), workflow);

        // 已提交任务但 revision-0 尚未首存：退出被拒绝且工作流不变。
        let mut unsaved = crate::plan::PlanDocument::new().unwrap();
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

        // 已保存草稿退出：解除绑定而非 Abandoned；文件保留在磁盘。
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
        write_plan_file(&agent, PLAN_CONTENT);
        let path = plan_path(
            dir.path(),
            &agent.workflow.plan.as_ref().unwrap().id.clone(),
        );
        // 原确认不能用于已经首存的文档。
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
}

#[test]
fn plan_save_and_leave_requires_current_saved_bytes_and_preserves_failures() {
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
fn plan_keep_approval_appends_one_message_and_rolls_back_on_failure() {
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
        // 审批交接：execution prompt 指向 plans 路径；Drafts 源链接已
        // 清理；登记表条目指向 Plans。
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
            let _lock = crate::tools::plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = crate::tools::plan_catalog::load_catalog(workspace).unwrap();
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
        // journal 只读后审批在首次持久化点失败：workflow 保持 Plan/Ready，
        // 已发布的目标链接保留，源链接与登记/绑定状态如实保留。
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

#[test]
fn plan_fresh_approval_starts_an_empty_session_and_keeps_source_readable() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
        agent.workflow.review = Some(crate::plan::PlanReviewReport {
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
        // 两个绑定都指向同一 Plans 路径；Drafts 源链接已清理。
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
        // journal 只读后审批在首次持久化点失败：workflow 保持 Plan/Ready，
        // 已发布的目标链接保留，源规划会话未被替换。
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
