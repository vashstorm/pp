//! Lifecycle moves: file identity, publish/commit and confirmed leftover cleanup.
use anyhow::{Result, bail, ensure};
use std::path::Path;

use super::{PlanCatalogEntry, PlanCatalogLock, PlanFileIdentity, load_catalog, save_catalog};
use pp_core::plan::PlanLocation;
use pp_workspace::save::Workspace;

/// Safely read the file identity of a relative-path target; missing returns `None` and symlinks or non-regular
/// files error. Used for registry tombstones and same-inode leftover confirmation, and no substitute for the
/// exact byte read performed before an operation.
pub fn file_identity(workspace: &Workspace, path: &Path) -> Result<Option<PlanFileIdentity>> {
    let target = workspace.target(path, false)?;
    Ok(target
        .stat_optional()?
        .map(|(device, inode)| PlanFileIdentity { device, inode }))
}

/// Publish step of a lifecycle move: after checking the current registration (tombstone, source identity, target name clash),
/// publish a non-overwriting same-inode link at the target and update the registry path with an atomic CAS.
/// A failure keeps the source link; an existing same-inode target allows idempotent re-entry. Lock order: the caller must
/// already hold the plan `PlanLease`, and this function then acquires `PlanCatalogLock`. After a successful publish
/// the caller commits workflow/handoff and only then calls `commit_plan_move` to remove the source name.
pub fn publish_plan_move(
    workspace: &Workspace,
    id: &str,
    source: (&str, PlanLocation),
    target: (&str, PlanLocation),
    expected: &[u8],
) -> Result<Option<String>> {
    let _lock = PlanCatalogLock::acquire(workspace)?;
    let snapshot = load_catalog(workspace)?;
    if snapshot
        .catalog
        .entries
        .iter()
        .any(|entry| entry.deleted && entry.id == id)
    {
        bail!("Plan was deleted; choose another plan from /plan-list.");
    }
    let (source_name, source_location) = source;
    let (target_name, target_location) = target;
    match snapshot.catalog.active_by_id(id) {
        // Already at the target identity: idempotent re-entry that only checks the same-inode link, no registry update needed.
        Some(entry) if entry.file_name == target_name && entry.location == target_location => {
            return workspace.link_plan(
                Path::new(&path_for_name(source_name, source_location)),
                Path::new(&path_for_name(target_name, target_location)),
                expected,
            );
        }
        // Normal move: the registry entry must still point at the source identity; otherwise another session
        // already changed the registry, so fail closed and wait for ID-based reconciliation.
        Some(entry) => {
            ensure!(
                entry.file_name == source_name && entry.location == source_location,
                "Plan registration moved; reopen the plan from /plan-list."
            );
        }
        None => {}
    }
    ensure!(
        snapshot
            .catalog
            .entries
            .iter()
            .all(|entry| entry.deleted || entry.id == id || entry.file_name != target_name),
        "Plan file name is registered to another plan."
    );
    let warning = workspace.link_plan(
        Path::new(&path_for_name(source_name, source_location)),
        Path::new(&path_for_name(target_name, target_location)),
        expected,
    )?;
    let mut catalog = snapshot.catalog;
    match catalog
        .entries
        .iter_mut()
        .find(|entry| entry.id == id && !entry.deleted)
    {
        Some(entry) => {
            entry.file_name = target_name.to_owned();
            entry.location = target_location;
        }
        None => catalog.entries.push(PlanCatalogEntry {
            id: id.to_owned(),
            file_name: target_name.to_owned(),
            location: target_location,
            deleted: false,
            deleted_file_identity: None,
        }),
    }
    save_catalog(workspace, snapshot.raw.as_deref(), &catalog)
        .map(|registration_warning| warning.or(registration_warning))
}

/// Commit step of a lifecycle move: remove the source link after the caller committed workflow/handoff.
/// A failure keeps the source link and leaves same-inode leftover cleanup to ID-based reconciliation.
pub fn commit_plan_move(
    workspace: &Workspace,
    source: (&str, PlanLocation),
    target: (&str, PlanLocation),
    expected: &[u8],
) -> Result<Option<String>> {
    workspace.finish_plan_move(
        Path::new(&path_for_name(source.0, source.1)),
        Path::new(&path_for_name(target.0, target.1)),
        expected,
    )
}

/// Leftover cleanup during recovery reconciliation: remove the old name only when the old lifecycle path and the new
/// registered path are confirmed to be the same inode; missing paths, different inodes, symlinks and any read error keep
/// the status quo, never deleting beyond authority and never letting an unconfirmed leftover block plan access.
pub fn cleanup_confirmed_leftover(
    workspace: &Workspace,
    current: &str,
    leftover: &str,
) -> Result<()> {
    if current == leftover {
        return Ok(());
    }
    let current_target = workspace.target(Path::new(current), false)?;
    let Some(current_identity) = current_target.stat_optional()? else {
        return Ok(());
    };
    let Ok(leftover_target) = workspace.target(Path::new(leftover), false) else {
        // Anomalous leftovers (symlinks, special files, missing directories) are left untouched for explicit management.
        return Ok(());
    };
    let Ok(Some(leftover_identity)) = leftover_target.stat_optional() else {
        return Ok(());
    };
    if leftover_identity != current_identity {
        return Ok(());
    }
    leftover_target.unlink()?;
    Ok(())
}

fn path_for_name(file_name: &str, location: PlanLocation) -> String {
    pp_core::plan::path_for(file_name, location)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{sample_id, workspace};
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, symlink};

    #[test]
    fn file_identity_matches_metadata_and_refuses_missing_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        let path = dir.path().join(".pp/plans/a.md");
        fs::write(&path, b"# title").unwrap();

        let identity = file_identity(&ws, Path::new(".pp/plans/a.md"))
            .unwrap()
            .unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(identity.device, metadata.dev());
        assert_eq!(identity.inode, metadata.ino());

        assert!(
            file_identity(&ws, Path::new(".pp/plans/missing.md"))
                .unwrap()
                .is_none()
        );

        symlink("a.md", dir.path().join(".pp/plans/link.md")).unwrap();
        assert!(file_identity(&ws, Path::new(".pp/plans/link.md")).is_err());
    }

    #[test]
    fn publish_and_commit_move_update_registration_and_preserve_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let id = sample_id(1);
        fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        fs::write(dir.path().join(".pp/drafts/a.md"), b"body").unwrap();

        // Unregistered identity: publish and register at the target location, then commit to remove the source.
        let warning = publish_plan_move(
            &ws,
            &id,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap();
        assert_eq!(warning, None);
        let snapshot = load_catalog(&ws).unwrap();
        let entry = snapshot.catalog.active_by_id(&id).unwrap();
        assert_eq!(entry.file_name, "a.md");
        assert_eq!(entry.location, PlanLocation::Plans);
        assert_eq!(
            fs::read(dir.path().join(".pp/plans/a.md")).unwrap(),
            b"body"
        );

        commit_plan_move(
            &ws,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap();
        assert!(!dir.path().join(".pp/drafts/a.md").exists());
        assert_eq!(
            fs::read(dir.path().join(".pp/plans/a.md")).unwrap(),
            b"body"
        );

        // Already at the target identity: idempotent re-entry (including a same-inode leftover source).
        fs::hard_link(
            dir.path().join(".pp/plans/a.md"),
            dir.path().join(".pp/drafts/a.md"),
        )
        .unwrap();
        publish_plan_move(
            &ws,
            &id,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap();
        commit_plan_move(
            &ws,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap();
        assert!(!dir.path().join(".pp/drafts/a.md").exists());

        // Fail closed when the registry points at a third identity (neither source nor target); a tombstone reports already deleted.
        let error = publish_plan_move(
            &ws,
            &id,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Done),
            b"body",
        )
        .unwrap_err();
        assert!(error.to_string().contains("registration moved"), "{error}");

        let _lock = PlanCatalogLock::acquire(&ws).unwrap();
        let mut snapshot = load_catalog(&ws).unwrap();
        snapshot.catalog.entries[0].deleted = true;
        save_catalog(&ws, snapshot.raw.as_deref(), &snapshot.catalog).unwrap();
        drop(_lock);
        let error = publish_plan_move(
            &ws,
            &id,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Done),
            b"body",
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan was deleted; choose another plan from /plan-list."
        );
    }

    #[test]
    fn publish_move_refuses_other_registered_name_and_keeps_source() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let id = sample_id(1);
        let other = sample_id(2);
        fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        fs::write(dir.path().join(".pp/drafts/mine.md"), b"body").unwrap();

        let _lock = PlanCatalogLock::acquire(&ws).unwrap();
        let snapshot = load_catalog(&ws).unwrap();
        let mut catalog = snapshot.catalog;
        catalog.entries.push(PlanCatalogEntry {
            id: other.clone(),
            file_name: "taken.md".into(),
            location: PlanLocation::Plans,
            deleted: false,
            deleted_file_identity: None,
        });
        save_catalog(&ws, snapshot.raw.as_deref(), &catalog).unwrap();
        drop(_lock);

        let error = publish_plan_move(
            &ws,
            &id,
            ("mine.md", PlanLocation::Drafts),
            ("taken.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("registered to another plan"),
            "{error}"
        );
        assert!(!dir.path().join(".pp/plans/taken-new.md").exists());
        assert_eq!(
            fs::read(dir.path().join(".pp/drafts/mine.md")).unwrap(),
            b"body"
        );
    }

    #[test]
    fn cleanup_leftover_only_removes_inode_confirmed_links() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        fs::write(dir.path().join(".pp/plans/a.md"), b"body").unwrap();

        // A same-inode leftover is removed; a missing leftover completes silently.
        fs::hard_link(
            dir.path().join(".pp/plans/a.md"),
            dir.path().join(".pp/drafts/a.md"),
        )
        .unwrap();
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/drafts/a.md").unwrap();
        assert!(!dir.path().join(".pp/drafts/a.md").exists());
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/drafts/gone.md").unwrap();

        // Different inodes, symlinks and special leftovers all keep the status quo.
        fs::write(dir.path().join(".pp/drafts/other.md"), b"body").unwrap();
        symlink("a.md", dir.path().join(".pp/drafts/alias.md")).unwrap();
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/drafts/other.md").unwrap();
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/drafts/alias.md").unwrap();
        assert_eq!(
            fs::read(dir.path().join(".pp/drafts/other.md")).unwrap(),
            b"body"
        );
        assert!(dir.path().join(".pp/drafts/alias.md").is_symlink());

        // An identical path and a missing registration both do nothing.
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/plans/a.md").unwrap();
        cleanup_confirmed_leftover(&ws, ".pp/plans/missing.md", ".pp/drafts/other.md").unwrap();
        assert!(dir.path().join(".pp/plans/a.md").exists());
    }
}
