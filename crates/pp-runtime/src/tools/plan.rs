use crate::agent::input::InputQueue;
use anyhow::{Context, Result, bail, ensure};
use pp_core::plan;
use pp_core::plan::PlanDocument;
use pp_core::plan::PlanLocation;
use serde_json::Value;
use std::path::Path;

use super::{WriteArgs, save};

mod query;
mod submit;
mod write;

pub(super) use query::save_progress;
pub(crate) use query::{is_plan_path, read_plan};
use submit::{PlanSubmitArgs, submit};
use write::write;

/// Tool-visible outcome of the dedicated plan tools. The protocol-agnostic
/// caller applies it to the borrowed workflow state at the batch commit
/// boundary; executing a tool itself never mutates the workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanChange {
    Written {
        title: String,
        file_name: String,
        content: String,
    },
    Submitted,
}

const NAME_COLLISION: &str = "Plan file name already exists; choose another plan path.";
const RENAME_BOUND: &str = "Rename plans through /plan-list.";
const MOVED_PLAN: &str = "Plan file moved; reopen it from /plan-list.";

pub(crate) fn run(
    workspace: &save::Workspace,
    document: &PlanDocument,
    input: Option<&InputQueue>,
    name: &str,
    arguments: &str,
) -> Result<(Value, Option<PlanChange>)> {
    match name {
        "write" => {
            let args: WriteArgs =
                serde_json::from_str(arguments).context("invalid write arguments")?;
            ensure_writable(document)?;
            write(workspace, document, args)
        }
        "plan_submit" => {
            let args: PlanSubmitArgs =
                serde_json::from_str(arguments).context("invalid plan_submit arguments")?;
            ensure_writable(document)?;
            submit(workspace, document, input, args)
        }
        _ => bail!("Tool is not available in Plan mode."),
    }
}

/// Plans 中只有重新进入讨论的 Draft 可以保存或提交。
fn ensure_writable(document: &PlanDocument) -> Result<()> {
    ensure!(
        document.location != PlanLocation::Plans || document.status == plan::PlanStatus::Draft,
        "This plan is awaiting pre-execution review; choose a draft from /plan-list to plan again."
    );
    Ok(())
}

/// Opens the current plan file through the safe NOFOLLOW handle and verifies
/// the on-disk bytes still match the registered snapshot; an external edit is
/// surfaced, never clobbered. Shared by write and plan_submit.
pub(super) fn verified_target(
    workspace: &save::Workspace,
    path: &str,
    expected: &[u8],
) -> Result<save::Target> {
    let target = workspace.target(Path::new(path), false)?;
    let disk = target.read_bounded(plan::PLAN_MAX_BYTES)?;
    ensure!(
        disk == expected,
        "Plan file changed externally; review it before writing."
    );
    Ok(target)
}

#[cfg(test)]
mod fixtures {
    use super::*;
    use crate::tools::plan_catalog;

    pub(super) fn plan_document(revision: u64, title: &str, content: &str) -> PlanDocument {
        let mut document = PlanDocument::new().unwrap();
        document.revision = revision;
        document.title = title.to_owned();
        document.content = content.to_owned();
        document.origin_prompt = "original task".to_owned();
        if revision >= 1 {
            document.file_name = Some("create-canary.md".to_owned());
            document.location = PlanLocation::Drafts;
        }
        document
    }

    pub(super) fn write_args(file_name: &str, content: &str) -> WriteArgs {
        WriteArgs {
            path: plan::path_for(file_name, PlanLocation::Drafts),
            content: content.to_owned(),
        }
    }

    /// 把已登记条目写入登记表（模拟另一会话的登记）。
    pub(super) fn register(
        workspace: &save::Workspace,
        id: &str,
        file_name: &str,
        location: PlanLocation,
    ) -> plan_catalog::PlanCatalog {
        let _lock = plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = plan_catalog::load_catalog(workspace).unwrap();
        let mut catalog = snapshot.catalog;
        catalog.entries.push(plan_catalog::PlanCatalogEntry {
            id: id.to_owned(),
            file_name: file_name.to_owned(),
            location,
            deleted: false,
            deleted_file_identity: None,
        });
        plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
        catalog
    }

    /// Registered Plans entry plus matching on-disk bytes for one document.
    pub(super) fn plan_todo_registered(
        workspace: &save::Workspace,
        document: &PlanDocument,
        content: &str,
    ) {
        {
            let _lock = plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let mut catalog = snapshot.catalog;
            catalog.entries.push(plan_catalog::PlanCatalogEntry {
                id: document.id.clone(),
                file_name: document.file_name.clone().unwrap(),
                location: PlanLocation::Plans,
                deleted: false,
                deleted_file_identity: None,
            });
            plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
        }
        let target = workspace
            .target(std::path::Path::new(&document.path().unwrap()), true)
            .unwrap();
        target.save_new(content.as_bytes()).unwrap();
    }
}
