//! 编辑结果与 applyEdit 回调：建议结果、格式化建议与多服务器冲突。

use crate::lsp::Registry;
use crate::lsp::client::Client;
use crate::lsp::edits;
use crate::lsp::types::TextEdit;
use crate::lsp::types::WorkspaceEdit;
use crate::tools::Tools;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Instant;

use pp_core::cancellation::CancellationToken;

use super::servers::{real_servers_for, resolve_path};
use super::{Action, Params, deny_service, done, is_control, with_server, with_useless};

pub(super) fn edit_is_empty(edit: &WorkspaceEdit) -> bool {
    let no_changes = edit
        .changes
        .as_ref()
        .is_none_or(|changes| changes.values().all(Vec::is_empty));
    let no_document_changes = edit
        .document_changes
        .as_ref()
        .is_none_or(|changes| changes.is_empty());
    no_changes && no_document_changes
}

pub(super) fn parse_workspace_edit(value: &Value) -> Result<WorkspaceEdit> {
    if value.is_null() {
        return Ok(WorkspaceEdit::default());
    }
    serde_json::from_value(value.clone())
        .context("the language server returned an invalid workspace edit")
}

pub(super) fn suggestion_result(
    tools: &Tools,
    client: &Client,
    action: Action,
    edit: &WorkspaceEdit,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let suggestion = edits::suggest(tools, client, edit, cancellation)?;
    let mut result = with_server(
        done(
            action,
            true,
            format!(
                "Modification suggestions only; no files were changed. Read files again before using edit/write/bash.\n{}",
                serde_json::to_string_pretty(&suggestion)?
            ),
        ),
        &client.name,
    );
    result["details"]["applied"] = json!(false);
    result["details"]["suggestion"] = suggestion;
    result["details"]["complete"] = json!(true);
    if edit_is_empty(edit) {
        result = with_useless(result);
    }
    Ok(result)
}

pub(super) fn format_suggestions(
    tools: &Tools,
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let path = resolve_path(registry.cwd(), params.file()?);
    ensure!(params.file()? != "*", "format requires a concrete file");
    let mut results = Vec::new();
    let mut failures = Vec::new();
    for definition in real_servers_for(registry, &path) {
        let outcome = (|| -> Result<Value> {
            let client = registry.ensure_client(&definition.name, deadline, cancellation)?;
            ensure!(
                client.capabilities.document_formatting,
                "Formatting is unsupported"
            );
            let mut deny = deny_service();
            let edit = client.operation(deadline,cancellation,&mut deny,|op| {
                let state = client.open_or_refresh(op,&path)?;
                let value = op.request("textDocument/formatting",json!({"textDocument":{"uri":state.uri},"options":crate::lsp::format_options::resolve(&path,&state.text)}))?;
                let edits: Vec<TextEdit> = if value.is_null() {Vec::new()} else {serde_json::from_value(value).context("Invalid formatting edits")?};
                Ok(WorkspaceEdit{changes:None,document_changes:Some(vec![json!({"textDocument":{"uri":state.uri,"version":state.version},"edits":edits})])})
            })?;
            suggestion_result(tools, &client, Action::Format, &edit, cancellation)
        })();
        match outcome {
            Ok(value)=>results.push(value),
            Err(error) if is_control(&error)=>return Err(error),
            Err(error)=>failures.push(json!({"server":definition.name,"error":pp_core::logging::diagnostic_text(&format!("{error:#}"))})),
        }
    }
    let conflicts = suggestion_conflicts(&results);
    let mut result = done(
        Action::Format,
        !results.is_empty() && failures.is_empty(),
        format!(
            "Formatting suggestions only; no files were changed.\n{}",
            serde_json::to_string_pretty(&results)?
        ),
    );
    result["details"]["complete"] =
        json!(!results.is_empty() && failures.is_empty() && conflicts.is_empty());
    result["details"]["conflicts"] = json!(conflicts);
    result["details"]["applied"] = json!(false);
    result["details"]["suggestions"] = json!(results);
    result["details"]["failures"] = json!(failures);
    Ok(result)
}

/// 多服务器候选按文件比较，仅报告冲突，不选优先级或合并编辑。
pub(super) fn suggestion_conflicts(suggestions: &[Value]) -> Vec<Value> {
    let mut seen: BTreeMap<&str, (&Value, &Value)> = BTreeMap::new();
    let mut conflicts = Vec::new();
    for suggestion in suggestions {
        let details = &suggestion["details"];
        let server = &details["serverName"];
        if let Some(files) = details["suggestion"]["files"].as_array() {
            for file in files {
                if let Some(path) = file["path"].as_str() {
                    if let Some((prior, owner)) = seen.get(path) {
                        if prior["before"] != file["before"] || prior["after"] != file["after"] {
                            conflicts.push(json!({"path":path,"servers":[owner,server]}));
                        }
                    } else {
                        seen.insert(path, (file, server));
                    }
                }
            }
        }
    }
    conflicts
}
