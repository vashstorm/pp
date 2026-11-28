//! Read-only suggestion entry point: move validation and virtual preview, committing no modification.

use crate::types::WorkspaceEdit;
use anyhow::Result;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::check;
use pp_workspace::diff::Original;
use pp_workspace::save::Workspace;
use serde_json::Value;
use std::path::Path;

use super::plan;

/// Validate the workspace boundary, node types and target conflicts of a file move suggestion without creating directories.
pub(crate) fn validate_move(
    workspace: &Workspace,
    source: &Path,
    dest: &Path,
    cancellation: &CancellationToken,
) -> Result<()> {
    let edit = WorkspaceEdit {
        changes: None,
        document_changes: Some(vec![
            serde_json::json!({"kind":"rename","oldUri":crate::uri::path_to_uri(source),"newUri":crate::uri::path_to_uri(dest)}),
        ]),
    };
    plan(workspace, None, &edit, cancellation).map(|_| ())
}

/// Read-only suggestion: preserves the original edit order and versions, and returns the baseline and final virtual content.
pub(crate) fn suggest(
    workspace: &Workspace,
    client: &crate::Client,
    edit: &WorkspaceEdit,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let plan = plan(workspace, Some(client), edit, cancellation)?;
    check(cancellation)?;
    let files: Vec<Value> = plan
        .records
        .iter()
        .map(|(path, record)| {
            let before = match &record.before {
                Original::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
                _ => None,
            };
            let after = record
                .after
                .as_ref()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
            serde_json::json!({"path":path,"before":before,"after":after,
            "resource":record.resource,
            "diff":pp_workspace::diff::committed(&record.before,record.after.as_deref())})
        })
        .collect();
    Ok(
        serde_json::json!({"applied":false,"workspace_edit":edit,"files":files,
        "summary":plan.summary,"warnings":plan.warnings,
        "coordinates":"Original LSP ranges are zero-based UTF-16 positions.",
        "instruction":"Advice only. Read target files again and use edit/write/bash. No files were changed."}),
    )
}
