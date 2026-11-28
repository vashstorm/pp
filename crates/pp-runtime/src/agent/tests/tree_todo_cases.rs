mod integration {
    use super::super::*;
    use pp_test_support::TestServer;
    use std::sync::Arc;

    #[test]
    fn todo_tree_navigation_plan_authority_and_pipe_rebind() {
        use pp_core::todo::TodoStatus;
        use pp_tools::todo::project_plan_todos;
        for protocol in ["completion", "response"] {
            for plan_bound in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
                let (_, change) =
                    pp_tools::todo::run(&[], r#"{"op":"init","items":["Historical standalone"]}"#);
                apply_todo_change(&mut agent.workflow, &mut agent.journal, change).unwrap();
                seed_session(&mut agent, "first history boundary");
                let target = agent.session_tree().unwrap().tree.nodes[0].id.clone();
                if plan_bound {
                    let content = "## Live\n- [x] finished\n- [ ] waiting\n";
                    agent.workflow =
                        plan_workflow(content, ORIGIN, pp_core::plan::PlanStatus::Executed);
                    agent.workflow.mode = pp_core::plan::WorkMode::Execute;
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
                        pp_tools::todo::run(&agent.workflow.todos, r#"{"op":"done"}"#);
                    apply_todo_change(&mut agent.workflow, &mut agent.journal, change).unwrap();
                }
                seed_session(&mut agent, "second history boundary");
                let source = agent.workflow.todo_plan_id.clone();
                let before_navigation = agent.pipe_store();
                agent.navigate_tree(&target).unwrap();
                assert!(!before_navigation.same_instance(&agent.pipe_store()));
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
                let before_reset = agent.pipe_store();
                agent.reset();
                assert!(!before_reset.same_instance(&agent.pipe_store()));
            }
        }

        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            let (_, change) = pp_tools::todo::run(
                &[],
                r#"{"op":"init","items":["Historical execution task"]}"#,
            );
            apply_todo_change(&mut agent.workflow, &mut agent.journal, change).unwrap();
            seed_session(&mut agent, "execution history");
            let target = agent.session_tree().unwrap().tree.nodes[0].id.clone();
            agent.enter_plan(&CancellationToken::new()).unwrap();
            agent.navigate_tree(&target).unwrap();
            assert_eq!(agent.workflow.mode, pp_core::plan::WorkMode::Plan);
            assert!(agent.workflow.plan.is_none());
            assert!(agent.workflow.todos.is_empty());
            assert!(agent.workflow.todo_plan_id.is_none());
        }
    }

    #[test]
    fn todo_journal_failures_recovery_and_runtime_snapshots() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            let body = "## Work\n- [ ] first\n- [ ] second\n";
            agent.workflow = plan_workflow(body, ORIGIN, pp_core::plan::PlanStatus::Executed);
            agent.workflow.mode = pp_core::plan::WorkMode::Execute;
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
                pp_tools::ToolPolicy::Execute {
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
            let restored = persistence::load_test(
                &dir.path().join("sessions"),
                &dir.path().join("sessions/.locks"),
                Some(&id),
                true,
            )
            .unwrap();
            let mut template = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            let tools = std::mem::replace(
                &mut template.tools,
                Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
            );
            let recovered = Agent::restore(template.models(), tools, restored).unwrap();
            assert!(recovered.replay.iter().any(|record| matches!(
                record,
                persistence::ReplayRecord::Event(AgentEvent::TodoSnapshot { phases, .. })
                    if phases[0].tasks[0].status == pp_core::todo::TodoStatus::Completed
            )));
            assert_eq!(
                recovered.workflow.todos[0].tasks[0].status,
                pp_core::todo::TodoStatus::Completed
            );
            assert_eq!(
                recovered.workflow.todos[0].tasks[1].status,
                pp_core::todo::TodoStatus::InProgress
            );
            assert_eq!(
                recovered.workflow.plan.as_ref().unwrap().revision,
                trusted.plan.as_ref().unwrap().revision + 1
            );
        }

        {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
            seed_session(&mut agent, "seed durable history");
            let (_, change) = pp_tools::todo::run(&[], r#"{"op":"init","items":["A","B"]}"#);
            apply_todo_change(&mut agent.workflow, &mut agent.journal, change).unwrap();
            let previous = agent.workflow.clone();
            let path = dir
                .path()
                .join("sessions")
                .join(format!("{}.jsonl", agent.session_id().unwrap()));
            agent.journal.use_read_only_file(&path);
            let (_, change) =
                pp_tools::todo::run(&agent.workflow.todos, r#"{"op":"done","task":"A"}"#);
            assert!(apply_todo_change(&mut agent.workflow, &mut agent.journal, change).is_err());
            assert_eq!(agent.workflow, previous);
            assert!(agent.journal.check().is_err());
        }

        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let server = Arc::new(TestServer::start().unwrap());
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.addr().to_string(), "");
            let serving = protocol.to_owned();
            let worker = Arc::clone(&server);
            let handle = std::thread::spawn(move || {
                for args in [
                    json!({"op":"init","items":["A","B"]}),
                    json!({"op":"done","task":"missing"}),
                    json!({"op":"done"}),
                    json!({"op":"init","list":[]}),
                ] {
                    let request = worker
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap()
                        .unwrap();
                    let _: Value = request.json().unwrap();
                    tool_call_for(&serving, request, "todo", &args.to_string());
                }
                let request = worker
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                final_for(&serving, request, "All clear", true);
            });
            let mut snapshots = Vec::new();
            agent
                .run(
                    "Exercise committed todos",
                    &CancellationToken::new(),
                    None,
                    &mut |event| {
                        if let AgentEvent::TodoStateUpdated { state } = event {
                            let journal = std::fs::read_dir(dir.path().join("sessions"))
                                .unwrap()
                                .map(|entry| entry.unwrap().path())
                                .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                                .unwrap();
                            let text = crate::agent::persistence::tests::read_log(&journal);
                            let saved: Value = text
                                .lines()
                                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                                .rfind(|entry| entry["customType"] == "pp.workflow")
                                .unwrap();
                            let persisted = saved["data"]["workflow"]
                                .get("todos")
                                .cloned()
                                .unwrap_or_else(|| json!([]));
                            assert_eq!(persisted, serde_json::to_value(&state.phases).unwrap());
                            assert!(!text.contains("todo_state_updated"));
                            snapshots.push(state);
                        }
                    },
                )
                .unwrap();
            handle.join().unwrap();
            // Failed operations publish no state; clearing must publish an empty snapshot.
            assert_eq!(snapshots.len(), 3);
            assert_eq!(
                snapshots[0].phases[0].tasks[0].status,
                pp_core::todo::TodoStatus::InProgress
            );
            assert!(
                snapshots[1].phases[0]
                    .tasks
                    .iter()
                    .all(|task| task.status == pp_core::todo::TodoStatus::Completed)
            );
            assert!(snapshots[2].phases.is_empty());
        }
    }
}
