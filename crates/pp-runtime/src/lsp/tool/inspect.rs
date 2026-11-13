//! status / capabilities / reload：服务器状态查询与重连。

use crate::lsp::Registry;
use anyhow::Result;
use serde_json::Value;
use std::collections::BTreeSet;
use std::time::Instant;

use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::check;

use super::servers::{all_real_servers, display_path, real_servers_for, resolve_path};
use super::{Action, Params, done, is_control, with_server};

pub(super) fn status(registry: &Registry) -> Result<Value> {
    let action = Action::Status;
    if let Some(error) = registry.config_error() {
        return Ok(done(
            action,
            false,
            format!(
                "LSP configuration is invalid, so no language server can start until it is fixed:\n{error}"
            ),
        ));
    }
    let discovered = registry.discovered();
    let mut lines = vec![format!(
        "Language servers for {}:",
        registry.cwd().display()
    )];
    if discovered.is_empty() {
        let known = registry.definitions().len();
        lines.push("  none available".to_owned());
        if known > 0 {
            lines.push(format!(
                "  {known} built-in definition(s) are known, but none match this workspace or their executables are not on PATH."
            ));
        }
    } else {
        for definition in &discovered {
            let label = match registry.state(&definition.name) {
                crate::lsp::ClientState::NotStarted => "not started".to_owned(),
                crate::lsp::ClientState::Initializing => "initializing".to_owned(),
                crate::lsp::ClientState::Ready => "ready".to_owned(),
                crate::lsp::ClientState::Failed(reason) => format!("failed: {reason}"),
            };
            lines.push(format!("  {}: {label}", definition.name));
        }
    }
    let configured: Vec<String> = discovered.iter().map(|d| d.name.clone()).collect();
    for client in registry.started() {
        if !configured.iter().any(|name| name == &client.name) {
            lines.push(format!(
                "  {}: ready (started under a previous configuration)",
                client.name
            ));
        }
    }
    if !discovered.is_empty() {
        lines.push(
            "Note: 'not started' means the server was discovered but no request has started it yet; \
             'ready' means a live process exists for this workspace."
                .to_owned(),
        );
    }
    let server = configured.join(", ");
    Ok(with_server(done(action, true, lines.join("\n")), &server))
}

pub(super) fn capabilities(
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = Action::Capabilities;
    let definitions = match params.file.as_deref().filter(|file| *file != "*") {
        Some(file) => {
            let path = resolve_path(registry.cwd(), file);
            let servers = real_servers_for(registry, &path);
            if servers.is_empty() {
                return Ok(done(
                    action,
                    false,
                    format!(
                        "No language server is configured for {}.",
                        display_path(registry.cwd(), &path)
                    ),
                ));
            }
            servers
        }
        None => all_real_servers(registry),
    };
    if definitions.is_empty() {
        return Ok(done(
            action,
            false,
            "No language server is available for this workspace.".to_owned(),
        ));
    }
    let mut sections = Vec::new();
    let mut responded = Vec::new();
    for definition in &definitions {
        check(cancellation)?;
        match registry.ensure_client(&definition.name, deadline, cancellation) {
            Ok(client) => {
                responded.push(definition.name.clone());
                let capabilities = client.capabilities_json();
                let pretty = serde_json::to_string_pretty(&capabilities)
                    .unwrap_or_else(|_| capabilities.to_string());
                sections.push(format!(
                    "{}:\n  capabilities: {}",
                    definition.name,
                    pretty.replace('\n', "\n  ")
                ));
            }
            Err(error) if is_control(&error) => return Err(error),
            Err(error) => sections.push(format!(
                "{}: failed to start ({})",
                definition.name,
                pp_core::logging::diagnostic_text(&format!("{error:#}"))
            )),
        }
    }
    let success = !responded.is_empty();
    Ok(with_server(
        done(action, success, sections.join("\n")),
        &responded.join(", "),
    ))
}

pub(super) fn reload(
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    registry.reload_config()?;
    let definitions = match params.file.as_deref().filter(|file| *file != "*") {
        Some(file) => real_servers_for(registry, &resolve_path(registry.cwd(), file)),
        None => all_real_servers(registry),
    };
    let mut notes = Vec::new();
    let mut success = !definitions.is_empty();
    let mut evicted = BTreeSet::new();
    for definition in &definitions {
        if let Some(client) = registry.client(&definition.name)
            && evicted.insert(client.identity.clone())
        {
            registry.evict(&definition.name, &client)?;
        }
    }
    for definition in definitions {
        registry.clear_failure(&definition.name);
        match registry.ensure_client(&definition.name, deadline, cancellation) {
            Ok(_) => notes.push(format!("Reconnected {}", definition.name)),
            Err(error) if is_control(&error) => return Err(error),
            Err(error) => {
                success = false;
                notes.push(pp_core::logging::diagnostic_text(&format!(
                    "{}: {error:#}",
                    definition.name
                )));
            }
        }
    }
    Ok(done(Action::Reload, success, notes.join("\n")))
}
