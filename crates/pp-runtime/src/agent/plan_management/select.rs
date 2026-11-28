use super::{CHANGED, DELETED_PLAN, NOT_FOUND, PlanSelection, VerifiedSelection, read_plan_bytes};
use crate::agent::Agent;
use anyhow::{Result, anyhow, bail, ensure};
use pp_core::plan::PlanLocation;
use pp_core::plan::path_for;
use pp_core::plan::valid_id;
use pp_tools::plan_catalog::{self, PlanCatalogLock, PlanLease};
use pp_workspace::save::Workspace;
use std::path::Path;

impl Agent {
    /// Safely read the body of the plan selected from the list as the selection token for later
    /// operations; read-only, issues no approval token and does not enter model history.
    pub fn read_listed_plan(&mut self, id: &str) -> Result<PlanSelection> {
        ensure!(valid_id(id), "Invalid plan id.");
        let workspace = self.tools.plan_workspace();
        let _lock = PlanCatalogLock::acquire(workspace)?;
        let snapshot = plan_catalog::load_catalog(workspace)?;
        let entry = snapshot
            .catalog
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| anyhow!("{NOT_FOUND}"))?;
        let path = path_for(&entry.file_name, entry.location);
        if entry.deleted {
            // Only a same-inode deletion leftover may be read, so the deletion can be confirmed
            // again; entering/renaming still go through resolve_active, which rejects tombstones,
            // and neither the old identity nor execution permission is restored.
            let expected = entry
                .deleted_file_identity
                .ok_or_else(|| anyhow!("{DELETED_PLAN}"))?;
            ensure!(
                plan_catalog::file_identity(workspace, Path::new(&path))? == Some(expected),
                "{DELETED_PLAN}"
            );
        }
        let content = read_plan_bytes(workspace, Path::new(&path))?;
        Ok(PlanSelection {
            id: id.to_owned(),
            path,
            content,
        })
    }

    /// Operation-token verification: acquire the plan lease non-blocking first (busy means taken),
    /// then resolve the canonical name/location by ID under the registry lock (a tombstone reports
    /// deleted), and finally read and verify the exact bytes safely. Lock order is fixed at
    /// lease → registry lock; registration transactions such as `publish_plan_move` are kept
    /// mutually exclusive by the caller's lease. workflow/journal are not touched.
    pub(crate) fn verify_selection(
        &mut self,
        selection: &PlanSelection,
    ) -> Result<VerifiedSelection> {
        ensure!(valid_id(&selection.id), "Invalid plan selection.");
        let workspace = self.tools.plan_workspace();
        let lease = PlanLease::acquire(workspace, &selection.id)?;
        let (file_name, location) = resolve_active(workspace, &selection.id)?;
        let path = path_for(&file_name, location);
        ensure!(path == selection.path, "{CHANGED}");
        let content = read_plan_bytes(workspace, Path::new(&path))?;
        ensure!(content == selection.content, "{CHANGED}");
        Ok(VerifiedSelection {
            id: selection.id.clone(),
            file_name,
            location,
            path,
            content,
            _lease: lease,
        })
    }
}

/// Resolve an active registry entry by ID; a tombstone and a missing entry each error out, and
/// nothing is ever operated on via a guessed path.
fn resolve_active(workspace: &Workspace, id: &str) -> Result<(String, PlanLocation)> {
    let _lock = PlanCatalogLock::acquire(workspace)?;
    let snapshot = plan_catalog::load_catalog(workspace)?;
    if snapshot
        .catalog
        .entries
        .iter()
        .any(|entry| entry.deleted && entry.id == id)
    {
        bail!("{DELETED_PLAN}");
    }
    let entry = snapshot
        .catalog
        .active_by_id(id)
        .ok_or_else(|| anyhow!("{NOT_FOUND}"))?;
    Ok((entry.file_name.clone(), entry.location))
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{entry, fixture_agent, write_plan};
    use super::*;
    use crate::agent::tests::err_message;

    #[test]
    fn read_and_verify_selection_reject_stale_bytes_and_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        write_plan(
            &root,
            PlanLocation::Drafts,
            "verify-plan.md",
            "# Verify\n\n- [ ] task\n",
        );
        let listing = agent.list_plans().unwrap();
        let id = entry(&listing, "verify-plan.md").id.clone();

        let selection = agent.read_listed_plan(&id).unwrap();
        assert_eq!(selection.path, ".pp/drafts/verify-plan.md");
        assert!(selection.content.contains("# Verify"));
        assert!(agent.verify_selection(&selection).is_ok());

        // Exact bytes do not match: the operation is rejected and the old body is not reused.
        let mut stale = selection.clone();
        stale.content = "# Stale\n".to_owned();
        assert!(err_message(agent.verify_selection(&stale)).contains("Plan changed"));

        // Tombstone: both verify and read reject it; it is never revived.
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
        assert_eq!(agent.read_listed_plan(&id).unwrap(), selection);
        assert!(err_message(agent.verify_selection(&selection)).contains("Plan was deleted"));
        std::fs::remove_file(root.join(&selection.path)).unwrap();
        assert!(err_message(agent.read_listed_plan(&id)).contains("Plan was deleted"));
    }
}
