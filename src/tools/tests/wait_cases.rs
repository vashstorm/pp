use super::*;

#[test]
fn wait_background_timeout_selection_and_delivery_commit() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    assert_eq!(execute(&tools, "wait", json!({}))["jobs"], json!([]));
    let held = execute(
        &tools,
        "bash",
        json!({"command":"while [ ! -f release ]; do sleep 0.01; done; printf 'first-result'", "mode":"background"}),
    );
    let id = held["task_id"].as_str().unwrap();
    let zero = execute(&tools, "wait", json!({"timeout_ms":0}));
    assert_eq!(zero["pending"], true);
    assert_eq!(zero["timed_out"], true);
    assert_eq!(zero["background_results"], json!([]));
    assert_eq!(zero["jobs"][0]["type"], "bash");
    fs::write(dir.path().join("release"), "").unwrap();
    let done = execute(&tools, "wait", json!({"task_ids":[id],"timeout_ms":5000}));
    assert_eq!(done["ok"], true, "{done}");
    assert_eq!(done["pending"], false);
    assert_eq!(done["background_results"][0]["output"], "first-result");
    let repeated = execute(&tools, "wait", json!({"task_ids":[id]}));
    assert_eq!(repeated["background_results"], json!([]));
    assert_eq!(repeated["jobs"][0]["delivered"], true);
    // 模拟原生历史提交失败：清除当前批次预留后，结果仍可交付。
    tools.start_wait_batch(None);
    assert_eq!(
        execute(&tools, "wait", json!({}))["background_results"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    tools.commit_wait_results().unwrap();
    tools.start_wait_batch(None);
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
fn wait_background_first_completion_collects_failures_without_stopping_siblings() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let slow = execute(
        &tools,
        "bash",
        json!({"command":"sleep 30", "mode":"background"}),
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
    let result = execute(&tools, "wait", json!({}));
    assert_eq!(result["ok"], true);
    assert_eq!(result["pending"], true);
    assert_eq!(result["background_results"][0]["exit_code"], 7);
    assert_eq!(result["background_results"][0]["output"], "failure-output");
    let stopped = execute(
        &tools,
        "bash_task",
        json!({"task_id":slow["task_id"],"action":"stop"}),
    );
    assert_eq!(stopped["status"], "stopped");
    let result = execute(&tools, "wait", json!({"task_ids":[slow["task_id"]]}));
    assert_eq!(result["background_results"][0]["status"], "stopped");
}

#[test]
fn wait_validates_arguments_and_remains_available_without_subagents() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let definitions = tools.all_definitions(WorkMode::Execute);
    assert_eq!(definitions.iter().filter(|d| d.name == "wait").count(), 1);
    for args in [
        json!({"timeout_ms":1800001}),
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
        json!({"command":"while [ ! -f release ]; do sleep 0.01; done; printf 'retained'","mode":"background"}),
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
        r["status"] == "exited"
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
fn wait_steering_interrupts_without_claiming_followup_or_stopping_background() {
    use crate::agent::input::PendingKind;
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let task = execute(
        &tools,
        "bash",
        json!({"command":"sleep 30","mode":"background"}),
    );
    let queue = InputQueue::default();
    queue
        .enqueue(PendingKind::FollowUp, "later".into(), None, false)
        .unwrap();
    tools.start_wait_batch(Some(&queue));
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
    assert_eq!(progress, 1);
    assert_eq!(value["interrupted"], true);
    assert_eq!(value["timed_out"], false);
    assert_eq!(value["pending"], true);
    assert!(queue.has_steering().unwrap());
    assert_eq!(
        execute(
            &tools,
            "bash_task",
            json!({"task_id":task["task_id"],"action":"status"})
        )["status"],
        "running"
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
