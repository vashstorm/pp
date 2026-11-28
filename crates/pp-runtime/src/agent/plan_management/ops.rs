use super::{
    CHANGED, DELETED_PLAN, NAME_EXISTS, NOT_FOUND, PP_DIR_NAME, PlanListing, PlanSelection,
    is_missing, read_plan_bytes,
};
use crate::agent::Agent;
use anyhow::{Context, Result, anyhow, bail, ensure};
use pp_core::plan::PLAN_MAX_BYTES;
use pp_core::plan::PlanLocation;
use pp_core::plan::path_for;
use pp_core::plan::valid_id;
use pp_core::plan::validate_file_name;
use pp_core::plan::validate_stored_file_name;
use pp_tools::plan_catalog::{self, PlanCatalogLock, PlanFileIdentity, PlanLease};
use pp_workspace::save::Workspace;
use rustix::fs::Dir;
use std::ffi::OsString;
use std::path::Path;

impl Agent {
    /// Rename: after verifying the selection, check the new name (strict kebab rules, on-disk
    /// name clashes across the three directories and active name clashes in the registry); an
    /// identical name succeeds directly, otherwise link → CAS-update the registry → sync the
    /// current binding → clean up the old-name link. Only the file name changes, never the
    /// H1/body/checkbox/ID/body revision; the old path token naturally expires because the path
    /// changed.
    pub fn rename_listed_plan(
        &mut self,
        selection: PlanSelection,
        file_name: String,
    ) -> Result<PlanListing> {
        self.journal.check()?;
        let verified = self.verify_selection(&selection)?;
        if file_name == verified.file_name {
            return self.list_plans();
        }
        validate_file_name(&file_name)?;
        let workspace = self.tools.plan_workspace();
        let verified_identity = plan_catalog::file_identity(workspace, Path::new(&verified.path))?
            .context("Plan file vanished; refresh the list.")?;
        {
            let _lock = PlanCatalogLock::acquire(workspace)?;
            let snapshot = plan_catalog::load_catalog(workspace)?;
            ensure!(
                snapshot.catalog.active_by_file_name(&file_name).is_none(),
                "{NAME_EXISTS}"
            );
            // On-disk name-clash precheck across the three lifecycle directories; a missing
            // directory counts as no clash, and a concurrent collision after the precheck is
            // caught by `link_plan`'s same-inode confirmation, never by overwriting.
            for location in [
                PlanLocation::Drafts,
                PlanLocation::Plans,
                PlanLocation::Done,
            ] {
                let candidate = path_for(&file_name, location);
                match plan_catalog::file_identity(workspace, Path::new(&candidate)) {
                    Ok(Some(identity)) => {
                        ensure!(identity == verified_identity, "{NAME_EXISTS}");
                    }
                    Ok(None) => {}
                    Err(error) if is_missing(&error) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        // Publish the new-name link and CAS-update the registry; on failure keep the source name
        // and return the real error.
        plan_catalog::publish_plan_move(
            workspace,
            &verified.id,
            (verified.file_name.as_str(), verified.location),
            (file_name.as_str(), verified.location),
            verified.content.as_bytes(),
        )?;
        // Sync the current binding: file name only; the registry is committed, so a journal
        // failure returns the real error.
        // End the workspace borrow above before mutating workflow to avoid a borrow conflict.
        if self
            .workflow
            .plan
            .as_ref()
            .is_some_and(|document| document.id == verified.id)
        {
            let previous = self.workflow.clone();
            let document = self.workflow.plan.as_mut().expect("bound plan");
            document.file_name = Some(file_name.clone());
            if let Err(error) = self.journal.write_workflow(&self.workflow) {
                self.workflow = previous;
                return Err(error);
            }
        }
        // Clean up the old-name link; a failure is left to ID reconciliation's same-inode
        // leftover cleanup.
        let workspace = self.tools.plan_workspace();
        if let Err(error) = plan_catalog::commit_plan_move(
            workspace,
            (verified.file_name.as_str(), verified.location),
            (file_name.as_str(), verified.location),
            verified.content.as_bytes(),
        ) {
            tracing::warn!(event = "plan_rename_cleanup_failed", error = %error);
        }
        self.list_plans()
    }

    /// Confirmed deletion: after verifying the selection, first CAS-commit a tombstone recording
    /// the deleted identity, then delete only the selected regular file plus lifecycle leftover
    /// links with the same ID and inode; a registration failure or body mismatch does not unlink
    /// anything. Deleting the current binding only releases the binding and keeps the mode; other
    /// sessions then get a tombstone error by ID and are never refilled from a snapshot. A failed
    /// unlink after the tombstone is committed reports `Delete incomplete`, can be retried
    /// explicitly, and the leftover is never adopted as a new ID.
    pub fn delete_listed_plan(&mut self, selection: PlanSelection) -> Result<PlanListing> {
        self.journal.check()?;
        ensure!(valid_id(&selection.id), "Invalid plan selection.");
        let workspace = self.tools.plan_workspace();
        let lease = PlanLease::acquire(workspace, &selection.id)?;
        let (identity, primary) = {
            let _lock = PlanCatalogLock::acquire(workspace)?;
            let snapshot = plan_catalog::load_catalog(workspace)?;
            if let Some(entry) = snapshot
                .catalog
                .entries
                .iter()
                .find(|entry| entry.id == selection.id && !entry.deleted)
            {
                let path = path_for(&entry.file_name, entry.location);
                ensure!(path == selection.path, "{CHANGED}");
                let target = workspace.target(Path::new(&path), false)?;
                let bytes = target.read_bounded(PLAN_MAX_BYTES)?;
                ensure!(bytes.len() <= PLAN_MAX_BYTES, "Plan file is too large.");
                let content = std::str::from_utf8(&bytes)
                    .map_err(|_| anyhow!("Plan file is not valid UTF-8."))?;
                ensure!(content == selection.content, "{CHANGED}");
                // CAS: commit the tombstone (recording the deleted identity) first, then unlink; a
                // failed registration deletes no file.
                let identity = target
                    .stat_optional()?
                    .context("Plan file vanished; refresh the list.")
                    .map(|(device, inode)| PlanFileIdentity { device, inode })?;
                let mut catalog = snapshot.catalog;
                let entry = catalog
                    .entries
                    .iter_mut()
                    .find(|entry| entry.id == selection.id && !entry.deleted)
                    .expect("active entry still present");
                entry.deleted = true;
                entry.deleted_file_identity = Some(identity);
                plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog)?;
                (identity, path)
            } else if let Some(entry) = snapshot
                .catalog
                .entries
                .iter()
                .find(|entry| entry.id == selection.id && entry.deleted)
            {
                // Deletion-leftover retry: only when the file identity at the path matches the
                // tombstone.
                let identity = entry
                    .deleted_file_identity
                    .ok_or_else(|| anyhow!("{DELETED_PLAN}"))?;
                let path = path_for(&entry.file_name, entry.location);
                ensure!(path == selection.path, "{CHANGED}");
                match workspace
                    .target(Path::new(&path), false)?
                    .stat_optional()?
                    .map(|(device, inode)| PlanFileIdentity { device, inode })
                {
                    Some(current) if current == identity => {
                        let content = read_plan_bytes(workspace, Path::new(&path))?;
                        ensure!(content == selection.content, "{CHANGED}");
                        (identity, path)
                    }
                    _ => bail!("{DELETED_PLAN}"),
                }
            } else {
                bail!("{NOT_FOUND}");
            }
        };
        let mut failures: Vec<String> = Vec::new();
        remove_matching(workspace, Path::new(&primary), identity, &mut failures);
        // Lifecycle leftover links with the same inode: only names confirmed to share the deleted
        // file's inode are cleaned up.
        for location in [
            PlanLocation::Drafts,
            PlanLocation::Plans,
            PlanLocation::Done,
        ] {
            let names = stored_names(workspace, location).unwrap_or_default();
            for name in names {
                let path = path_for(&name, location);
                if path == primary {
                    continue;
                }
                if let Ok(Some(current)) = plan_catalog::file_identity(workspace, Path::new(&path))
                    && current == identity
                {
                    remove_matching(workspace, Path::new(&path), identity, &mut failures);
                }
            }
        }
        ensure!(
            failures.is_empty(),
            "Delete incomplete; retry deleting this plan from /plan-list."
        );
        // Delete the current binding: release only the plan/review binding and keep the mode;
        // legacy sessions report it as deleted by ID and are not refilled from a snapshot.
        if self
            .plan_lease
            .as_ref()
            .is_some_and(|lease| lease.id() == selection.id)
        {
            self.plan_lease = None;
        }
        if self
            .workflow
            .plan
            .as_ref()
            .is_some_and(|document| document.id == selection.id)
        {
            let previous = self.workflow.clone();
            self.workflow.plan = None;
            self.workflow.review = None;
            if self.workflow.todo_plan_id.as_deref() == Some(selection.id.as_str()) {
                self.workflow.todos.clear();
                self.workflow.todo_plan_id = None;
            }
            if let Err(error) = self.journal.write_workflow(&self.workflow) {
                self.workflow = previous;
                return Err(error);
            }
        }
        drop(lease);
        self.list_plans()
    }
}

/// `.md` names among a directory's direct children that satisfy the stored naming rules; used by
/// leftover sweeps, with a missing directory counted as empty.
fn stored_names(workspace: &Workspace, location: PlanLocation) -> Result<Vec<String>> {
    let dir = match workspace.directory(
        &[
            OsString::from(PP_DIR_NAME),
            OsString::from(location.dir_name()),
        ],
        false,
    ) {
        Ok(dir) => dir,
        Err(error) if is_missing(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut names = Vec::new();
    for entry in Dir::new(dir)
        .map_err(std::io::Error::from)
        .context("opening plan directory")?
    {
        let entry = entry
            .map_err(std::io::Error::from)
            .context("reading plan directory entries")?;
        let raw = entry.file_name();
        if raw.to_bytes() == b"." || raw.to_bytes() == b".." {
            continue;
        }
        let Ok(name) = raw.to_str() else {
            continue;
        };
        if validate_stored_file_name(name).is_ok() {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// Remove a single file once its identity is confirmed; a missing file counts as done, a
/// different inode is never touched, and other failures are recorded in failures for the caller
/// to report as `Delete incomplete`.
fn remove_matching(
    workspace: &Workspace,
    path: &Path,
    identity: PlanFileIdentity,
    failures: &mut Vec<String>,
) {
    let outcome = (|| -> Result<()> {
        let target = workspace.target(path, false)?;
        match target
            .stat_optional()?
            .map(|(device, inode)| PlanFileIdentity { device, inode })
        {
            Some(current) if current == identity => target.unlink(),
            Some(_) => Ok(()),
            None => Ok(()),
        }
    })();
    if let Err(error) = outcome {
        failures.push(format!("{}: {error:#}", path.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{entry, fixture_agent, registered, write_plan};
    use super::super::plan_display_title;
    use super::*;
    use crate::agent::tests::{err_message, seed_session};
    use pp_core::plan::PlanDocument;
    use pp_core::plan::PlanStatus;
    use pp_core::plan::WorkMode;
    use pp_core::plan::WorkflowState;

    fn bound_workflow(id: &str, file_name: &str, content: &str) -> WorkflowState {
        let mut document = PlanDocument::new().unwrap();
        document.id = id.to_owned();
        document.revision = 1;
        document.title = plan_display_title(content, file_name);
        document.content = content.to_owned();
        document.status = PlanStatus::Draft;
        document.origin_prompt = "list fixture".to_owned();
        document.file_name = Some(file_name.to_owned());
        document.location = PlanLocation::Drafts;
        WorkflowState {
            mode: WorkMode::Plan,
            plan: Some(document),
            ..Default::default()
        }
    }

    #[test]
    fn rename_updates_catalog_binding_and_keeps_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        let content = "# Rename\n\n- [ ] task\n";
        write_plan(&root, PlanLocation::Drafts, "rename-me.md", content);
        let listing = agent.list_plans().unwrap();
        let old = entry(&listing, "rename-me.md").clone();
        agent.workflow = bound_workflow(&old.id, "rename-me.md", content);
        seed_session(&mut agent, "rename fixture");

        let selection = agent.read_listed_plan(&old.id).unwrap();

        // An invalid name is rejected and the original file is unchanged.
        let error =
            err_message(agent.rename_listed_plan(selection.clone(), "Bad Name.md".to_owned()));
        assert!(error.contains("kebab-case"));
        assert!(root.join(".pp/drafts/rename-me.md").exists());

        // A name clash is rejected.
        write_plan(&root, PlanLocation::Plans, "taken-name.md", "# taken\n");
        agent.list_plans().unwrap();
        let error =
            err_message(agent.rename_listed_plan(selection.clone(), "taken-name.md".to_owned()));
        assert!(error.contains("already exists"));
        assert!(root.join(".pp/drafts/rename-me.md").exists());
        std::fs::remove_file(root.join(".pp/plans/taken-name.md")).unwrap();

        // Same name: succeeds as a no-op.
        agent
            .rename_listed_plan(selection.clone(), "rename-me.md".to_owned())
            .unwrap();

        // Normal rename: the old path disappears, bytes and ID are kept, and the registry and
        // binding stay in sync.
        let listing = agent
            .rename_listed_plan(selection.clone(), "renamed-plan.md".to_owned())
            .unwrap();
        assert!(!root.join(".pp/drafts/rename-me.md").exists());
        let renamed = root.join(".pp/drafts/renamed-plan.md");
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), content);
        assert_eq!(entry(&listing, "renamed-plan.md").id, old.id);
        let registered = registered(&agent, "renamed-plan.md");
        assert_eq!(registered.id, old.id);
        assert_eq!(registered.location, PlanLocation::Drafts);
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().file_name.as_deref(),
            Some("renamed-plan.md")
        );
        assert_eq!(agent.workflow.plan.as_ref().unwrap().content, content);
        let session_id = agent.session_id().unwrap().to_owned();
        assert_eq!(
            crate::agent::tests::journal_workflow(&dir.path().join(".pp/agent"), &session_id)
                .plan
                .unwrap()
                .file_name
                .as_deref(),
            Some("renamed-plan.md")
        );

        // The old selection's path is no longer valid; the same body cannot authorize an
        // operation on the new path.
        assert!(err_message(agent.verify_selection(&selection)).contains("Plan changed"));
        assert!(err_message(agent.delete_listed_plan(selection.clone())).contains("Plan changed"));
        assert!(
            err_message(agent.rename_listed_plan(selection, "stale-rename.md".to_owned()))
                .contains("Plan changed")
        );
        let refreshed = agent.read_listed_plan(&old.id).unwrap();
        assert_eq!(refreshed.path, ".pp/drafts/renamed-plan.md");
        assert!(agent.verify_selection(&refreshed).is_ok());
    }

    #[test]
    fn delete_commits_tombstone_before_unlink_and_clears_binding() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        let content = "# Delete\n\n- [ ] task\n";
        write_plan(&root, PlanLocation::Drafts, "delete-me.md", content);
        write_plan(&root, PlanLocation::Plans, "leftover-name.md", content);
        let listing = agent.list_plans().unwrap();
        let target = entry(&listing, "delete-me.md").clone();
        agent.workflow = bound_workflow(&target.id, "delete-me.md", content);
        // Simulates the plan preview cache left behind by an older version loading the details.
        agent.workflow.todos =
            pp_tools::todo::project_plan_todos(agent.workflow.plan.as_ref().unwrap(), &[], false)
                .unwrap();
        agent.workflow.todo_plan_id = Some(target.id.clone());
        seed_session(&mut agent, "delete fixture");
        let selection = agent.read_listed_plan(&target.id).unwrap();

        // Exact bytes do not match: neither a tombstone is committed nor anything deleted
        // (registered() only matches active entries).
        let mut stale = selection.clone();
        stale.content = "# Stale\n".to_owned();
        assert!(err_message(agent.delete_listed_plan(stale)).contains("Plan changed"));
        assert!(root.join(".pp/drafts/delete-me.md").exists());
        let _active = registered(&agent, "delete-me.md");

        // Deleting the current binding: the file and the same-inode leftover link disappear, the
        // binding is released and the mode is kept.
        let leftover_path = root.join(".pp/plans/leftover-name.md");
        std::fs::remove_file(&leftover_path).unwrap();
        std::fs::hard_link(root.join(".pp/drafts/delete-me.md"), &leftover_path).unwrap();
        let listing = agent.delete_listed_plan(selection.clone()).unwrap();
        assert!(listing.entries.iter().all(|entry| entry.id != target.id));
        assert!(!root.join(".pp/drafts/delete-me.md").exists());
        assert!(!leftover_path.exists());
        assert!(agent.workflow.plan.is_none());
        assert!(agent.workflow.todos.is_empty());
        assert!(agent.workflow.todo_plan_id.is_none());
        assert_eq!(agent.workflow.mode, WorkMode::Plan);
        let workspace = agent.tools.plan_workspace();
        {
            let _lock = PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let tombstone = snapshot
                .catalog
                .entries
                .iter()
                .find(|entry| entry.id == target.id)
                .unwrap();
            assert!(tombstone.deleted);
            assert!(tombstone.deleted_file_identity.is_some());
        }
        // Later access reports it as deleted by ID and never refills it from a snapshot.
        assert!(err_message(agent.read_listed_plan(&target.id)).contains("Plan was deleted"));
        assert!(err_message(agent.verify_selection(&selection)).contains("Plan was deleted"));
    }

    #[test]
    fn delete_retries_confirmed_leftover_without_readopting() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        let content = "# Retry\n\n- [ ] task\n";
        write_plan(&root, PlanLocation::Drafts, "retry-me.md", content);
        let listing = agent.list_plans().unwrap();
        let id = entry(&listing, "retry-me.md").id.clone();
        let selection = agent.read_listed_plan(&id).unwrap();

        // Commit a tombstone by hand without unlinking (simulating a successful first-delete
        // registration followed by a failed deletion).
        let workspace = agent.tools.plan_workspace();
        {
            let _lock = PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let mut catalog = snapshot.catalog;
            let entry = catalog
                .entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .unwrap();
            entry.deleted = true;
            entry.deleted_file_identity = Some(
                plan_catalog::file_identity(workspace, Path::new(&selection.path))
                    .unwrap()
                    .unwrap(),
            );
            plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
        }
        // The leftover can still be re-read to confirm the deletion; an external body change on
        // the same inode invalidates the old confirmation.
        assert_eq!(agent.read_listed_plan(&id).unwrap(), selection);
        std::fs::write(root.join(&selection.path), "# Updated retry\n").unwrap();
        assert!(err_message(agent.delete_listed_plan(selection.clone())).contains("Plan changed"));
        assert!(root.join(&selection.path).exists());
        let selection = agent.read_listed_plan(&id).unwrap();
        // Retry the deletion: the same-inode leftover is cleaned up, the registration stays a
        // tombstone and is not adopted as a new ID.
        agent.delete_listed_plan(selection).unwrap();
        assert!(!root.join(".pp/drafts/retry-me.md").exists());
        let workspace = agent.tools.plan_workspace();
        {
            let _lock = PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let tombstone = snapshot
                .catalog
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .unwrap();
            assert!(tombstone.deleted);
        }
    }

    #[test]
    fn management_respects_live_plan_lease() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        write_plan(&root, PlanLocation::Drafts, "busy-plan.md", "# Busy\n");
        let listing = agent.list_plans().unwrap();
        let id = entry(&listing, "busy-plan.md").id.clone();
        let selection = agent.read_listed_plan(&id).unwrap();

        // Another real PlanLease holder: rename/delete are rejected without blocking; listing
        // and preview remain readable.
        let workspace = agent.tools.plan_workspace();
        let holder = PlanLease::acquire(workspace, &id).unwrap();
        // A second lease for the same ID conflicts immediately (non-blocking, no wait).
        assert!(err_message(PlanLease::acquire(workspace, &id)).contains("Plan is in use"));
        let rename_error =
            err_message(agent.rename_listed_plan(selection.clone(), "renamed-plan.md".to_owned()));
        assert!(rename_error.contains("Plan is in use"));
        let delete_error = err_message(agent.delete_listed_plan(selection));
        assert!(delete_error.contains("Plan is in use"));
        assert!(root.join(".pp/drafts/busy-plan.md").exists());
        assert!(agent.list_plans().is_ok());
        assert!(agent.read_listed_plan(&id).is_ok());
        drop(holder);
        // Manageable again after release.
        let selection = agent.read_listed_plan(&id).unwrap();
        agent
            .rename_listed_plan(selection, "renamed-plan.md".to_owned())
            .unwrap();
        assert!(root.join(".pp/drafts/renamed-plan.md").exists());
    }
}
