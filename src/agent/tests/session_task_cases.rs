use super::*;

#[test]
fn instruction_loading_cards_failed_requests_record_frozen_body_once() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("AGENTS.md");
        std::fs::write(&path, "loaded-before-network-result").unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        for (expected, next, expected_loads) in [
            (
                "loaded-before-network-result",
                "changed-after-request-open",
                1,
            ),
            (
                "changed-after-request-open",
                "changed-after-request-open",
                1,
            ),
            (
                "changed-after-request-open",
                "changed-after-request-open",
                0,
            ),
        ] {
            let mut events = Vec::new();
            std::thread::scope(|scope| {
                scope.spawn(|| {
                    let mut request = server
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap()
                        .expect("actual provider request");
                    let mut body = String::new();
                    request.as_reader().read_to_string(&mut body).unwrap();
                    let body: Value = serde_json::from_str(&body).unwrap();
                    let system = if protocol == "response" {
                        body["instructions"].as_str().unwrap()
                    } else {
                        body["messages"][0]["content"].as_str().unwrap()
                    };
                    assert!(system.contains(expected));
                    // 首次建连结果返回前改写磁盘；卡片必须仍使用已发送的快照。
                    std::fs::write(&path, next).unwrap();
                    request.respond(tiny_http::Response::empty(500)).unwrap();
                });
                assert!(
                    agent
                        .run(
                            "actual request",
                            &CancellationToken::new(),
                            None,
                            &mut |event| { events.push(event) }
                        )
                        .is_err()
                );
            });
            let loads: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    AgentEvent::ProjectInstructionsLoaded { content, .. } => Some(content.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(loads, vec![expected; expected_loads]);
        }
        let id = agent.session_id().unwrap().to_owned();
        drop(agent);
        let restored = persistence::load(
            &dir.path().join("sessions"),
            &dir.path().join("sessions/.locks"),
            Some(&id),
            true,
        )
        .unwrap();
        let loads: Vec<_> = restored
            .replay
            .iter()
            .filter_map(|record| match record {
                persistence::ReplayRecord::Event(AgentEvent::ProjectInstructionsLoaded {
                    content,
                    ..
                }) => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            loads,
            ["loaded-before-network-result", "changed-after-request-open"]
        );
    }
}

#[test]
fn resume_preparation_preserves_current_session_and_releases_failed_candidate() {
    const FIXTURE: &str = "PP_TEST_RESUME_PREPARATION";
    let Ok(root) = std::env::var(FIXTURE) else {
        let home = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "agent::tests::session_task_cases::resume_preparation_preserves_current_session_and_releases_failed_candidate", "--nocapture"])
            .env("HOME", home.path())
            .env(FIXTURE, home.path())
            .status().unwrap();
        assert!(status.success());
        return;
    };
    let root = std::path::Path::new(&root);
    let global = root.join(".pp/agent");
    let sessions = global.join("sessions");
    std::fs::create_dir_all(&global).unwrap();
    let models = global.join("models.yml");
    let settings = global.join("settings.yml");
    let configuration =
        json!({"providers":{"fixture": {"api_key":"fixture","models":{"one": {"model":"one"}}}}})
            .to_string();
    std::fs::write(&models, &configuration).unwrap();
    let mut ids = Vec::new();
    let mut agents = Vec::new();
    for name in ["old", "saved"] {
        let cwd = root.join(name);
        std::fs::create_dir(&cwd).unwrap();
        let mut agent = Agent::new(
            ModelCatalog::from_files(&settings, &models).unwrap(),
            Tools::new(cwd, EditMode::Hashline).unwrap(),
            sessions.clone(),
        )
        .unwrap();
        let Session::Response(request) = &mut agent.session else {
            panic!()
        };
        response_input(request).push(InputItem::EasyMessage(EasyInputMessage {
            role: Role::User,
            content: name.into(),
            ..Default::default()
        }));
        let metadata = Metadata {
            cwd: agent.tools.cwd(),
            model_reference: &agent.current_model.reference,
            think_level: agent.think_level,
            edit_mode: agent.tools.edit_mode(),
            tool_limits: agent.tools.limits,
            base_system: &agent.base_system,
            prompt_source: agent.prompt_source.as_ref(),
        };
        agent
            .journal
            .sent(
                &metadata,
                (&agent.session).into(),
                &agent.workflow,
                Some(name),
                None,
            )
            .unwrap();
        ids.push(agent.session_id().unwrap().to_owned());
        agents.push(agent);
    }
    let saved = agents.pop().unwrap();
    let expected = serde_json::to_value(&saved.session).unwrap();
    drop(saved);
    let mut old = agents.pop().unwrap();
    let before = serde_json::to_value(&old.session).unwrap();
    let cwd = std::env::current_dir().unwrap();
    assert!(old.prepare_resume(&ids[0]).is_err());
    // 运行中的恢复使用当前配置，不重新读取磁盘上的模型文件。
    std::fs::write(&models, "invalid configuration").unwrap();
    let candidate = old.prepare_resume(&ids[1]).unwrap();
    let mut expected = expected;
    expected["request"]["instructions"] = before["request"]["instructions"].clone();
    assert_eq!(candidate.journal.id(), Some(ids[1].as_str()));
    assert_eq!(
        serde_json::to_value(&candidate.checkpoint.session).unwrap(),
        expected
    );
    assert_eq!(old.session_id(), Some(ids[0].as_str()));
    assert_eq!(serde_json::to_value(&old.session).unwrap(), before);
    assert_eq!(std::env::current_dir().unwrap(), cwd);
    assert!(persistence::load(&sessions, &sessions.join(".locks"), Some(&ids[0]), true).is_err());
    assert!(persistence::load(&sessions, &sessions.join(".locks"), Some(&ids[1]), true).is_err());
    drop(candidate);
    assert!(persistence::load(&sessions, &sessions.join(".locks"), Some(&ids[1]), true).is_ok());
    old.resume(&ids[1], &CancellationToken::new()).unwrap();
    assert_eq!(old.session_id(), Some(ids[1].as_str()));
    assert!(persistence::load(&sessions, &sessions.join(".locks"), Some(&ids[0]), true).is_ok());
    assert!(persistence::load(&sessions, &sessions.join(".locks"), Some(&ids[1]), true).is_err());
}

#[test]
fn cancellation_before_request_saves_accepted_input_and_replays_terminal() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.yml");
        let models = dir.path().join("models.yml");
        let sessions = dir.path().join("sessions");
        std::fs::write(
            &models,
            json!({"providers":{"fixture": { "api_key":"fixture-key", "protocol":protocol,
                "base_url":"http://127.0.0.1:1/v1", "models":{"one": {"model":"one"}}
            }}})
            .to_string(),
        )
        .unwrap();
        let mut agent = Agent::new(
            ModelCatalog::from_files(&settings, &models).unwrap(),
            Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
            sessions.clone(),
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let outcome = agent
            .run("accepted input", &cancellation, None, &mut |event| {
                assert!(
                    !matches!(
                        event,
                        AgentEvent::AssistantStarted | AgentEvent::ToolStarted { .. }
                    ),
                    "no request or tool may start"
                );
            })
            .unwrap();
        assert_eq!(outcome, RunOutcome::Cancelled);
        let id = agent.session_id().unwrap().to_owned();
        drop(agent);
        let restored =
            persistence::load(&sessions, &sessions.join(".locks"), Some(&id), true).unwrap();
        assert!(
            matches!(restored.replay.as_slice(), [persistence::ReplayRecord::User(text), persistence::ReplayRecord::Cancelled] if text == "accepted input")
        );
        let native = serde_json::to_value(&restored.checkpoint.session).unwrap();
        let entries = if protocol == "completion" {
            native["request"]["messages"].as_array().unwrap()
        } else {
            native["request"]["input"].as_array().unwrap()
        };
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry["role"] == "user")
                .count(),
            1
        );
        assert!(!persistence::indexed_listing(&sessions).unwrap().sessions[0].done);
    }
}

#[test]
fn background_finish_run_reclaims_processes_for_every_outcome() {
    let dir = tempfile::tempdir().unwrap();
    // 不发真实请求，仅借共享 fixture 构造 Agent；地址占位即可。
    let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
    let mut previous = None;
    for outcome in [0, 1, 2] {
        let token = CancellationToken::new();
        let started = agent.tools.execute("bash", &json!({"command":"printf '%s\\n%s' \"$$\" \"$TMPDIR\" > marker.pending; mv marker.pending marker; exec sleep 60","mode":"background"}).to_string(), "start", crate::tools::ToolPolicy::Execute { workflow: &agent.workflow }, &token, &mut |_| {});
        let snapshot: serde_json::Value = serde_json::from_str(&started.output).unwrap();
        let id = snapshot["task_id"].as_str().unwrap().to_owned();
        assert_ne!(previous.as_ref(), Some(&id));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !dir.path().join("marker").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let marker = std::fs::read_to_string(dir.path().join("marker")).unwrap();
        let (pid, scratch) = marker.split_once('\n').unwrap();
        let pid = rustix::process::Pid::from_raw(pid.parse().unwrap()).unwrap();
        let result = match outcome {
            0 => Ok(RunOutcome::Completed),
            1 => Err(anyhow::anyhow!("provider failed")),
            _ => {
                token.cancel();
                Err(Cancelled.into())
            }
        };
        let finished = agent.finish_run(result, &token);
        match outcome {
            0 => assert_eq!(finished.unwrap(), RunOutcome::Completed),
            1 => assert!(
                finished
                    .unwrap_err()
                    .to_string()
                    .contains("provider failed")
            ),
            _ => assert_eq!(finished.unwrap(), RunOutcome::Cancelled),
        }
        assert_eq!(
            rustix::process::test_kill_process(pid),
            Err(rustix::io::Errno::SRCH)
        );
        assert!(!std::path::Path::new(scratch).exists());
        let old = agent.tools.execute(
            "bash_task",
            &json!({"task_id":id,"action":"status"}).to_string(),
            "status",
            crate::tools::ToolPolicy::Execute {
                workflow: &agent.workflow,
            },
            &CancellationToken::new(),
            &mut |_| {},
        );
        assert!(old.output.contains("Unknown background task"));
        previous = Some(id);
        std::fs::remove_file(dir.path().join("marker")).unwrap();
    }
}

#[test]
fn finish_task_archives_completed_plan_only_on_success_boundary() {
    use crate::plan::{PlanLocation, PlanStatus, plan_tasks};
    use crate::tools::plan_catalog::{self, PlanCatalogEntry, PlanCatalogLock};

    let archive_agent = |dir: &std::path::Path, protocol: &str| -> Agent {
        let mut agent = runtime_input_agent(dir, protocol, "127.0.0.1:1", "");
        let mut workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
        workflow.mode = crate::plan::WorkMode::Execute;
        {
            let document = workflow.plan.as_mut().expect("plan bound");
            document.file_name = Some("create-canary.md".into());
            document.approved_tasks = Some(
                plan_tasks(PLAN_CONTENT)
                    .into_iter()
                    .map(|task| task.text)
                    .collect(),
            );
        }
        agent.workflow = workflow;
        let workspace = agent.tools.plan_workspace();
        let _lock = PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = plan_catalog::load_catalog(workspace).unwrap();
        let mut catalog = snapshot.catalog;
        let id = agent.workflow.plan.as_ref().unwrap().id.clone();
        catalog.entries.push(PlanCatalogEntry {
            id,
            file_name: "create-canary.md".into(),
            location: PlanLocation::Plans,
            deleted: false,
            deleted_file_identity: None,
        });
        plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
        drop(_lock);
        seed_session(&mut agent, EXPLORATION);
        write_plan_file(&agent, &PLAN_CONTENT.replace("- [ ]", "- [x]"));
        agent
    };

    for protocol in ["completion", "response"] {
        // Completed 且未取消：归档到 done。
        let dir = tempfile::tempdir().unwrap();
        let mut agent = archive_agent(dir.path(), protocol);
        let cancellation = CancellationToken::new();
        let result = agent.finish_task(
            "test",
            std::time::Instant::now(),
            Ok(RunOutcome::Completed),
            &cancellation,
            None,
            &mut |_| {},
        );
        assert_eq!(result.unwrap(), RunOutcome::Completed);
        assert!(!dir.path().join(".pp/plans/create-canary.md").exists());
        assert!(dir.path().join(".pp/done/create-canary.md").exists());
        let document = agent.workflow.plan.as_ref().unwrap();
        assert_eq!(document.status, PlanStatus::Completed);
        assert_eq!(document.location, PlanLocation::Done);

        // Completed 但取消已置位：进度刷新保留，归档不发生。
        let dir = tempfile::tempdir().unwrap();
        let mut agent = archive_agent(dir.path(), protocol);
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let result = agent.finish_task(
            "test",
            std::time::Instant::now(),
            Ok(RunOutcome::Completed),
            &cancellation,
            None,
            &mut |_| {},
        );
        assert_eq!(result.unwrap(), RunOutcome::Completed);
        assert!(dir.path().join(".pp/plans/create-canary.md").exists());
        assert!(!dir.path().join(".pp/done/create-canary.md").exists());
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().status,
            PlanStatus::Executed
        );

        // 任务失败：不归档。
        let dir = tempfile::tempdir().unwrap();
        let mut agent = archive_agent(dir.path(), protocol);
        let result = agent.finish_task(
            "test",
            std::time::Instant::now(),
            Err(anyhow::anyhow!("provider failed")),
            &CancellationToken::new(),
            None,
            &mut |_| {},
        );
        assert!(result.unwrap_err().to_string().contains("provider failed"));
        assert!(dir.path().join(".pp/plans/create-canary.md").exists());
        assert!(!dir.path().join(".pp/done/create-canary.md").exists());

        // AwaitingPlanReview：不归档。
        let dir = tempfile::tempdir().unwrap();
        let mut agent = archive_agent(dir.path(), protocol);
        let result = agent.finish_task(
            "test",
            std::time::Instant::now(),
            Ok(RunOutcome::AwaitingPlanReview),
            &CancellationToken::new(),
            None,
            &mut |_| {},
        );
        assert_eq!(result.unwrap(), RunOutcome::AwaitingPlanReview);
        assert!(dir.path().join(".pp/plans/create-canary.md").exists());
        assert!(!dir.path().join(".pp/done/create-canary.md").exists());
    }
}

#[test]
fn system_prompt_render_failure_preserves_native_session_and_journal() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.yml");
        let models = dir.path().join("models.yml");
        std::fs::write(&settings, "default_model: fixture/one\n").unwrap();
        std::fs::write(
            &models,
            json!({"providers":{"fixture":{
                "protocol":protocol, "api_key":"fixture-key",
                "models":{"one":{"model":"one"},"two":{"model":"two"}}
            }}})
            .to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("base.md"), "{{#if (eq model \"two\")}}{{missing}}{{/if}}{{#if (eq work_mode \"plan\")}}{{missing}}{{/if}}custom {{model}}").unwrap();
        let source = crate::system_prompt::Snapshot::discover(
            dir.path(),
            dir.path(),
            None,
            Some("base.md"),
            None,
        )
        .unwrap();
        let mut agent = Agent::new_with_prompt(
            ModelCatalog::from_files(&settings, &models).unwrap(),
            Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
            dir.path().join("sessions"),
            dir.path().join("sessions/.locks"),
            source,
        )
        .unwrap();
        seed_session(&mut agent, "preserve this input");
        let native = serde_json::to_value(&agent.session).unwrap();
        let path = dir
            .path()
            .join("sessions")
            .join(format!("{}.jsonl", agent.session_id().unwrap()));
        let journal = std::fs::read(&path).unwrap();
        assert!(
            format!("{:#}", agent.switch_model("fixture/two").unwrap_err()).contains("base.md")
        );
        assert_eq!(agent.current_model.reference, "fixture/one");
        assert_eq!(serde_json::to_value(&agent.session).unwrap(), native);
        assert_eq!(std::fs::read(&path).unwrap(), journal);
        agent.workflow.mode = crate::plan::WorkMode::Plan;
        assert!(agent.refresh_instructions().is_err());
        assert_eq!(serde_json::to_value(&agent.session).unwrap(), native);
        assert_eq!(std::fs::read(&path).unwrap(), journal);
    }
}

#[test]
fn model_switch_failure_keeps_native_history_and_selection() {
    let dir = tempfile::tempdir().unwrap();
    let settings = dir.path().join("settings.yml");
    let models = dir.path().join("models.yml");
    std::fs::write(
        &models,
        json!({"providers": {
            "a": { "api_key":"fixture-key", "models":{
                "one": {"model":"one"}, "two": {"model":"two"}}},
            "bad": { "api_key":"secret-marker\ninvalid", "models":{
                "one": {"model":"bad"}}}}})
        .to_string(),
    )
    .unwrap();
    let catalog = ModelCatalog::from_files(&settings, &models).unwrap();
    let mut agent = Agent::new(
        catalog,
        Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
        dir.path().join("sessions"),
    )
    .unwrap();
    let Session::Response(request) = &mut agent.session else {
        panic!()
    };
    response_input(request).push(InputItem::EasyMessage(EasyInputMessage {
        role: Role::User,
        content: "remember me".into(),
        ..Default::default()
    }));
    let before = serde_json::to_value(&request).unwrap();
    for reference in ["bad/one", "missing/secret-marker", "secret-marker"] {
        let error = agent.switch_model(reference).unwrap_err().to_string();
        assert!(!error.contains("secret-marker"));
        assert_eq!(agent.current_model().reference, "a/one");
        let Session::Response(request) = &agent.session else {
            panic!()
        };
        assert_eq!(serde_json::to_value(request).unwrap(), before);
    }
    agent.switch_model("a/one").unwrap();
    let Session::Response(request) = &agent.session else {
        panic!()
    };
    assert_eq!(serde_json::to_value(request).unwrap(), before);
    agent.switch_model("a/two").unwrap();
    let Session::Response(request) = &agent.session else {
        panic!()
    };
    let mut expected = before;
    expected["model"] = json!("two");
    assert_eq!(serde_json::to_value(request).unwrap(), expected);
    agent.reset();
    assert_eq!(agent.current_model().reference, "a/two");
    let Session::Response(request) = &agent.session else {
        panic!()
    };
    assert!(matches!(&request.input, InputParam::Items(items) if items.is_empty()));
    assert_eq!(
        serde_json::to_value(request).unwrap()["instructions"],
        expected["instructions"]
    );
}

#[test]
fn bang_shell_agent_is_local_and_plan_is_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:9", "");
    let before = serde_json::to_value(&agent.session).unwrap();
    let mut events = Vec::new();
    agent
        .run_shell(
            "printf local; exit 7",
            "local",
            &CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .unwrap();
    assert!(events.iter().any(
        |event| matches!(event, AgentEvent::ToolFinished { result, .. } if result["exit_code"] == 7)
    ));
    assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
    assert!(!dir.path().join("sessions").exists());
    agent.workflow.mode = crate::plan::WorkMode::Plan;
    let error = agent
        .run_shell(
            "touch forbidden",
            "local",
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "Shell commands are unavailable in Plan mode."
    );
    assert!(!dir.path().join("forbidden").exists());
}
