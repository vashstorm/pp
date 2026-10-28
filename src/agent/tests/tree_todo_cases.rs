use super::*;

#[test]
fn todo_tree_navigation_restores_standalone_and_retains_current_plan_authority() {
    use crate::tools::todo::{TodoStatus, project_plan_todos};
    for protocol in ["completion", "response"] {
        for plan_bound in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            let (_, change) =
                crate::tools::todo::run(&[], r#"{"op":"init","items":["Historical standalone"]}"#);
            apply_todo_change(&mut agent.workflow, &mut agent.journal, change).unwrap();
            seed_session(&mut agent, "first history boundary");
            let target = agent.session_tree().unwrap().tree.nodes[0].id.clone();
            if plan_bound {
                let content = "## Live\n- [x] finished\n- [ ] waiting\n";
                agent.workflow = plan_workflow(content, ORIGIN, crate::plan::PlanStatus::Executed);
                agent.workflow.mode = crate::plan::WorkMode::Execute;
                let document = agent.workflow.plan.as_mut().unwrap();
                document.approved_tasks = Some(vec!["finished".into(), "waiting".into()]);
                agent.workflow.todo_plan_id = Some(document.id.clone());
                agent.workflow.todos = project_plan_todos(document, &[], false).unwrap();
                agent.workflow.todos[0].tasks[1].status = TodoStatus::Blocked;
                agent.workflow.todos[0].tasks[1].blocker = Some("dependency".into());
                write_plan_file(&agent, content);
                agent.reconcile_plan_location().unwrap();
                agent.journal.write_workflow(&agent.workflow).unwrap();
            } else {
                let (_, change) =
                    crate::tools::todo::run(&agent.workflow.todos, r#"{"op":"done"}"#);
                apply_todo_change(&mut agent.workflow, &mut agent.journal, change).unwrap();
            }
            seed_session(&mut agent, "second history boundary");
            let source = agent.workflow.todo_plan_id.clone();
            agent.navigate_tree(&target).unwrap();
            if plan_bound {
                assert_eq!(agent.workflow.todo_plan_id, source);
                assert_eq!(
                    agent.workflow.todos[0].tasks[0].status,
                    TodoStatus::Completed
                );
                assert_eq!(agent.workflow.todos[0].tasks[1].status, TodoStatus::Blocked);
                assert_eq!(
                    agent.workflow.todos[0].tasks[1].blocker.as_deref(),
                    Some("dependency")
                );
                assert_eq!(
                    agent
                        .tools
                        .read_plan(agent.workflow.plan.as_ref().unwrap())
                        .unwrap(),
                    "## Live\n- [x] finished\n- [ ] waiting\n"
                );
            } else {
                assert!(agent.workflow.todo_plan_id.is_none());
                assert_eq!(
                    agent.workflow.todos[0].tasks[0].content,
                    "Historical standalone"
                );
                assert_eq!(
                    agent.workflow.todos[0].tasks[0].status,
                    TodoStatus::InProgress
                );
            }
        }
    }
}

#[test]
fn plan_todo_committed_checkbox_survives_failed_journal_and_recovers_from_disk() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        let body = "## Work\n- [ ] first\n- [ ] second\n";
        agent.workflow = plan_workflow(body, ORIGIN, crate::plan::PlanStatus::Executed);
        agent.workflow.mode = crate::plan::WorkMode::Execute;
        write_plan_file(&agent, body);
        agent.ensure_task_plan_lease().unwrap();
        plan_sync::sync_plan_todos(
            &agent.tools,
            &mut agent.journal,
            &mut agent.workflow,
            &mut |_| {},
            false,
        )
        .unwrap();
        seed_session(&mut agent, "persist approved runtime");
        let id = agent.session_id().unwrap().to_owned();
        let trusted = agent.workflow.clone();
        let path = dir.path().join("sessions").join(format!("{id}.jsonl"));
        agent.journal.use_read_only_file(&path);
        let execution = agent.tools.execute(
            "todo",
            r#"{"op":"done","task":"first"}"#,
            "saved-progress",
            crate::tools::ToolPolicy::Execute {
                workflow: &agent.workflow,
            },
            &CancellationToken::new(),
            &mut |_| {},
        );
        assert_eq!(
            serde_json::from_str::<Value>(&execution.output).unwrap()["ok"],
            true
        );
        let error = apply_todo_change(
            &mut agent.workflow,
            &mut agent.journal,
            execution.todo_change,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("Plan progress saved"));
        assert_eq!(agent.workflow, trusted);
        assert_eq!(
            agent
                .tools
                .read_plan(agent.workflow.plan.as_ref().unwrap())
                .unwrap(),
            body.replacen("[ ]", "[x]", 1)
        );
        drop(agent);
        let restored = persistence::load(&dir.path().join("sessions"), Some(&id), true).unwrap();
        let template = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
        let recovered = Agent::restore(template.catalog, template.tools, restored).unwrap();
        assert!(recovered.replay.iter().any(|record| matches!(
            record,
            persistence::ReplayRecord::Event(AgentEvent::TodoSnapshot { phases, .. })
                if phases[0].tasks[0].status == crate::tools::todo::TodoStatus::Completed
        )));
        assert_eq!(
            recovered.workflow.todos[0].tasks[0].status,
            crate::tools::todo::TodoStatus::Completed
        );
        assert_eq!(
            recovered.workflow.todos[0].tasks[1].status,
            crate::tools::todo::TodoStatus::InProgress
        );
        assert_eq!(
            recovered.workflow.plan.as_ref().unwrap().revision,
            trusted.plan.as_ref().unwrap().revision + 1
        );
    }
}

#[test]
fn todo_failed_journal_commit_restores_trusted_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
    seed_session(&mut agent, "seed durable history");
    let (_, change) = crate::tools::todo::run(&[], r#"{"op":"init","items":["A","B"]}"#);
    apply_todo_change(&mut agent.workflow, &mut agent.journal, change).unwrap();
    let previous = agent.workflow.clone();
    let path = dir
        .path()
        .join("sessions")
        .join(format!("{}.jsonl", agent.session_id().unwrap()));
    agent.journal.use_read_only_file(&path);
    let (_, change) = crate::tools::todo::run(&agent.workflow.todos, r#"{"op":"done","task":"A"}"#);
    assert!(apply_todo_change(&mut agent.workflow, &mut agent.journal, change).is_err());
    assert_eq!(agent.workflow, previous);
    assert!(agent.journal.check().is_err());
}
