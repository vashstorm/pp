//! rename：符号重命名建议。

use crate::lsp::Registry;
use crate::lsp::client::Client;
use crate::lsp::types::WorkspaceEdit;
use crate::tools::Tools;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Instant;

use pp_core::cancellation::CancellationToken;

use super::position::position_json;
use super::servers::{display_path, real_servers_for, resolve_path, wait_for_project};
use super::suggestion::{parse_workspace_edit, suggestion_result};
use super::{Action, Params, deny_service, done, project_aware, with_server};

/// 在已持有的 operation 里发起 `textDocument/rename`，不重新进入 operation。
fn rename_request(
    op: &mut crate::lsp::client::Op<'_>,
    client: &Client,
    path: &Path,
    line: u32,
    symbol: Option<&str>,
    new_name: &str,
) -> Result<WorkspaceEdit> {
    let state = client.open_or_refresh(op, path)?;
    let position = position_json(&state.text, line, symbol)?;
    let value = op.request(
        "textDocument/rename",
        json!({
            "textDocument": {"uri": state.uri},
            "position": position,
            "newName": new_name,
        }),
    )?;
    parse_workspace_edit(&value)
}

pub(super) fn rename(
    tools: &Tools,
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = Action::Rename;
    let file = params.file()?;
    ensure!(file != "*", "rename requires a concrete file");
    let new_name = params
        .new_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .context("rename requires `new_name`")?
        .to_owned();
    let path = resolve_path(registry.cwd(), file);
    let definitions = real_servers_for(registry, &path);
    let Some(definition) = definitions.first().cloned() else {
        return Ok(done(
            action,
            false,
            format!(
                "No language server covers {}.",
                display_path(registry.cwd(), &path)
            ),
        ));
    };
    let client = registry.ensure_client(&definition.name, deadline, cancellation)?;
    if !client.capabilities.rename {
        return Ok(with_server(
            done(
                action,
                false,
                format!("{} does not support rename.", definition.name),
            ),
            &definition.name,
        ));
    }
    let aware = project_aware(&definition);
    if let Some(line) = params.line
        && params.symbol.as_deref().is_none_or(str::is_empty)
        && aware
    {
        bail!(
            "symbol is required for project-aware rename on line {line}; pass symbol=<name>, optionally name#N"
        );
    }
    wait_for_project(&client, &definition, action, &path, deadline, cancellation)?;
    let mut service = deny_service();
    let edit = client.operation(deadline, cancellation, &mut service, |op| {
        rename_request(
            op,
            &client,
            &path,
            params.line.unwrap_or(1),
            params.symbol.as_deref(),
            &new_name,
        )
    })?;
    suggestion_result(tools, &client, action, &edit, cancellation)
}
