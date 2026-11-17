use super::*;
use crate::agent::input::{InputQueue, PendingKind};
use pp_core::events::AgentEvent;
use pp_core::plan::{PlanDocument, PlanLocation, PlanStatus, WorkflowState};
use pp_tools::{PlanChange, ToolPolicy, Tools};
use std::fs;

#[test]
fn wait_steering_and_followup_remain_queued_until_the_batch_settles() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    let task_exec = tools.execute(
        "bash",
        &json!({"command":"while [ ! -f release ]; do sleep 0.01; done","mode":"background"})
            .to_string(),
        "test-call",
        ToolPolicy::Execute {
            workflow: &WorkflowState::default(),
        },
        &CancellationToken::new(),
        &mut |_| {},
    );
    let task: Value = serde_json::from_str(&task_exec.output).unwrap();
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
    let status_exec = tools.execute(
        "bash_task",
        &json!({"task_id":task["task_id"],"action":"status"}).to_string(),
        "test-call",
        ToolPolicy::Execute {
            workflow: &WorkflowState::default(),
        },
        &CancellationToken::new(),
        &mut |_| {},
    );
    let status: Value = serde_json::from_str(&status_exec.output).unwrap();
    assert_eq!(status["status"], "exited");
}

#[test]
fn plan_submit_seals_input_queue_and_pending_steering_defers() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), pp_config::config::EditMode::Replace).unwrap();
    let content = "# Title\n- [ ] task\n";
    let mut document = PlanDocument::new().unwrap();
    document.title = "Title".into();
    document.file_name = Some("title.md".into());
    document.location = PlanLocation::Drafts;
    document.content = content.into();
    document.revision = 1;
    document.status = PlanStatus::Draft;
    let plan_path = document.path().unwrap();
    fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
    fs::write(dir.path().join(&plan_path), content).unwrap();

    let queue = InputQueue::default();
    queue.begin_run().unwrap();
    queue
        .enqueue(PendingKind::Steering, "steer".into(), None, false)
        .unwrap();
    let arguments = json!({"expected_revision":1}).to_string();
    let execution = tools.execute(
        "plan_submit",
        &arguments,
        "submit-call",
        ToolPolicy::Plan {
            document: &document,
            sealed: false,
            input: Some(&queue),
        },
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
    let result: Value = serde_json::from_str(&execution.output).unwrap();
    assert_eq!(
        result,
        json!({"ok":false,"error":"Apply pending input before submitting the plan."})
    );
    assert!(execution.plan_change.is_none());
    assert_eq!(
        queue.claim_steering(false).unwrap().len(),
        1,
        "deferred submission must not consume steering input"
    );

    let execution = tools.execute(
        "plan_submit",
        &arguments,
        "submit-call",
        ToolPolicy::Plan {
            document: &document,
            sealed: false,
            input: Some(&queue),
        },
        &CancellationToken::new(),
        &mut |_| {},
    );
    assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
    let result: Value = serde_json::from_str(&execution.output).unwrap();
    assert_eq!(result, json!({"ok":true,"revision":1}));
    assert!(matches!(execution.plan_change, Some(PlanChange::Submitted)));
    assert_eq!(
        fs::read(dir.path().join(&plan_path)).unwrap(),
        content.as_bytes()
    );
}
