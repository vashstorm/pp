//! 导航与 hover：definition / typeDefinition / implementation / references / hover。

use crate::lsp::Registry;
use anyhow::{Result, bail, ensure};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::check;

use super::format::{
    extract_hover_text, format_location, format_location_with_context, is_only_queried_declaration,
    parse_locations,
};
use super::position::position_json;
use super::servers::{display_path, real_servers_for, resolve_path, wait_for_project};
use super::{Action, Params, deny_service, done, project_aware, with_server, with_useless};

/// 显式 `symbol` 是必需的动作（project-aware 服务器上）。
const SYMBOL_REQUIRED_ACTIONS: &[&str] = &["definition", "references", "rename"];

/// `references` 带上下文展示的条数上限。
const REFERENCE_CONTEXT_LIMIT: usize = 50;
/// project-aware 服务器只返回声明时额外查询次数与间隔。
const REFERENCES_RETRY_COUNT: u32 = 2;
const REFERENCES_RETRY_DELAY: Duration = Duration::from_millis(250);

pub(super) fn navigation(
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = params.action;
    let file = params.file()?;
    ensure!(file != "*", "{} requires a concrete file", action.as_str());
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
    let aware = project_aware(&definition);
    let supported = match action {
        Action::Definition => client.capabilities.definition,
        Action::TypeDefinition => client.capabilities.type_definition,
        Action::Implementation => client.capabilities.implementation,
        Action::References => client.capabilities.references,
        Action::Hover => client.capabilities.hover,
        _ => false,
    };
    if !supported {
        return Ok(with_server(
            done(
                action,
                false,
                format!(
                    "{} does not support {} in this workspace.",
                    definition.name,
                    method_for(action)
                ),
            ),
            &definition.name,
        ));
    }
    if let Some(line) = params.line
        && params.symbol.as_deref().is_none_or(str::is_empty)
        && aware
        && SYMBOL_REQUIRED_ACTIONS.contains(&action.as_str())
    {
        bail!(
            "symbol is required for project-aware {} on line {line}; pass symbol=<name>, optionally name#N for the Nth occurrence",
            action.as_str()
        );
    }
    wait_for_project(&client, &definition, action, &path, deadline, cancellation)?;
    let line = params.line.unwrap_or(1);
    let mut service = deny_service();
    let (content, useless) =
        client.operation(deadline, cancellation, &mut service, |op| {
            let state = client.open_or_refresh(op, &path)?;
            let position = position_json(&state.text, line, params.symbol.as_deref())?;
            let uri = state.uri.clone();
            match action {
                Action::Definition | Action::TypeDefinition | Action::Implementation => {
                    let method = match action {
                        Action::Definition => "textDocument/definition",
                        Action::TypeDefinition => "textDocument/typeDefinition",
                        _ => "textDocument/implementation",
                    };
                    let value = op.request(
                        method,
                        json!({"textDocument": {"uri": uri}, "position": position}),
                    )?;
                    let locations = parse_locations(&value)?;
                    if locations.is_empty() {
                        return Ok((empty_navigation(action), true));
                    }
                    let lines: Vec<String> = locations
                        .iter()
                        .map(|location| format_location_with_context(registry.cwd(), location))
                        .collect();
                    Ok((
                        format!(
                            "Found {} {}:\n{}",
                            locations.len(),
                            noun_for(action),
                            lines.join("\n")
                        ),
                        false,
                    ))
                }
                Action::References => {
                    let mut locations = Vec::new();
                    for attempt in 0..=REFERENCES_RETRY_COUNT {
                        let value = op.request(
                            "textDocument/references",
                            json!({
                                "textDocument": {"uri": uri},
                                "position": position,
                                "context": {"includeDeclaration": true},
                            }),
                        )?;
                        locations = parse_locations(&value)?;
                        if !aware || attempt == REFERENCES_RETRY_COUNT {
                            break;
                        }
                        if !locations.is_empty()
                            && !is_only_queried_declaration(&locations, &uri, &position)
                        {
                            break;
                        }
                        check(cancellation)?;
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        std::thread::sleep(REFERENCES_RETRY_DELAY.min(deadline - now));
                    }
                    if locations.is_empty() {
                        return Ok(("No references found".to_owned(), true));
                    }
                    let mut lines: Vec<String> = locations
                        .iter()
                        .take(REFERENCE_CONTEXT_LIMIT)
                        .map(|location| format_location_with_context(registry.cwd(), location))
                        .collect();
                    if locations.len() > REFERENCE_CONTEXT_LIMIT {
                        lines.push(format!(
                            "  ... {} additional reference(s) shown without context",
                            locations.len() - REFERENCE_CONTEXT_LIMIT
                        ));
                        lines.extend(locations.iter().skip(REFERENCE_CONTEXT_LIMIT).map(
                            |location| format!("  {}", format_location(registry.cwd(), location)),
                        ));
                    }
                    Ok((
                        format!(
                            "Found {} reference(s):\n{}",
                            locations.len(),
                            lines.join("\n")
                        ),
                        false,
                    ))
                }
                Action::Hover => {
                    let value = op.request(
                        "textDocument/hover",
                        json!({"textDocument": {"uri": uri}, "position": position}),
                    )?;
                    let contents = value.get("contents").cloned().unwrap_or(Value::Null);
                    let text = extract_hover_text(&contents);
                    if text.trim().is_empty() {
                        return Ok(("No hover information".to_owned(), true));
                    }
                    Ok((text, false))
                }
                _ => unreachable!("navigation dispatches only navigation actions"),
            }
        })?;
    let value = with_server(done(action, true, content), &definition.name);
    Ok(if useless { with_useless(value) } else { value })
}

fn method_for(action: Action) -> &'static str {
    match action {
        Action::Definition => "definitions",
        Action::TypeDefinition => "type definitions",
        Action::Implementation => "implementations",
        Action::References => "references",
        Action::Hover => "hover information",
        _ => "this request",
    }
}

fn noun_for(action: Action) -> &'static str {
    match action {
        Action::Definition => "definition(s)",
        Action::TypeDefinition => "type definition(s)",
        Action::Implementation => "implementation(s)",
        _ => "result(s)",
    }
}

fn empty_navigation(action: Action) -> String {
    match action {
        Action::Definition => "No definition found".to_owned(),
        Action::TypeDefinition => "No type definition found".to_owned(),
        Action::Implementation => "No implementation found".to_owned(),
        _ => "No results found".to_owned(),
    }
}
