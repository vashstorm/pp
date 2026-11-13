//! 只读建议入口：移动校验与虚拟预览，不提交任何修改。

use crate::lsp::types::WorkspaceEdit;
use crate::tools::Tools;
use crate::tools::changes::Original;
use anyhow::Result;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::check;
use serde_json::Value;
use std::path::Path;

use super::plan;

/// 校验文件移动建议的工作区边界、节点类型和目标冲突，不创建目录。
pub(crate) fn validate_move(
    tools: &Tools,
    source: &Path,
    dest: &Path,
    cancellation: &CancellationToken,
) -> Result<()> {
    let edit = WorkspaceEdit {
        changes: None,
        document_changes: Some(vec![
            serde_json::json!({"kind":"rename","oldUri":crate::lsp::uri::path_to_uri(source),"newUri":crate::lsp::uri::path_to_uri(dest)}),
        ]),
    };
    plan(tools, None, &edit, cancellation).map(|_| ())
}

/// 只读建议：保留原始编辑顺序与版本，并返回基线和最终虚拟内容。
pub(crate) fn suggest(
    tools: &Tools,
    client: &crate::lsp::Client,
    edit: &WorkspaceEdit,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let plan = plan(tools, Some(client), edit, cancellation)?;
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
            "diff":crate::tools::diff::committed(&record.before,record.after.as_deref())})
        })
        .collect();
    Ok(
        serde_json::json!({"applied":false,"workspace_edit":edit,"files":files,
        "summary":plan.summary,"warnings":plan.warnings,
        "coordinates":"Original LSP ranges are zero-based UTF-16 positions.",
        "instruction":"Advice only. Read target files again and use edit/write/bash. No files were changed."}),
    )
}
