use crate::agent::input::InputQueue;
use crate::plan::{self, PLAN_MAX_BYTES, PlanDocument};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

use super::save;

/// Tool-visible outcome of the dedicated plan tools. The protocol-agnostic
/// caller applies it to the borrowed workflow state at the batch commit
/// boundary; executing a tool itself never mutates the workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanChange {
    Written { title: String, content: String },
    Submitted,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanWriteArgs {
    title: String,
    content: String,
    expected_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanSubmitArgs {
    expected_revision: u64,
}

pub(crate) fn run(
    workspace: &save::Workspace,
    document: &PlanDocument,
    input: Option<&InputQueue>,
    name: &str,
    arguments: &str,
) -> Result<(Value, Option<PlanChange>)> {
    match name {
        "plan_write" => {
            let args: PlanWriteArgs =
                serde_json::from_str(arguments).context("invalid plan_write arguments")?;
            write(workspace, document, args)
        }
        "plan_submit" => {
            let args: PlanSubmitArgs =
                serde_json::from_str(arguments).context("invalid plan_submit arguments")?;
            submit(workspace, document, input, args)
        }
        _ => bail!("Tool is not available in Plan mode."),
    }
}

fn write(
    workspace: &save::Workspace,
    document: &PlanDocument,
    args: PlanWriteArgs,
) -> Result<(Value, Option<PlanChange>)> {
    ensure!(
        args.expected_revision == document.revision,
        "Plan revision changed; review the latest plan before writing."
    );
    let title = args.title.trim();
    plan::validate_title(title)?;
    plan::validate_content(&args.content)?;
    let path = document.path();
    if document.revision == 0 {
        // First real save: create `.pp/plans/<id>.md` atomically; a
        // pre-existing or raced target must never be overwritten.
        let target = workspace.target(Path::new(&path), true)?;
        let warning = target.save_new(args.content.as_bytes())?;
        return Ok((
            success(&path, 1, warning),
            Some(PlanChange::Written {
                title: title.to_owned(),
                content: args.content,
            }),
        ));
    }
    // Registered revisions compare the on-disk bytes with the snapshot
    // before any overwrite; an external edit is surfaced, never clobbered.
    let target = verified_target(workspace, document)?;
    if title == document.title && args.content == document.content {
        return Ok((success(&path, document.revision, None), None));
    }
    let revision = document
        .revision
        .checked_add(1)
        .context("Plan revision limit reached.")?;
    let warning = target.save(args.content.as_bytes())?;
    Ok((
        success(&path, revision, warning),
        Some(PlanChange::Written {
            title: title.to_owned(),
            content: args.content,
        }),
    ))
}

fn submit(
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
    verified_target(workspace, document)?;
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

fn success(path: &str, revision: u64, warning: Option<String>) -> Value {
    let mut value = json!({"ok":true,"path":path,"revision":revision});
    if let Some(warning) = warning {
        value["warning"] = json!(warning);
    }
    value
}

/// Opens the current plan file through the safe NOFOLLOW handle and verifies
/// the on-disk bytes still match the registered snapshot; an external edit is
/// surfaced, never clobbered. Shared by plan_write and plan_submit.
fn verified_target(workspace: &save::Workspace, document: &PlanDocument) -> Result<save::Target> {
    let path = document.path();
    let target = workspace.target(Path::new(&path), false)?;
    let disk = target.read_bounded(PLAN_MAX_BYTES)?;
    ensure!(
        disk == document.content.as_bytes(),
        "Plan file changed externally; review it before writing."
    );
    Ok(target)
}

/// Safe read of the current plan document through the NOFOLLOW workspace
/// handle; oversize, non-UTF-8, symlink and special-type targets are errors,
/// never a fallback to the logged snapshot.
pub(crate) fn read_plan(workspace: &save::Workspace, id: &str) -> Result<String> {
    ensure!(plan::valid_id(id), "Invalid plan ID.");
    let path = format!(".pp/plans/{id}.md");
    let target = workspace.target(Path::new(&path), false)?;
    let bytes = target.read_bounded(PLAN_MAX_BYTES)?;
    ensure!(bytes.len() <= PLAN_MAX_BYTES, "Plan file is too large.");
    let content = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("Plan file is not valid UTF-8."))?;
    ensure!(!content.trim().is_empty(), "Plan file is empty.");
    Ok(content.to_owned())
}

/// True only when the workspace-normalized path is exactly the current plan's
/// fixed `.pp/plans/<id>.md` file; used to react to plan-file edits without
/// canonicalizing in the UI.
pub(crate) fn is_plan_path(workspace: &save::Workspace, path: &str, id: &str) -> bool {
    let plan_path = format!(".pp/plans/{id}.md");
    match (
        workspace.components(Path::new(path)),
        workspace.components(Path::new(&plan_path)),
    ) {
        (Ok(changed), Ok(expected)) => changed == expected,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_document(revision: u64, title: &str, content: &str) -> PlanDocument {
        let mut document = PlanDocument::new().unwrap();
        document.revision = revision;
        document.title = title.to_owned();
        document.content = content.to_owned();
        document.origin_prompt = "original task".to_owned();
        document
    }

    fn write_args(title: &str, content: &str, expected_revision: u64) -> PlanWriteArgs {
        PlanWriteArgs {
            title: title.to_owned(),
            content: content.to_owned(),
            expected_revision,
        }
    }

    #[test]
    fn write_first_revision_is_written_once_and_collision_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let document = plan_document(0, "", "");
        let (value, change) = write(
            &workspace,
            &document,
            write_args("  Spaced  ", "- [ ] task\n", 0),
        )
        .unwrap();
        assert_eq!(
            value,
            json!({"ok":true,"path":document.path(),"revision":1})
        );
        let PlanChange::Written { title, content } = change.unwrap() else {
            panic!("first write must produce Written");
        };
        assert_eq!(title, "Spaced");
        assert_eq!(content, "- [ ] task\n");
        assert_eq!(
            std::fs::read(dir.path().join(document.path())).unwrap(),
            b"- [ ] task\n"
        );
        // The plan directory contains only the plan file; no temporaries.
        assert_eq!(
            std::fs::read_dir(dir.path().join(".pp/plans"))
                .unwrap()
                .count(),
            1
        );

        // A pre-existing file at the fixed path blocks a fresh first save.
        let other = plan_document(0, "", "");
        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        std::fs::write(dir.path().join(other.path()), b"external").unwrap();
        let error = write(&workspace, &other, write_args("Title", "- [ ] task", 0)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan path already exists; use /plan new."
        );
        assert_eq!(
            std::fs::read(dir.path().join(other.path())).unwrap(),
            b"external"
        );
    }

    #[test]
    fn write_noop_keeps_revision_external_change_and_overflow_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let content = "- [ ] task\n";
        let document = plan_document(1, "Title", content);
        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        std::fs::write(dir.path().join(document.path()), content).unwrap();

        // Same title and content: success without a revision bump or change.
        let (value, change) =
            write(&workspace, &document, write_args("Title", content, 1)).unwrap();
        assert_eq!(
            value,
            json!({"ok":true,"path":document.path(),"revision":1})
        );
        assert!(change.is_none());

        // A distinct write increments the revision and reports the change.
        let (value, change) = write(
            &workspace,
            &document,
            write_args("Title", "- [ ] task\n- [ ] second\n", 1),
        )
        .unwrap();
        assert_eq!(
            value,
            json!({"ok":true,"path":document.path(),"revision":2})
        );
        assert!(matches!(change, Some(PlanChange::Written { .. })));

        // Stale expected_revision is refused before any disk access.
        let error = write(&workspace, &document, write_args("Title", content, 0)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan revision changed; review the latest plan before writing."
        );

        // External bytes are never clobbered.
        let external = plan_document(1, "Title", content);
        std::fs::write(dir.path().join(external.path()), b"external\n").unwrap();
        let error =
            write(&workspace, &external, write_args("Title", "- [ ] new\n", 1)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan file changed externally; review it before writing."
        );
        assert_eq!(
            std::fs::read(dir.path().join(external.path())).unwrap(),
            b"external\n"
        );

        // Revision overflow is an error, not a wraparound.
        let last = plan_document(u64::MAX, "Title", content);
        std::fs::write(dir.path().join(last.path()), content).unwrap();
        let error = write(
            &workspace,
            &last,
            write_args("Title", "- [ ] new\n", u64::MAX),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Plan revision limit reached.");

        // Invalid titles and NUL content are refused without side effects.
        let current = plan_document(1, "Title", content);
        std::fs::write(dir.path().join(current.path()), content).unwrap();
        let error = write(&workspace, &current, write_args("bad\0title", content, 1)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan title must contain 1 to 120 characters without control characters."
        );
        let error =
            write(&workspace, &current, write_args("Title", "bad\0content", 1)).unwrap_err();
        assert_eq!(error.to_string(), "Plan content must not contain NUL.");
        assert_eq!(
            std::fs::read(dir.path().join(current.path())).unwrap(),
            content.as_bytes()
        );
    }

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
        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();

        // A disk/snapshot mismatch is reported before the task-list gate.
        std::fs::write(dir.path().join(ready.path()), "- plain paragraph\n").unwrap();
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
        std::fs::write(dir.path().join(ready.path()), tasks).unwrap();

        // Fenced examples do not count as a task list.
        let fenced = plan_document(2, "Title", "```\n- [ ] not a task\n```\n");
        std::fs::write(
            dir.path().join(fenced.path()),
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
        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        std::fs::write(dir.path().join(ready.path()), tasks).unwrap();

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

    #[test]
    fn read_plan_rejects_invalid_ids_and_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let document = plan_document(1, "Title", "- [ ] task\n");
        assert!(read_plan(&workspace, "short").is_err());
        assert!(read_plan(&workspace, "ZZZZ").is_err());

        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        std::fs::write(dir.path().join(document.path()), "- [ ] task\n").unwrap();
        assert_eq!(read_plan(&workspace, &document.id).unwrap(), "- [ ] task\n");

        // Missing, symlinked and non-UTF-8 targets are errors, not snapshots.
        assert!(read_plan(&workspace, &"0".repeat(32)).is_err());
        std::os::unix::fs::symlink(
            dir.path().join(document.path()),
            dir.path().join(format!(".pp/plans/{}.md", "1".repeat(32))),
        )
        .unwrap();
        assert!(read_plan(&workspace, &"1".repeat(32)).is_err());
        let binary = plan_document(1, "Title", "");
        std::fs::write(dir.path().join(binary.path()), b"\xff\xfe").unwrap();
        assert!(read_plan(&workspace, &binary.id).is_err());
        let empty = plan_document(1, "Title", "");
        std::fs::write(dir.path().join(empty.path()), b"  \n").unwrap();
        assert!(read_plan(&workspace, &empty.id).is_err());
    }

    #[test]
    fn is_plan_path_matches_only_the_fixed_plan_file() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let document = plan_document(1, "Title", "");
        let relative = document.path();
        let absolute = dir.path().join(&relative);
        assert!(is_plan_path(&workspace, &relative, &document.id));
        assert!(is_plan_path(
            &workspace,
            &absolute.to_string_lossy(),
            &document.id
        ));
        assert!(is_plan_path(
            &workspace,
            &format!("./{relative}"),
            &document.id
        ));
        assert!(!is_plan_path(&workspace, ".pp/plans", &document.id));
        assert!(!is_plan_path(
            &workspace,
            ".pp/plans/other.md",
            &document.id
        ));
        assert!(!is_plan_path(&workspace, "canary.txt", &document.id));
        assert!(!is_plan_path(&workspace, &relative, &"f".repeat(32)));
        assert!(!is_plan_path(&workspace, "../outside", &document.id));
    }
}
