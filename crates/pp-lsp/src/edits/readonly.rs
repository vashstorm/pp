//! 只读建议入口：移动校验与虚拟预览，不提交任何修改。

use crate::types::WorkspaceEdit;
use anyhow::Result;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::check;
use pp_workspace::diff::Original;
use pp_workspace::save::Workspace;
use serde_json::Value;
use std::path::Path;

use super::plan;

/// 校验文件移动建议的工作区边界、节点类型和目标冲突，不创建目录。
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

/// 只读建议：保留原始编辑顺序与版本，并返回基线和最终虚拟内容。
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
