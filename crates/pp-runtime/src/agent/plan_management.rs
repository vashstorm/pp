//! Project-level plan management: listing, adopting, renaming and confirmed deletion across the
//! three lifecycle directories.
//!
//! All I/O happens on the worker side; the body remains the only source of content/progress and
//! the registry only provides stable identity and location, never execution permission. The UI
//! only holds a `PlanSelection` operation token; the worker re-reads the registration by ID and
//! verifies the current path/exact body, never operating on disk by list index.

mod list;
mod ops;
mod select;

use anyhow::{Result, anyhow, ensure};
use pp_core::plan::PLAN_MAX_BYTES;
use pp_core::plan::PlanLocation;
use pp_tools::plan_catalog::{PlanFileIdentity, PlanLease};
use pp_workspace::save::Workspace;
use rustix::io::Errno;
use std::path::Path;
use std::time::SystemTime;

/// One plan entry in the list; a non-empty `error` disables entering/renaming/deleting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanListEntry {
    pub id: String,
    pub file_name: String,
    pub location: PlanLocation,
    /// The first H1 text, or the file name when absent; constrained to a valid 1..120 character
    /// display title.
    pub title: String,
    pub completed: usize,
    pub total: usize,
    pub modified: SystemTime,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanListing {
    pub entries: Vec<PlanListEntry>,
    pub diagnostics: Vec<String>,
}

/// Operation token: the ID, the project-relative path at that moment and the exact body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanSelection {
    pub id: String,
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanOpenIntent {
    ContinueDraft,
    Execute,
}

const DELETED_PLAN: &str = "Plan was deleted; choose another plan from /plan-list.";
const CHANGED: &str = "Plan changed; reopen it from /plan-list.";
const NOT_FOUND: &str = "Plan was not found; refresh the list.";
const NAME_EXISTS: &str = "Plan file name already exists; choose another file_name.";
const REGISTER_CONFLICT: &str =
    "Plan file conflicts with the registered plan of the same name; resolve it in /plan-list.";
const PP_DIR_NAME: &str = ".pp";

/// Result of `verify_selection`: the stable identity resolved from the registry, the canonical
/// path and the verified body bytes, plus the plan lease the caller must hold until the
/// operation ends (released on Drop, not stored back into `Agent::plan_lease`).
pub(crate) struct VerifiedSelection {
    pub(crate) id: String,
    pub(crate) file_name: String,
    pub(crate) location: PlanLocation,
    /// The canonical project-relative path resolved from the registry, verified to match
    /// `selection.path`.
    pub(crate) path: String,
    /// The exact bytes currently on disk (verified to match `selection.content`).
    pub(crate) content: String,
    pub(crate) _lease: PlanLease,
}

/// A single valid `.md` direct child found by the directory scan; a stat failure (symlink,
/// special file, identity read error) is kept as an error and the list degrades it to an entry
/// with operations disabled.
struct ScannedFile {
    file_name: String,
    location: PlanLocation,
    stat: Result<(PlanFileIdentity, SystemTime)>,
}

/// NOFOLLOW-safe read of a single plan body; bounded by `PLAN_MAX_BYTES` and required to be
/// UTF-8.
fn read_plan_bytes(workspace: &Workspace, path: &Path) -> Result<String> {
    let target = workspace.target(path, false)?;
    let bytes = target.read_bounded(PLAN_MAX_BYTES)?;
    ensure!(bytes.len() <= PLAN_MAX_BYTES, "Plan file is too large.");
    let content =
        std::str::from_utf8(&bytes).map_err(|_| anyhow!("Plan file is not valid UTF-8."))?;
    Ok(content.to_owned())
}

pub(crate) use pp_core::plan::plan_display_title;

/// Whether the error chain is only "path does not exist"; a missing lifecycle directory means
/// that category is empty.
pub(super) fn is_missing(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<Errno>() == Some(&Errno::NOENT))
}

#[cfg(test)]
mod fixtures {
    use super::super::Agent;
    use super::*;
    use pp_config::config::EditMode;
    use pp_config::config::ModelCatalog;
    use pp_tools::Tools;
    use pp_tools::plan_catalog::{self, PlanCatalogLock};
    use serde_json::json;

    /// An agent journaled into the fixture's global sessions directory, as
    /// production does, with its own working directory.
    pub(super) fn fixture_agent(cwd: &std::path::Path, sessions: &std::path::Path) -> Agent {
        std::fs::create_dir_all(cwd).unwrap();
        let global = sessions.parent().unwrap();
        // Production creates the global sessions directory at startup; the fixture creates it
        // before writing config.
        std::fs::create_dir_all(sessions).unwrap();
        let settings = global.join("settings.yml");
        let models = global.join("models.yml");
        std::fs::write(&settings, "default_model: fixture/one\n").unwrap();
        std::fs::write(
            &models,
            json!({"providers":{"fixture": { "api_key":"fixture-key", "protocol":"completion",
                "models":{"one": {"model":"one"}}}}})
            .to_string(),
        )
        .unwrap();
        Agent::new(
            ModelCatalog::from_files(&settings, &models).unwrap(),
            Tools::new(cwd.to_owned(), EditMode::Replace).unwrap(),
            sessions.to_owned(),
        )
        .unwrap()
    }

    pub(super) fn write_plan(
        dir: &std::path::Path,
        location: PlanLocation,
        name: &str,
        content: &str,
    ) {
        let path = dir.join(".pp").join(location.dir_name()).join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    pub(super) fn entry<'a>(listing: &'a PlanListing, file_name: &str) -> &'a PlanListEntry {
        listing
            .entries
            .iter()
            .find(|entry| entry.file_name == file_name)
            .unwrap()
    }

    pub(super) fn registered(
        agent: &Agent,
        file_name: &str,
    ) -> pp_tools::plan_catalog::PlanCatalogEntry {
        let workspace = agent.tools.plan_workspace();
        let _lock = PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = plan_catalog::load_catalog(workspace).unwrap();
        snapshot
            .catalog
            .entries
            .iter()
            .find(|entry| !entry.deleted && entry.file_name == file_name)
            .cloned()
            .unwrap()
    }
}
