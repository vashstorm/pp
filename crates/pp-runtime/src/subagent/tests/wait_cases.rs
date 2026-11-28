fn wait_fixture_run(id: &str, status: &str) -> Run {
    Run {
        id: id.into(),
        agent_id: format!("agent-{id}"),
        owner: "Main".into(),
        status: status.into(),
        task: format!("work {id}"),
        result: (!matches!(status, "queued" | "running"))
            .then(|| json!({"ok":true,"output":format!("answer-{id}")})),
        consumed: false,
        duration_ms: 0,
        started_ms: now(),
        requests: 0,
        tokens: 0,
        cost: None,
    }
}

fn unified_wait(tools: &pp_tools::Tools, args: Value) -> Value {
    let result = tools.execute(
        "wait",
        &args.to_string(),
        "wait",
        pp_tools::ToolPolicy::Execute {
            workflow: &Default::default(),
        },
        &Default::default(),
        &mut |_| {},
    );
    serde_json::from_str(&result.output).unwrap()
}

#[test]
fn wait_subagent_reports_status_without_consuming_results_and_rejects_other_owners() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let mut tools =
        pp_tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    tools.subagent = Some(Arc::new(binding.clone()));
    let mut foreign = wait_fixture_run("foreign", "completed");
    foreign.owner = "other".into();
    {
        let mut state = lock(&binding.inner.state);
        state
            .disk
            .runs
            .insert("r1".into(), wait_fixture_run("r1", "completed"));
        state.disk.runs.insert("foreign".into(), foreign);
    }
    assert_eq!(
        unified_wait(&tools, json!({"run_ids":["r1","foreign"]}))["ok"],
        false
    );
    assert_eq!(binding.deliveries(&[]).len(), 1);
    let first = unified_wait(&tools, json!({"run_ids":["r1","r1"]}));
    assert_eq!(first["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(first["jobs"][0]["status"], "completed");
    assert_eq!(first["jobs"][0]["delivered"], false);
    assert!(!first.to_string().contains("answer-r1"), "{first}");
    tools.commit_wait_results().unwrap();
    assert_eq!(binding.deliveries(&[]).len(), 1);
    // A failed registry persist must not commit the in-memory consumed flag early.
    let path = binding.directory().join("registry.json");
    fs::create_dir_all(binding.directory()).unwrap();
    if path.exists() {
        fs::remove_file(&path).unwrap();
    }
    fs::create_dir(&path).unwrap();
    assert!(binding.acknowledge(&["r1".into()]).is_err());
    assert_eq!(binding.deliveries(&[]).len(), 1);
    fs::remove_dir(&path).unwrap();
    binding.acknowledge(&["r1".into()]).unwrap();
    assert!(binding.deliveries(&[]).is_empty());
    assert_eq!(unified_wait(&tools, json!({}))["jobs"], json!([]));
    assert_eq!(
        unified_wait(&tools, json!({"run_ids":["r1"]}))["jobs"][0]["delivered"],
        true
    );
    assert!(!lock(&binding.inner.state).disk.runs["foreign"].consumed);
}

#[test]
fn wait_wrong_id_kinds_explain_selectors_without_reserving_results() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let mut tools =
        pp_tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    tools.subagent = Some(Arc::new(binding.clone()));
    let mut run = wait_fixture_run("r1", "completed");
    run.agent_id = "a1".into();
    lock(&binding.inner.state)
        .disk
        .runs
        .insert("r1".into(), run);
    for args in [
        json!({"task_ids":["a1"]}),
        json!({"task_ids":["r1"]}),
        json!({"run_ids":["a1"]}),
        json!({"run_ids":["agent://a1"]}),
        json!({"run_ids":["bg-1"]}),
        json!({"run_ids":["r1"],"task_ids":["a1"]}),
    ] {
        let result = unified_wait(&tools, args);
        assert_eq!(result["ok"], false, "{result}");
        let error = result["error"].as_str().unwrap();
        for hint in [
            "wait_args",
            "run_id",
            "run_ids",
            "task_id",
            "task_ids",
            "agent_id",
        ] {
            assert!(error.contains(hint), "missing ID usage guidance {hint}: {error}");
        }
        assert_eq!(binding.deliveries(&[]).len(), 1);
    }
    let corrected = unified_wait(&tools, json!({"run_ids":["r1"]}));
    assert_eq!(corrected["ok"], true, "{corrected}");
    assert_eq!(corrected["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(corrected["jobs"][0]["id"], "r1");
    assert_eq!(binding.deliveries(&[]).len(), 1);
}

#[test]
fn wait_mixed_targets_wait_for_child_and_background_bash() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let mut tools =
        pp_tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    tools.subagent = Some(Arc::new(binding.clone()));
    lock(&binding.inner.state)
        .disk
        .runs
        .insert("r1".into(), wait_fixture_run("r1", "running"));
    let workflow = pp_core::plan::WorkflowState::default();
    let policy = || pp_tools::ToolPolicy::Execute {
        workflow: &workflow,
    };
    tools.execute(
        "bash",
        r#"{"command":"while [ ! -f release ]; do sleep 0.01; done; printf mixed-bash","mode":"background"}"#,
        "bg",
        policy(),
        &Default::default(),
        &mut |_| {},
    );
    let mut updates = 0;
    let result = tools.execute(
        "wait",
        r#"{"timeout_ms":5000}"#,
        "wait",
        policy(),
        &Default::default(),
        &mut |event| {
            if let AgentEvent::ToolOutputSnapshot { output, .. } = event {
                updates += 1;
                let snapshot: Value = serde_json::from_str(&output).unwrap();
                assert_eq!(snapshot["jobs"].as_array().unwrap().len(), 2);
                if updates > 1 {
                    assert!(
                        snapshot["jobs"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .any(|job| job["id"] == "r1" && job["status"] == "completed")
                    );
                    fs::write(dir.path().join("release"), "").unwrap();
                    return;
                }
                binding
                    .settle(
                        "r1",
                        json!({"ok":true,"output":"child-mixed-result"}),
                        20,
                        None,
                    )
                    .unwrap();
                // A newly appearing Run is not part of the target set fixed by this wait.
                lock(&binding.inner.state)
                    .disk
                    .runs
                    .insert("r2".into(), wait_fixture_run("r2", "completed"));
            }
        },
    );
    assert!(updates >= 2);
    let result: Value = serde_json::from_str(&result.output).unwrap();
    assert_eq!(result["pending"], false);
    assert_eq!(result["jobs"].as_array().unwrap().len(), 2);
    assert_eq!(result["jobs"][0]["id"], "r1");
    assert!(!result.to_string().contains("child-mixed-result"), "{result}");
    assert_eq!(result["background_results"][0]["output"], "mixed-bash");
    let only = unified_wait(&tools, json!({"run_ids":["r2"]}));
    assert_eq!(only["pending"], false);
    assert_eq!(only["jobs"].as_array().unwrap().len(), 1);
    let plan = tools.all_definitions(WorkMode::Plan);
    assert_eq!(plan.iter().filter(|d| d.name == "wait").count(), 1);
}

#[test]
fn wait_timeout_cancels_only_selected_runs_and_their_descendant_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let mut tools =
        pp_tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    tools.subagent = Some(Arc::new(binding.clone()));
    let selected = CancellationToken::new();
    let descendant = selected.child_token();
    let unselected = CancellationToken::new();
    {
        let mut state = lock(&binding.inner.state);
        state
            .disk
            .runs
            .insert("r1".into(), wait_fixture_run("r1", "completed"));
        for (id, token) in [("r2", &selected), ("r3", &unselected)] {
            let run = wait_fixture_run(id, "running");
            let (sender, _) = mpsc::channel();
            state.live.insert(
                run.agent_id.clone(),
                Live {
                    sender,
                    input: InputQueue::default(),
                    cancellation: token.clone(),
                    generation: 1,
                },
            );
            state.disk.runs.insert(id.into(), run);
        }
    }
    let worker_binding = binding.clone();
    let worker_token = selected.clone();
    let worker = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker_token.is_cancelled() {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        worker_binding
            .settle(
                "r2",
                json!({"ok":false,"cancelled":true,"partial_output":"selected-partial"}),
                15,
                None,
            )
            .unwrap();
    });
    let result = tools
        .run_wait_with_elapsed(
            r#"{"run_ids":["r1","r2"]}"#,
            "timeout",
            &Default::default(),
            &mut |_| panic!("a timeout must enter the reaping path"),
            || pp_tools::wait::TIMEOUT,
        )
        .unwrap();
    worker.join().unwrap();
    assert_eq!(result["timed_out"], true);
    assert_eq!(result["ok"], false);
    assert_eq!(result["pending"], false);
    assert_eq!(result["jobs"].as_array().unwrap().len(), 2);
    assert!(!result.to_string().contains("selected-partial"), "{result}");
    assert!(descendant.is_cancelled());
    assert!(!unselected.is_cancelled());
    assert_eq!(
        binding.wait_jobs(&["r3".into()]).unwrap()[0].status,
        "running"
    );
    // Completed cancellation is not successful consumption; results can still be delivered again before the native history commit.
    assert_eq!(binding.deliveries(&["r2".into()]).len(), 1);
    tools.commit_wait_results().unwrap();
    assert_eq!(binding.deliveries(&["r1".into(), "r2".into()]).len(), 2);
}

#[test]
fn wait_cleanup_failure_is_a_stop_error_and_does_not_reserve_results() {
    let dir = tempfile::tempdir().unwrap();
    let binding = binding(dir.path(), TaskSettings::default());
    let mut tools =
        pp_tools::Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    tools.subagent = Some(Arc::new(binding.clone()));
    lock(&binding.inner.cleanup_errors).push(
        pp_core::cancellation::CleanupError(anyhow::anyhow!("fixture cleanup failure")).into(),
    );
    let workflow = Default::default();
    let token = CancellationToken::new();
    let start = tools.execute(
        "bash",
        r#"{"command":"sleep 30","mode":"background"}"#,
        "start",
        pp_tools::ToolPolicy::Execute {
            workflow: &workflow,
        },
        &token,
        &mut |_| {},
    );
    assert!(start.stop_error.is_none());
    let result = tools.execute(
        "wait",
        "{}",
        "wait",
        pp_tools::ToolPolicy::Execute {
            workflow: &workflow,
        },
        &token,
        &mut |event| {
            if matches!(event, AgentEvent::ToolOutputSnapshot { .. }) {
                token.cancel();
            }
        },
    );
    assert!(
        result
            .stop_error
            .unwrap()
            .is::<pp_core::cancellation::CleanupError>()
    );
    tools.start_wait_batch();
    let result = unified_wait(&tools, json!({}));
    assert_eq!(result["background_results"].as_array().unwrap().len(), 1);
}
