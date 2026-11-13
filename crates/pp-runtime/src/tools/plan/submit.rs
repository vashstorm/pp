//! `plan_submit`：revision／任务清单校验与 steering 封闭边界。
use anyhow::{Result, ensure};
use pp_core::plan;
use pp_core::plan::PlanDocument;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{PlanChange, save, verified_target};
use crate::agent::input::InputQueue;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PlanSubmitArgs {
    expected_revision: u64,
}

pub(super) fn submit(
    workspace: &save::Workspace,
    document: &PlanDocument,
    input: Option<&InputQueue>,
    args: PlanSubmitArgs,
) -> Result<(Value, Option<PlanChange>)> {
    ensure!(
        document.revision >= 1 && args.expected_revision == document.revision,
        "Plan revision changed; review the latest plan before writing."
    );
    // 提交前同样要求磁盘字节与快照一致；句柄仅用于这次校验。
    let path = document
        .path()
        .ok_or_else(|| anyhow::anyhow!("No saved plan."))?;
    verified_target(workspace, &path, document.content.as_bytes())?;
    let (_, total) = plan::task_progress(&document.content);
    ensure!(
        total > 0,
        "Plan must contain at least one Markdown TODO item."
    );
    // The submit boundary closes steering reception inside one queue lock;
    // pending steering input defers the submission without consuming it.
    if let Some(queue) = input {
        ensure!(
            queue.seal_for_plan_review()?,
            "Apply pending input before submitting the plan."
        );
    }
    Ok((
        json!({"ok":true,"revision":document.revision}),
        Some(PlanChange::Submitted),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::plan_document;
    use super::*;

    #[test]
    fn submit_requires_revision_tasklist_and_disk_match() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let tasks = "- [ ] task\n- [x] verified\n";

        // revision 0 cannot be submitted.
        let error = submit(
            &workspace,
            &plan_document(0, "", ""),
            None,
            PlanSubmitArgs {
                expected_revision: 0,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan revision changed; review the latest plan before writing."
        );

        let ready = plan_document(1, "Title", tasks);
        let ready_path = ready.path().unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();

        // A disk/snapshot mismatch is reported before the task-list gate.
        std::fs::write(dir.path().join(&ready_path), "- plain paragraph\n").unwrap();
        let error = submit(
            &workspace,
            &ready,
            None,
            PlanSubmitArgs {
                expected_revision: 1,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan file changed externally; review it before writing."
        );
        std::fs::write(dir.path().join(&ready_path), tasks).unwrap();

        // Fenced examples do not count as a task list.
        let mut fenced = plan_document(2, "Title", "```\n- [ ] not a task\n```\n");
        fenced.file_name = Some("fenced-example.md".into());
        std::fs::write(
            dir.path().join(fenced.path().unwrap()),
            "```\n- [ ] not a task\n```\n",
        )
        .unwrap();
        let error = submit(
            &workspace,
            &fenced,
            None,
            PlanSubmitArgs {
                expected_revision: 2,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan must contain at least one Markdown TODO item."
        );

        let (value, change) = submit(
            &workspace,
            &ready,
            None,
            PlanSubmitArgs {
                expected_revision: 1,
            },
        )
        .unwrap();
        assert_eq!(value, json!({"ok":true,"revision":1}));
        assert!(matches!(change, Some(PlanChange::Submitted)));
    }

    #[test]
    fn submit_seals_queue_and_pending_steering_defers_without_consuming() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let tasks = "- [ ] task\n";
        let ready = plan_document(1, "Title", tasks);
        let ready_path = ready.path().unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        std::fs::write(dir.path().join(&ready_path), tasks).unwrap();

        // Pending steering input defers the submission, unchanged and intact.
        let queue = InputQueue::default();
        queue.begin_run().unwrap();
        queue
            .enqueue(
                crate::agent::input::PendingKind::Steering,
                "steer".into(),
                None,
                false,
            )
            .unwrap();
        let before = queue.snapshot_since(None).unwrap().unwrap().revision;
        let error = submit(
            &workspace,
            &ready,
            Some(&queue),
            PlanSubmitArgs {
                expected_revision: 1,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Apply pending input before submitting the plan."
        );
        assert_eq!(
            queue.snapshot_since(None).unwrap().unwrap().revision,
            before
        );
        assert_eq!(
            queue.claim_steering(false).unwrap().len(),
            1,
            "steering input must survive a deferred submission"
        );

        // Follow-up input does not block submission and stays pending.
        let queue = InputQueue::default();
        queue.begin_run().unwrap();
        queue
            .enqueue(
                crate::agent::input::PendingKind::FollowUp,
                "later".into(),
                None,
                false,
            )
            .unwrap();
        let (value, change) = submit(
            &workspace,
            &ready,
            Some(&queue),
            PlanSubmitArgs {
                expected_revision: 1,
            },
        )
        .unwrap();
        assert_eq!(value, json!({"ok":true,"revision":1}));
        assert!(matches!(change, Some(PlanChange::Submitted)));
        assert!(queue.claim_next().unwrap().is_some());

        // A second submission after the seal still succeeds (idempotent seal).
        let (value, _) = submit(
            &workspace,
            &ready,
            Some(&queue),
            PlanSubmitArgs {
                expected_revision: 1,
            },
        )
        .unwrap();
        assert_eq!(value, json!({"ok":true,"revision":1}));
    }
}
