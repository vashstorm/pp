//! `lsp` tool: strict parsing of 14 actions, per-action gating and synchronous execution.
//!
//! Gating has two independent layers: the tool definition is trimmed by `Access` (a read-only session only sees read-only actions), and
//! `validate_access` and [`run`] check again after argument parsing, so hiding schema is never an
//! authorization boundary. `run` returns a pp tool result (`ok`/`content`/`details`); cancellation and cleanup
//! failures keep their typed error and propagate, while ordinary action failures are described by a full `ok:false` result.
//!
//! Every edit is a suggestion only; server applyEdit is always rejected, and the tool never commits file modifications.

mod code_actions;
mod diagnostics;
mod format;
mod inspect;
mod navigation;
mod position;
mod rename;
mod rename_file;
mod servers;
mod suggestion;
mod symbols;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Registry;
use crate::adapters;
use crate::config::ServerDefinition;
use crate::transport::Failure;
use crate::transport::ServerRequest;
use crate::transport::UNAUTHORIZED_EDIT;
use pp_workspace::Tier;
use pp_workspace::save::Workspace;

/// Narrow port between the `lsp` tool and the host, implemented by the tool collection.
pub trait LspToolHost {
    fn lsp_registry(&self) -> Option<&Registry>;
    fn lsp_owner_id(&self) -> &str;
    fn workspace(&self) -> &Workspace;
}
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::cancellation::CleanupError;
use pp_core::cancellation::check;
use pp_core::events::AgentEvent;

use code_actions::code_actions;
use diagnostics::diagnostics_action;
use inspect::{capabilities, reload, status};
use navigation::navigation;
use rename::rename;
use rename_file::rename_file;
use suggestion::format_suggestions;
use symbols::symbols;

/// The full set of actions allowed in a read-only session (Plan/Review, read-only subagents).
const READONLY_ACTIONS: &[&str] = &[
    "diagnostics",
    "definition",
    "type_definition",
    "implementation",
    "references",
    "hover",
    "symbols",
    "status",
    "capabilities",
    "rename",
    "rename_file",
    "code_actions",
    "format",
];

/// Lower bound of the call budget when no explicit `timeout` is given.
const MIN_CALL_BUDGET: u64 = 20;

/// Per-action access mode of the tool; `ReadOnly` applies at both the definition and execution layers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Execute,
    ReadOnly,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Diagnostics,
    Definition,
    References,
    Hover,
    Symbols,
    Rename,
    RenameFile,
    CodeActions,
    TypeDefinition,
    Implementation,
    Status,
    Reload,
    Capabilities,
    Format,
}

impl Action {
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "diagnostics" => Self::Diagnostics,
            "definition" => Self::Definition,
            "references" => Self::References,
            "hover" => Self::Hover,
            "symbols" => Self::Symbols,
            "rename" => Self::Rename,
            "rename_file" => Self::RenameFile,
            "code_actions" => Self::CodeActions,
            "type_definition" => Self::TypeDefinition,
            "implementation" => Self::Implementation,
            "status" => Self::Status,
            "reload" => Self::Reload,
            "capabilities" => Self::Capabilities,
            "format" => Self::Format,
            other => bail!("unknown lsp action `{other}`"),
        })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Diagnostics => "diagnostics",
            Self::Definition => "definition",
            Self::References => "references",
            Self::Hover => "hover",
            Self::Symbols => "symbols",
            Self::Rename => "rename",
            Self::RenameFile => "rename_file",
            Self::CodeActions => "code_actions",
            Self::TypeDefinition => "type_definition",
            Self::Implementation => "implementation",
            Self::Status => "status",
            Self::Reload => "reload",
            Self::Capabilities => "capabilities",
            Self::Format => "format",
        }
    }

    fn is_readonly(self) -> bool {
        READONLY_ACTIONS.contains(&self.as_str())
    }
}

/// Strict tool parameters; unknown fields are rejected and `line`/`timeout` are validated at parse time.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawParams {
    action: String,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    line: Option<u32>,
    #[serde(default)]
    symbol: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    new_name: Option<String>,
    #[serde(default)]
    apply: Option<bool>,
    #[serde(default)]
    timeout: Option<f64>,
    #[serde(default)]
    selection: Option<String>,
}

#[derive(Debug)]
struct Params {
    action: Action,
    file: Option<String>,
    line: Option<u32>,
    symbol: Option<String>,
    query: Option<String>,
    new_name: Option<String>,
    timeout: Option<f64>,
    selection: Option<String>,
}

impl Params {
    fn parse(arguments: &str) -> Result<Self> {
        let raw: RawParams = serde_json::from_str(arguments).context("invalid lsp arguments")?;
        let action = Action::parse(&raw.action)?;
        ensure!(
            raw.apply != Some(true),
            "LSP only returns information; apply=true is unsupported. Use edit/write/bash."
        );
        ensure!(
            raw.line != Some(0),
            "line must be a positive 1-based line number"
        );
        if let Some(timeout) = raw.timeout {
            ensure!(
                (5.0..=300.0).contains(&timeout),
                "timeout must be between 5 and 300 seconds"
            );
        }
        Ok(Self {
            action,
            file: raw.file,
            line: raw.line,
            symbol: raw.symbol,
            query: raw.query,
            new_name: raw.new_name,
            timeout: raw.timeout,
            selection: raw.selection,
        })
    }

    fn file(&self) -> Result<&str> {
        self.file
            .as_deref()
            .filter(|file| !file.trim().is_empty())
            .with_context(|| {
                format!(
                    "{} requires `file`; use \"*\" for workspace scope where supported",
                    self.action.as_str()
                )
            })
    }
}

/// `lsp` tool definition. A read-only session only sees the allowed actions, while the runtime gate still applies independently.
pub fn definition(read_only: bool) -> FunctionObject {
    let actions: Vec<&str> = if read_only {
        READONLY_ACTIONS.to_vec()
    } else {
        vec![
            "diagnostics",
            "definition",
            "type_definition",
            "implementation",
            "references",
            "hover",
            "symbols",
            "rename",
            "rename_file",
            "code_actions",
            "status",
            "reload",
            "capabilities",
            "format",
        ]
    };
    let mut description = String::from(
        "Query language servers for semantic navigation, diagnostics, and read-only modification suggestions (never applies edits or runs server commands; re-read and use edit/write/bash). Input lines are 1-based; returned suggestion ranges are 0-based UTF-16. Examples: {\"action\":\"symbols\",\"file\":\"*\",\"query\":\"Foo\"}, {\"action\":\"references\",\"file\":\"src/main.rs\",\"line\":42,\"symbol\":\"run\"}, {\"action\":\"code_actions\",\"file\":\"src/main.rs\",\"line\":42,\"query\":\"quickfix\"}.",
    );
    if read_only {
        description.push_str(" Read-only session: mutating actions are refused.");
    }
    FunctionObject {
        name: "lsp".into(),
        description: Some(description),
        parameters: Some(json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string", "enum": actions,
                    "description": "symbols (file outline or file='*' + query); definition/type_definition/implementation/references/hover (position); diagnostics (file or <=20-file glob, never '*'); rename/rename_file/code_actions/format (suggestions only); status/capabilities/reload (server lifecycle)."
                },
                "file": {
                    "type": "string",
                    "description": "Relative/absolute path (or '*' for workspace symbols; bounded glob for diagnostics; optional for capabilities/reload; omitted for status)."
                },
                "line": {
                    "type": "integer", "minimum": 1,
                    "description": "1-based line number for position-based actions (defaults to 1)."
                },
                "symbol": {
                    "type": "string",
                    "description": "Symbol text on `line` (use `name#N` for Nth occurrence); omit for first non-whitespace."
                },
                "query": {
                    "type": "string",
                    "description": "Required search text for symbols with file='*'; optional kind filter for code_actions (e.g. quickfix/refactor)."
                },
                "new_name": {
                    "type": "string",
                    "description": "Target identifier (rename) or destination path (rename_file)."
                },
                "timeout": {
                    "type": "number", "minimum": 5, "maximum": 300,
                    "description": "Wait budget in seconds (5..300)."
                },
                "selection": {
                    "type": "string",
                    "description": "code_actions: 0-based string index (e.g. '0') or title substring from prior candidate list."
                }
            },
            "required": ["action"],
            "additionalProperties": false
        })),
        ..Default::default()
    }
}

/// Pre-resolve the local target paths an `lsp` action will touch, so the caller can run sandbox read-boundary checks and path approval
/// before starting or querying an external language server. `status`/`reload`/`capabilities` have no concrete file, and `file == "*"` returns an empty set.
pub fn resolve_read_targets(
    cwd: &Path,
    arguments: &str,
    cancellation: &CancellationToken,
) -> Result<Vec<PathBuf>> {
    check(cancellation)?;
    let Ok(params) = Params::parse(arguments) else {
        return Ok(Vec::new());
    };
    let Some(file) = params
        .file
        .as_deref()
        .map(str::trim)
        .filter(|file| !file.is_empty())
    else {
        return Ok(Vec::new());
    };
    if file == "*" {
        return if matches!(
            params.action,
            Action::Status | Action::Reload | Action::Capabilities
        ) {
            Ok(Vec::new())
        } else {
            Ok(vec![cwd.to_path_buf()])
        };
    }
    let expand_tilde = |raw: &str| -> String {
        if let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            if raw == "~" {
                return PathBuf::from(home).to_string_lossy().into_owned();
            }
            if let Some(rest) = raw.strip_prefix("~/") {
                return PathBuf::from(home)
                    .join(rest)
                    .to_string_lossy()
                    .into_owned();
            }
        }
        raw.to_owned()
    };
    let expanded = expand_tilde(file);
    let mut targets = Vec::new();
    match params.action {
        Action::Diagnostics => {
            if crate::diagnostics::contains_glob(&expanded) {
                let (root, _) = crate::diagnostics::glob_scope(cwd, &expanded);
                targets.push(root);
            } else {
                targets.push(servers::resolve_path(cwd, &expanded));
            }
        }
        Action::RenameFile => {
            targets.push(servers::resolve_path(cwd, &expanded));
            if let Some(dest) = params
                .new_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                targets.push(servers::resolve_path(cwd, &expand_tilde(dest)));
            }
        }
        Action::Status | Action::Reload | Action::Capabilities => {
            targets.push(servers::resolve_path(cwd, &expanded));
        }
        Action::Definition
        | Action::TypeDefinition
        | Action::Implementation
        | Action::References
        | Action::Hover
        | Action::Symbols
        | Action::Rename
        | Action::CodeActions
        | Action::Format => {
            targets.push(servers::resolve_path(cwd, &expanded));
        }
    }
    Ok(targets)
}

/// Execution-layer access gate: a read-only session must be able to prove the action is read-only, otherwise it is rejected right after parsing.
pub fn validate_access(arguments: &str, access: Access) -> Result<()> {
    if access == Access::Execute {
        return Ok(());
    }
    let params = Params::parse(arguments)?;
    enforce_access(&params, access)
}

/// Permission tiering: read-only actions are Read; write actions (including previews) are Write; workspace diagnostics and invalid arguments are handled conservatively.
pub fn permission_tier(arguments: &str) -> Tier {
    let Ok(params) = Params::parse(arguments) else {
        return Tier::Write;
    };
    if params.action == Action::Diagnostics && params.file.is_none() {
        return Tier::Write;
    }
    if params.action.is_readonly() {
        Tier::Read
    } else {
        Tier::Write
    }
}

fn enforce_access(params: &Params, access: Access) -> Result<()> {
    if access == Access::Execute {
        return Ok(());
    }
    ensure!(
        params.action.is_readonly(),
        "LSP action {} is not available in a read-only session",
        params.action.as_str()
    );
    if params.action == Action::Diagnostics {
        ensure!(
            params.file.as_deref() != Some("*"),
            "workspace diagnostics run build commands and are not available in a read-only session"
        );
    }
    Ok(())
}

/// Execute one `lsp` call and return the pp tool result JSON.
pub fn run(
    host: &dyn LspToolHost,
    arguments: &str,
    _call_id: &str,
    access: Access,
    cancellation: &CancellationToken,
    _emit: &mut dyn FnMut(AgentEvent),
) -> Result<Value> {
    let params = Params::parse(arguments)?;
    enforce_access(&params, access)?;
    check(cancellation)?;
    let registry = host
        .lsp_registry()
        .context("LSP support is not enabled for this session.")?;
    ensure!(registry.enabled(), "LSP support is disabled.");
    if !matches!(params.action, Action::Status | Action::Reload) {
        // A configuration error is a reason explicit calls must see; status displays it as part of the status.
        let _ = registry.config()?;
    }
    let deadline = call_deadline(registry, params.timeout);
    match params.action {
        Action::Status => status(registry),
        Action::Capabilities => capabilities(registry, &params, deadline, cancellation),
        Action::Reload => reload(registry, &params, deadline, cancellation),
        Action::Diagnostics => {
            diagnostics_action(host, registry, &params, access, deadline, cancellation)
        }
        Action::Definition
        | Action::TypeDefinition
        | Action::Implementation
        | Action::References
        | Action::Hover => navigation(registry, &params, deadline, cancellation),
        Action::Symbols => symbols(registry, &params, deadline, cancellation),
        Action::Rename => rename(host, registry, &params, deadline, cancellation),
        Action::RenameFile => rename_file(host, registry, &params, deadline, cancellation),
        Action::CodeActions => code_actions(host, registry, &params, deadline, cancellation),
        Action::Format => format_suggestions(host, registry, &params, deadline, cancellation),
    }
}

fn call_deadline(registry: &Registry, timeout: Option<f64>) -> Instant {
    let seconds =
        timeout.unwrap_or_else(|| registry.settings().request_timeout.max(MIN_CALL_BUDGET) as f64);
    Instant::now() + Duration::from_secs_f64(seconds)
}

fn done(action: Action, success: bool, content: String) -> Value {
    json!({
        "ok": success,
        "content": content,
        "details": {"action": action.as_str(), "success": success},
    })
}

fn with_server(mut value: Value, server: &str) -> Value {
    if !server.is_empty() {
        value["details"]["serverName"] = json!(server);
    }
    value
}

fn with_useless(mut value: Value) -> Value {
    value["details"]["useless"] = json!(true);
    value
}

fn with_diagnostics(mut value: Value, diagnostics: Value) -> Value {
    value["diagnostics"] = diagnostics;
    value
}

fn is_control(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<Cancelled>() || cause.is::<CleanupError>())
}

fn failure_error(failure: Failure, what: &str) -> anyhow::Error {
    match failure {
        Failure::Cancelled => anyhow::Error::new(Cancelled),
        Failure::Timeout => anyhow::anyhow!("LSP {what} timed out"),
        Failure::Rpc(code) => anyhow::anyhow!("LSP {what} failed with error code {code}"),
        Failure::Closed => {
            anyhow::anyhow!("LSP {what} failed: the language server connection is closed")
        }
    }
}

/// Owning agent of explicit diagnostic reports: each subagent has its own, and no inbox is shared.
fn owner(host: &dyn LspToolHost) -> &str {
    host.lsp_owner_id()
}

fn deny_edit(request: ServerRequest) {
    request.respond(json!({
        "applied": false,
        "failureReason": UNAUTHORIZED_EDIT,
    }));
}

/// Server callback for the read-only scope: `workspace/applyEdit` is always rejected.
fn deny_service() -> impl FnMut(ServerRequest) {
    |request: ServerRequest| deny_edit(request)
}

fn project_aware(definition: &ServerDefinition) -> bool {
    !adapters::is_adapter(&definition.name) && !definition.is_linter
}

#[cfg(test)]
#[path = "tool/tests.rs"]
mod integration;
