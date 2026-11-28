//! Project plan identity registry (`.pp/plan-index.json`) and the plan-level auxiliary lock.
//!
//! The registry is the stable cross-session identity source: it stores only `{id, file_name, location, deleted,
//! deleted_file_identity}`, never bodies, titles, progress or session information; it is not a second
//! TODO database and grants no execution permission. All reads and writes go through the `Workspace`/`Target`
//! NOFOLLOW safe paths; concurrent changes are rejected by the read baseline and `save_if_unchanged`,
//! and are never retried or overwritten automatically.
//!
//! Lock protocol: model tasks and management actions for the same plan ID are mutually exclusive via `.pp/plan-locks/<id>.lock`
//! (`PlanLease`); short registry transactions additionally use `.pp/plan-index.lock`
//! (`PlanCatalogLock`). When both are held the order is fixed: plan lease → registry lock.
//! `load_catalog`/`save_catalog` never lock on their own, so the caller must already hold
//! `PlanCatalogLock`.

mod entries;
mod io;
mod lock;
mod moves;

pub use entries::{
    CatalogSnapshot, PlanCatalog, PlanCatalogEntry, PlanFileIdentity, validate_catalog,
};
pub use io::{load_catalog, save_catalog};
pub use lock::{PlanCatalogLock, PlanLease};
pub use moves::{cleanup_confirmed_leftover, commit_plan_move, file_identity, publish_plan_move};

/// Registry read cap is 16 MiB; a corrupted or oversized registry file must report an explicit error and never be overwritten as an empty table.
pub const CATALOG_MAX_BYTES: usize = 16 * 1024 * 1024;

/// Current registry schema version; loading rejects any mismatch.
pub const CATALOG_VERSION: u32 = 1;

#[cfg(test)]
mod fixtures {
    use super::*;
    use pp_core::plan::PlanLocation;
    use pp_workspace::save::Workspace;
    use std::path::PathBuf;

    pub(super) fn workspace(dir: &tempfile::TempDir) -> Workspace {
        Workspace::open(dir.path()).unwrap()
    }

    pub(super) fn sample_id(number: u8) -> String {
        format!("{number:032x}")
    }

    pub(super) fn entry(
        id: &str,
        file_name: &str,
        location: PlanLocation,
        deleted: bool,
    ) -> PlanCatalogEntry {
        PlanCatalogEntry {
            id: id.to_owned(),
            file_name: file_name.to_owned(),
            location,
            deleted,
            deleted_file_identity: None,
        }
    }

    pub(super) fn index_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join(".pp/plan-index.json")
    }
}
