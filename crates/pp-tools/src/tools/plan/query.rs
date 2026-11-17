//! Plan 正文的安全读取、路径判定与执行进度回写。
use anyhow::{Result, ensure};
use pp_core::plan;
use pp_core::plan::PLAN_MAX_BYTES;
use pp_core::plan::PlanDocument;
use pp_core::plan::PlanLocation;
use std::path::Path;

use super::{MOVED_PLAN, save};
use crate::tools::plan_catalog;

/// Safe read of the current plan document through the NOFOLLOW workspace
/// handle; oversize, non-UTF-8, symlink and special-type targets are errors,
/// never a fallback to the logged snapshot. A document without a saved
/// lifecycle path reports `No saved plan.` instead of a fake path.
pub(crate) fn read_plan(workspace: &save::Workspace, document: &PlanDocument) -> Result<String> {
    let path = document
        .path()
        .ok_or_else(|| anyhow::anyhow!("No saved plan."))?;
    let target = workspace.target(Path::new(&path), false)?;
    let bytes = target.read_bounded(PLAN_MAX_BYTES)?;
    ensure!(bytes.len() <= PLAN_MAX_BYTES, "Plan file is too large.");
    let content = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("Plan file is not valid UTF-8."))?;
    ensure!(!content.trim().is_empty(), "Plan file is empty.");
    Ok(content.to_owned())
}

/// True only when the workspace-normalized paths are exactly equal; used to
/// react to plan-file edits and to bind review tokens to the reviewed file
/// without canonicalizing in the UI.
pub(crate) fn is_plan_path(workspace: &save::Workspace, path: &str, expected_path: &str) -> bool {
    match (
        workspace.components(Path::new(path)),
        workspace.components(Path::new(expected_path)),
    ) {
        (Ok(changed), Ok(expected)) => changed == expected,
        _ => false,
    }
}

/// Conditional progress write-back for the plan todo adapter. The caller is
/// the Execute+Executed path that already holds the current task's PlanLease
/// (`ensure_task_plan_lease`); no lease is reacquired here and the lock order
/// stays lease → PlanCatalogLock. Within a short catalog lock the active
/// entry is re-checked by ID: non-tombstone, canonical file_name/location in
/// sync with the borrowed document, and located under Plans — progress
/// write-back only happens on handed-off executing plans. Old bytes are read
/// through the bounded NOFOLLOW handle and must match the synced document
/// before `save_if_unchanged_custom` commits; an external edit between the
/// sync and this call fails without overwriting anything. The returned
/// warning (if any) describes an already-committed catalog sync and is
/// passed through to the tool result unchanged.
pub(in crate::tools) fn save_progress(
    workspace: &save::Workspace,
    document: &PlanDocument,
    content: &str,
) -> Result<Option<String>> {
    let _lock = plan_catalog::PlanCatalogLock::acquire(workspace)?;
    let snapshot = plan_catalog::load_catalog(workspace)?;
    let entry = snapshot
        .catalog
        .active_by_id(&document.id)
        .ok_or_else(|| anyhow::anyhow!("Plan is not registered; reopen it from /plan-list."))?;
    ensure!(
        entry.location == PlanLocation::Plans,
        "Plan progress can only be saved to an executing plan under .pp/plans/."
    );
    let bound_matches = match &document.file_name {
        Some(name) => name == &entry.file_name && document.location == entry.location,
        // 旧记录缺文件名：接受登记 canonical 名称，位置必须一致。
        None => document.location == entry.location,
    };
    ensure!(bound_matches, "{MOVED_PLAN}");
    let path = plan::path_for(&entry.file_name, entry.location);
    let target = workspace.target(Path::new(&path), false)?;
    let old = target.read_bounded(PLAN_MAX_BYTES)?;
    ensure!(
        old == document.content.as_bytes(),
        "Plan file changed externally; review it before writing."
    );
    target.save_if_unchanged_custom(
        document.content.as_bytes(),
        content.as_bytes(),
        "Plan file changed externally; review it before writing.",
    )
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{plan_document, plan_todo_registered};
    use super::*;

    #[test]
    fn read_plan_rejects_missing_unsaved_and_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let document = plan_document(1, "Title", "- [ ] task\n");
        let path = document.path().unwrap();

        // 未保存的 revision-0 绑定没有路径，明确报 No saved plan.。
        let unsaved = plan_document(0, "", "");
        let error = read_plan(&workspace, &unsaved).unwrap_err();
        assert_eq!(error.to_string(), "No saved plan.");

        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        std::fs::write(dir.path().join(&path), "- [ ] task\n").unwrap();
        assert_eq!(read_plan(&workspace, &document).unwrap(), "- [ ] task\n");

        // Missing, symlinked and non-UTF-8 targets are errors, not snapshots.
        let mut missing = plan_document(1, "Title", "");
        missing.file_name = None;
        missing.location = PlanLocation::Plans;
        missing.id = "4".repeat(32);
        assert!(read_plan(&workspace, &missing).is_err());
        let mut other = plan_document(1, "Title", "");
        other.file_name = None;
        other.location = PlanLocation::Plans;
        std::os::unix::fs::symlink(
            dir.path().join(&path),
            dir.path().join(other.path().unwrap()),
        )
        .unwrap();
        assert!(read_plan(&workspace, &other).is_err());
        let mut binary = plan_document(1, "Title", "");
        binary.file_name = None;
        binary.location = PlanLocation::Plans;
        binary.id = "2".repeat(32);
        std::fs::write(dir.path().join(binary.path().unwrap()), b"\xff\xfe").unwrap();
        assert!(read_plan(&workspace, &binary).is_err());
        let mut empty = plan_document(1, "Title", "");
        empty.file_name = None;
        empty.location = PlanLocation::Plans;
        empty.id = "3".repeat(32);
        std::fs::write(dir.path().join(empty.path().unwrap()), b"  \n").unwrap();
        assert!(read_plan(&workspace, &empty).is_err());
    }

    #[test]
    fn is_plan_path_matches_only_the_exact_plan_file() {
        // 保存层按 canonical 根校验绝对路径；临时父目录规范化后拼写一致。
        let dir = tempfile::Builder::new()
            .tempdir_in(std::env::temp_dir().canonicalize().unwrap())
            .unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let document = plan_document(1, "Title", "");
        let relative = document.path().unwrap();
        let absolute = dir.path().join(&relative);
        assert!(is_plan_path(&workspace, &relative, &relative));
        assert!(is_plan_path(
            &workspace,
            &absolute.to_string_lossy(),
            &relative
        ));
        assert!(is_plan_path(
            &workspace,
            &format!("./{relative}"),
            &relative
        ));
        assert!(!is_plan_path(&workspace, ".pp/drafts", &relative));
        assert!(!is_plan_path(&workspace, ".pp/drafts/other.md", &relative));
        assert!(!is_plan_path(&workspace, "canary.txt", &relative));
        assert!(!is_plan_path(
            &workspace,
            &relative,
            ".pp/drafts/other-name.md"
        ));
        assert!(!is_plan_path(&workspace, "../outside", &relative));
    }

    #[test]
    fn plan_todo_save_progress_commits_conditionally_and_refuses_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let content = "## Write\n- [ ] Write canary bytes.\n- [ ] Verify canary bytes.\n";
        let mut document = plan_document(1, "Progress", content);
        document.location = PlanLocation::Plans;
        plan_todo_registered(&workspace, &document, content);
        let updated = "## Write\n- [x] Write canary bytes.\n- [ ] Verify canary bytes.\n";
        assert_eq!(save_progress(&workspace, &document, updated).unwrap(), None);
        assert_eq!(
            std::fs::read(dir.path().join(document.path().unwrap())).unwrap(),
            updated.as_bytes()
        );

        // External modification after the sync: the document snapshot no
        // longer matches the disk, so the write-back refuses and the
        // external bytes stay untouched.
        let external = "## Write\n- [x] Write canary bytes.\n- [ ] Verify canary bytes.\n\nExternally appended evidence.\n";
        std::fs::write(dir.path().join(document.path().unwrap()), external).unwrap();
        let error = save_progress(&workspace, &document, updated).unwrap_err();
        assert!(error.to_string().contains("changed externally"), "{error}");
        assert_eq!(
            std::fs::read(dir.path().join(document.path().unwrap())).unwrap(),
            external.as_bytes()
        );

        // Unregistered plan: fail-closed instead of guessing a file name.
        let mut unregistered = plan_document(1, "Other", content);
        unregistered.location = PlanLocation::Plans;
        assert!(save_progress(&workspace, &unregistered, updated).is_err());

        // Tombstoned entry: tombstones never resolve identities.
        let mut tombstoned = plan_document(1, "Progress", content);
        tombstoned.location = PlanLocation::Plans;
        tombstoned.id = document.id.clone();
        {
            let _lock = plan_catalog::PlanCatalogLock::acquire(&workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(&workspace).unwrap();
            let mut catalog = snapshot.catalog;
            catalog
                .entries
                .iter_mut()
                .filter(|entry| entry.id == document.id)
                .for_each(|entry| entry.deleted = true);
            plan_catalog::save_catalog(&workspace, snapshot.raw.as_deref(), &catalog).unwrap();
        }
        assert!(save_progress(&workspace, &tombstoned, updated).is_err());

        // Drafts location: progress write-back is limited to executing
        // plans under .pp/plans/.
        let mut draft = plan_document(1, "Draft", content);
        draft.location = PlanLocation::Drafts;
        plan_todo_registered(&workspace, &draft, content);
        assert!(save_progress(&workspace, &draft, updated).is_err());
    }
}
