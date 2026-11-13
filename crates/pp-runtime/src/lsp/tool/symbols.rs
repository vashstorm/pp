//! symbols：单文件符号大纲与工作区符号搜索。

use crate::lsp::Registry;
use crate::lsp::transport::Failure;
use crate::lsp::types::SymbolInformation;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::time::Instant;

use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::cancellation::check;

use super::format::{
    SymbolEntry, dedupe_workspace_symbols, filter_workspace_symbols, format_document_symbol,
    format_flat_symbol, parse_symbols,
};
use super::servers::{all_real_servers, display_path, real_servers_for, resolve_path};
use super::{
    Action, Params, deny_service, done, failure_error, is_control, with_server, with_useless,
};

/// 工作区符号结果上限。
const WORKSPACE_SYMBOL_LIMIT: usize = 200;

pub(super) fn symbols(
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = Action::Symbols;
    let file = params.file()?;
    if file == "*" {
        return workspace_symbols(registry, params, deadline, cancellation);
    }
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
    if !client.capabilities.document_symbol {
        return Ok(with_server(
            done(
                action,
                false,
                format!("{} does not support document symbols.", definition.name),
            ),
            &definition.name,
        ));
    }
    let mut service = deny_service();
    let symbols = client.operation(deadline, cancellation, &mut service, |op| {
        let state = client.open_or_refresh(op, &path)?;
        let value = op.request(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": state.uri}}),
        )?;
        parse_symbols(&value)
    })?;
    if symbols.is_empty() {
        return Ok(with_useless(with_server(
            done(action, true, "No symbols found".to_owned()),
            &definition.name,
        )));
    }
    let relative = display_path(registry.cwd(), &path);
    let mut lines = vec![format!("Symbols in {relative}:")];
    for symbol in &symbols {
        match symbol {
            SymbolEntry::Document(symbol) => lines.extend(format_document_symbol(symbol, 0)),
            SymbolEntry::Flat(symbol) => {
                lines.push(format!("  {}", format_flat_symbol(registry.cwd(), symbol)));
            }
        }
    }
    Ok(with_server(
        done(action, true, lines.join("\n")),
        &definition.name,
    ))
}

pub(super) fn workspace_symbols(
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = Action::Symbols;
    let query = params
        .query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .context("query is required for workspace symbol search (file=\"*\")")?
        .to_owned();
    let definitions = all_real_servers(registry);
    if definitions.is_empty() {
        return Ok(done(
            action,
            false,
            "No language server is available for workspace symbol search.".to_owned(),
        ));
    }
    let mut aggregated = Vec::new();
    let mut responded = Vec::new();
    let mut failures = Vec::new();
    for definition in &definitions {
        check(cancellation)?;
        let client = match registry.ensure_client(&definition.name, deadline, cancellation) {
            Ok(client) => client,
            Err(error) if is_control(&error) => return Err(error),
            Err(error) => {
                failures.push(format!(
                    "  {}: {}",
                    definition.name,
                    pp_core::logging::diagnostic_text(&format!("{error:#}"))
                ));
                continue;
            }
        };
        let mut service = deny_service();
        let outcome = client.operation(deadline, cancellation, &mut service, |op| {
            Ok(op.request_raw("workspace/symbol", json!({"query": query})))
        });
        let value = match outcome {
            Ok(Ok(value)) => {
                responded.push(definition.name.clone());
                value
            }
            Ok(Err(Failure::Cancelled)) => return Err(anyhow::Error::new(Cancelled)),
            Ok(Err(Failure::Rpc(-32601))) => continue,
            Ok(Err(failure)) => {
                failures.push(format!(
                    "  {}: {}",
                    definition.name,
                    failure_error(failure, "workspace/symbol")
                ));
                continue;
            }
            Err(error) if is_control(&error) => return Err(error),
            Err(error) => {
                failures.push(format!(
                    "  {}: {}",
                    definition.name,
                    pp_core::logging::diagnostic_text(&format!("{error:#}"))
                ));
                continue;
            }
        };
        let symbols: Vec<SymbolInformation> = match &value {
            Value::Null => Vec::new(),
            Value::Array(_) => serde_json::from_value(value.clone())
                .context("the language server returned an invalid workspace symbol list")?,
            _ => bail!("the language server returned an invalid workspace symbol list"),
        };
        aggregated.extend(filter_workspace_symbols(symbols, &query));
    }
    let failure_section = if failures.is_empty() {
        String::new()
    } else {
        format!("\nServer failures:\n{}", failures.join("\n"))
    };
    if responded.is_empty() {
        return Ok(with_server(
            done(
                action,
                false,
                format!(
                    "Workspace symbol search failed: all language servers failed{failure_section}"
                ),
            ),
            "",
        ));
    }
    let deduped = dedupe_workspace_symbols(aggregated);
    if deduped.is_empty() {
        return Ok(with_server(
            done(
                action,
                true,
                format!("No symbols matching \"{query}\"{failure_section}"),
            ),
            &responded.join(", "),
        ));
    }
    let limited: Vec<&SymbolInformation> = deduped.iter().take(WORKSPACE_SYMBOL_LIMIT).collect();
    let mut lines: Vec<String> = limited
        .iter()
        .map(|symbol| format!("  {}", format_flat_symbol(registry.cwd(), symbol)))
        .collect();
    if deduped.len() > WORKSPACE_SYMBOL_LIMIT {
        lines.push(format!(
            "\n[…{} symbol(s) elided…]",
            deduped.len() - WORKSPACE_SYMBOL_LIMIT
        ));
    }
    Ok(with_server(
        done(
            action,
            true,
            format!(
                "Found {} symbol(s) matching \"{query}\":\n{}{failure_section}",
                deduped.len(),
                lines.join("\n")
            ),
        ),
        &responded.join(", "),
    ))
}
