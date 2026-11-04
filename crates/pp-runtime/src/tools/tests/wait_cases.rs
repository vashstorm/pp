use super::*;

fn wait_events(
    tools: &Tools,
    args: Value,
    token: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Value {
    let workflow = WorkflowState::default();
    let result = tools.execute(
        "wait",
        &args.to_string(),
        "wait-test",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        token,
        emit,
    );
    assert!(result.stop_error.is_none(), "{:?}", result.stop_error);
    serde_json::from_str(&result.output).unwrap()
}

#[test]
fn wait_background_ignores_legacy_timeout_and_delivers_only_after_commit() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    assert_eq!(execute(&tools, "wait", json!({}))["jobs"], json!([]));
    let held = execute(
        &tools,
        "bash",
        json!({"command":"while [ ! -f release ]; do sleep 0.01; done; printf 'first-result'", "mode":"background"}),
    );
    let id = held["task_id"].as_str().unwrap();
    let mut updates = 0;
    let done = wait_events(
        &tools,
        json!({"task_ids":[id],"timeout_ms":0}),
        &Default::default(),
        &mut |event| {
            if let AgentEvent::ToolOutputSnapshot { output, .. } = event {
                let snapshot: Value = serde_json::from_str(&output).unwrap();
                assert_eq!(snapshot["pending"], true);
                assert_eq!(snapshot["background_results"], json!([]));
                updates += 1;
                fs::write(dir.path().join("release"), "").unwrap();
            }
        },
    );
    assert!(updates > 0);
    assert_eq!(done["ok"], true, "{done}");
    assert_eq!(done["pending"], false);
    assert_eq!(done["timed_out"], false);
    assert_eq!(done["background_results"][0]["output"], "first-result");
    let repeated = execute(&tools, "wait", json!({"task_ids":[id]}));
    assert_eq!(repeated["background_results"], json!([]));
    assert_eq!(repeated["jobs"][0]["delivered"], true);
    tools.start_wait_batch();
    assert_eq!(
        execute(&tools, "wait", json!({}))["background_results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    tools.commit_wait_results().unwrap();
    tools.start_wait_batch();
    assert_eq!(execute(&tools, "wait", json!({}))["jobs"], json!([]));
    assert_eq!(
        execute(&tools, "bash_task", json!({"task_id":id,"action":"status"}))["output"],
        "first-result"
    );
    tools.stop_background_tasks().unwrap();
    assert_eq!(
        execute(&tools, "wait", json!({"task_ids":[id]}))["ok"],
        false
    );
}

#[test]
fn wait_background_collects_failures_only_after_every_sibling_settles() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let slow = execute(
        &tools,
        "bash",
        json!({"command":"while [ ! -f release ]; do sleep 0.01; done; printf sibling-result", "mode":"background"}),
    );
    let failed = execute(
        &tools,
        "bash",
        json!({"command":"printf 'failure-output'; exit 7", "mode":"background"}),
    );
    let failed_id = failed["task_id"].as_str().unwrap();
    background_wait(&tools, failed_id, |r| r["status"] == "failed");
    // 未知 ID 不能吞掉同次指定的有效结果。
    assert_eq!(
        execute(&tools, "wait", json!({"task_ids":[failed_id,"missing"]}))["ok"],
        false
    );
    let mut observed_partial = false;
    let result = wait_events(&tools, json!({}), &Default::default(), &mut |event| {
        if let AgentEvent::ToolOutputSnapshot { output, .. } = event {
            let snapshot: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(snapshot["pending"], true);
            assert!(
                snapshot["jobs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|job| job["status"] == "failed")
            );
            assert_eq!(snapshot["background_results"], json!([]));
            observed_partial = true;
            fs::write(dir.path().join("release"), "").unwrap();
        }
    });
    assert!(observed_partial);
    assert_eq!(result["ok"], true);
    assert_eq!(result["pending"], false);
    let results = result["background_results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert!(
        results
            .iter()
            .any(|r| r["exit_code"] == 7 && r["output"] == "failure-output")
    );
    assert!(
        results
            .iter()
            .any(|r| r["task_id"] == slow["task_id"] && r["output"] == "sibling-result")
    );
}

#[test]
fn wait_validates_arguments_and_remains_available_without_subagents() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let definitions = tools.all_definitions(WorkMode::Execute);
    assert_eq!(definitions.iter().filter(|d| d.name == "wait").count(), 1);
    for args in [
        json!({"timeout_ms":-1}),
        json!({"timeout_ms":null}),
        json!({"timeout_ms":"1"}),
        json!({"task_ids":[" "]}),
        json!({"run_ids":["r1"]}),
        json!({"extra":true}),
    ] {
        assert_eq!(execute(&tools, "wait", args.clone())["ok"], false, "{args}");
    }
    assert_eq!(
        crate::tools::approval::tier_of("wait"),
        crate::tools::approval::Tier::Read
    );
    assert_eq!(execute(&tools, "wait", json!({}))["ok"], true);
    let result = tools.execute(
        "wait",
        "{malformed",
        "review",
        ToolPolicy::Review,
        &Default::default(),
        &mut |_| {},
    );
    assert!(result.output.contains("not available during Plan review"));
}

#[test]
fn wait_live_cancellation_and_multiple_settled_targets_preserve_results() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let started = execute(
        &tools,
        "bash",
        json!({"command":"printf 'retained'; while [ ! -f release ]; do sleep 0.01; done","mode":"background"}),
    );
    let token = CancellationToken::new();
    let workflow = WorkflowState::default();
    let result = tools.execute(
        "wait",
        "{}",
        "live-cancel",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        &token,
        &mut |event| {
            if matches!(event, AgentEvent::ToolOutputSnapshot { .. }) {
                token.cancel();
            }
        },
    );
    assert!(result.output.contains("cancelled"));
    fs::write(dir.path().join("release"), "").unwrap();
    background_wait(&tools, started["task_id"].as_str().unwrap(), |r| {
        r["status"] == "stopped"
    });
    let second = execute(
        &tools,
        "bash",
        json!({"command":"exit 9","mode":"background"}),
    );
    background_wait(&tools, second["task_id"].as_str().unwrap(), |r| {
        r["status"] == "failed"
    });
    let result = execute(
        &tools,
        "wait",
        json!({"task_ids":[started["task_id"],second["task_id"]],"timeout_ms":0}),
    );
    assert_eq!(result["background_results"].as_array().unwrap().len(), 2);
    assert_eq!(result["timed_out"], false);
}

#[test]
fn wait_steering_and_followup_remain_queued_until_the_batch_settles() {
    use crate::agent::input::PendingKind;
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let task = execute(
        &tools,
        "bash",
        json!({"command":"while [ ! -f release ]; do sleep 0.01; done","mode":"background"}),
    );
    let queue = InputQueue::default();
    queue
        .enqueue(PendingKind::FollowUp, "later".into(), None, false)
        .unwrap();
    tools.start_wait_batch();
    let workflow = WorkflowState::default();
    let mut progress = 0;
    let result = tools.execute(
        "wait",
        "{}",
        "wait",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        &Default::default(),
        &mut |event| {
            if matches!(event, AgentEvent::ToolOutputSnapshot { .. }) {
                progress += 1;
                if progress > 1 {
                    fs::write(dir.path().join("release"), "").unwrap();
                    return;
                }
                queue
                    .enqueue(
                        PendingKind::Steering,
                        "change direction".into(),
                        None,
                        false,
                    )
                    .unwrap();
            }
        },
    );
    let value: Value = serde_json::from_str(&result.output).unwrap();
    assert!(progress >= 2);
    assert_eq!(value["interrupted"], false);
    assert_eq!(value["timed_out"], false);
    assert_eq!(value["pending"], false);
    assert!(queue.has_steering().unwrap());
    assert_eq!(
        execute(
            &tools,
            "bash_task",
            json!({"task_id":task["task_id"],"action":"status"})
        )["status"],
        "exited"
    );
}

#[test]
fn wait_cancelled_call_does_not_deliver_a_completed_background_result() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let task = execute(
        &tools,
        "bash",
        json!({"command":"printf 'kept'","mode":"background"}),
    );
    background_wait(&tools, task["task_id"].as_str().unwrap(), |r| {
        r["status"] == "exited"
    });
    let token = CancellationToken::new();
    token.cancel();
    let result = tools.execute(
        "wait",
        "{}",
        "cancel",
        ToolPolicy::Execute {
            workflow: &WorkflowState::default(),
        },
        &token,
        &mut |_| {},
    );
    assert!(result.output.contains("cancelled"));
    assert_eq!(
        execute(&tools, "wait", json!({}))["background_results"][0]["output"],
        "kept"
    );
}
