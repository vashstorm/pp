//! code_actions: candidate enumeration, resolve and quick-fix context.

use crate::Registry;
use crate::client::Client;
use crate::types::Command;
use crate::types::Diagnostic;
use crate::types::WorkspaceEdit;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::path::Path;
use std::time::{Duration, Instant};

use pp_core::cancellation::CancellationToken;

use super::position::position_json;
use super::servers::{display_path, real_servers_for, resolve_path};
use super::suggestion::suggestion_result;
use super::{
    Action, LspToolHost, Params, deny_service, done, is_control, with_server, with_useless,
};

/// Wait budget for the diagnostics context `code_actions` supplies to quick fixes.
const CODE_ACTION_DIAGNOSTICS_WAIT: Duration = Duration::from_secs(3);

enum CodeActionEntry {
    Command(Command),
    Action(ActionItem),
}

struct ActionItem {
    raw: Value,
    title: String,
    kind: Option<String>,
    preferred: bool,
    disabled: Option<String>,
    edit: Option<WorkspaceEdit>,
    command: Option<Command>,
}

impl CodeActionEntry {
    fn title(&self) -> &str {
        match self {
            Self::Command(command) => &command.title,
            Self::Action(item) => &item.title,
        }
    }
}

fn parse_action_entry(value: &Value) -> Result<CodeActionEntry> {
    let object = value
        .as_object()
        .context("the language server returned an invalid code action")?;
    if object.get("command").is_some_and(Value::is_string) {
        let command: Command = serde_json::from_value(value.clone())
            .context("the language server returned an invalid command")?;
        return Ok(CodeActionEntry::Command(command));
    }
    let action: crate::types::CodeAction = serde_json::from_value(value.clone())
        .context("the language server returned an invalid code action")?;
    let disabled = action.disabled.as_ref().map(|disabled| {
        disabled
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| "disabled by the language server".to_owned())
    });
    Ok(CodeActionEntry::Action(ActionItem {
        raw: value.clone(),
        title: action.title,
        kind: action.kind,
        preferred: object
            .get("isPreferred")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        disabled,
        edit: action.edit,
        command: action.command,
    }))
}

fn parse_code_actions(value: &Value) -> Result<Vec<CodeActionEntry>> {
    let items: Vec<&Value> = match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        _ => bail!("the language server returned an invalid code action list"),
    };
    items.into_iter().map(parse_action_entry).collect()
}

fn format_code_action(index: usize, entry: &CodeActionEntry) -> String {
    match entry {
        CodeActionEntry::Command(command) => {
            format!("{index}: [unsupported server command] {}", command.title)
        }
        CodeActionEntry::Action(item) => {
            let kind = item.kind.as_deref().unwrap_or("action");
            let preferred = if item.preferred { " (preferred)" } else { "" };
            let disabled = item
                .disabled
                .as_deref()
                .map(|reason| format!(" (disabled: {reason})"))
                .unwrap_or_default();
            format!("{index}: [{kind}] {}{preferred}{disabled}", item.title)
        }
    }
}

fn diagnostic_json(diagnostic: &Diagnostic) -> Value {
    let mut value = json!({
        "range": {
            "start": {
                "line": diagnostic.range.start.line,
                "character": diagnostic.range.start.character,
            },
            "end": {
                "line": diagnostic.range.end.line,
                "character": diagnostic.range.end.character,
            },
        },
        "message": diagnostic.message,
    });
    if let Some(severity) = diagnostic.severity {
        value["severity"] = json!(severity.0);
    }
    if let Some(code) = &diagnostic.code {
        value["code"] = code.clone();
    }
    if let Some(source) = &diagnostic.source {
        value["source"] = json!(source);
    }
    if !diagnostic.related_information.is_empty() {
        value["relatedInformation"] = json!(diagnostic.related_information);
    }
    if !diagnostic.tags.is_empty() {
        value["tags"] = json!(diagnostic.tags);
    }
    value
}

/// Quick-fix context for `code_actions`: prefer a cached report from the current generation whose version matches, otherwise do one
/// bounded collection; a diagnostics failure itself never blocks refactoring actions.
fn code_action_diagnostics(
    client: &Client,
    path: &Path,
    uri: &str,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Vec<Diagnostic>> {
    let expected = client.document(path).map(|state| state.version as u64);
    // Freshness (not invalidated, generation not advanced by refresh) uses the same decision as push reuse and `unchanged`;
    // only the additional requirement is that the cached report's version matches the current document or carries no version.
    if let Some(entry) = client.diagnostics.fresh_snapshot(uri)
        && entry
            .version
            .is_none_or(|version| Some(version) == expected)
    {
        return Ok(entry.diagnostics);
    }
    let budget = deadline.min(Instant::now() + CODE_ACTION_DIAGNOSTICS_WAIT);
    match client.collect_diagnostics(path, budget, cancellation) {
        Ok(outcome) => Ok(outcome.diagnostics.unwrap_or_default()),
        Err(error) if is_control(&error) => Err(error),
        Err(_) => Ok(Vec::new()),
    }
}

pub(super) fn code_actions(
    host: &dyn LspToolHost,
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let action = Action::CodeActions;
    let file = params.file()?;
    ensure!(file != "*", "code_actions requires a concrete file");
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
    if !client.capabilities.code_action {
        return Ok(with_server(
            done(
                action,
                false,
                format!("{} does not support code actions.", definition.name),
            ),
            &definition.name,
        ));
    }
    let query = params
        .query
        .as_deref()
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .map(str::to_owned);
    let line = params.line.unwrap_or(1);
    let mut service = deny_service();
    let state = client.operation(deadline, cancellation, &mut service, |op| {
        client.open_or_refresh(op, &path)
    })?;
    let diagnostics = code_action_diagnostics(&client, &path, &state.uri, deadline, cancellation)?;
    let diagnostics_json: Vec<Value> = diagnostics.iter().map(diagnostic_json).collect();
    let entries = client.operation(deadline, cancellation, &mut service, |op| {
        let state = client.open_or_refresh(op, &path)?;
        let position = position_json(&state.text, line, params.symbol.as_deref())?;
        let mut context = json!({
            "diagnostics": diagnostics_json,
            "triggerKind": 1,
        });
        if let Some(query) = &query {
            context["only"] = json!([query]);
        }
        let value = op.request(
            "textDocument/codeAction",
            json!({
                "textDocument": {"uri": state.uri},
                "range": {"start": position, "end": position},
                "context": context,
            }),
        )?;
        parse_code_actions(&value)
    })?;
    let selection = params.selection.as_deref();
    let list = selection.is_none();
    if list {
        if entries.is_empty() {
            return Ok(with_useless(with_server(
                done(action, true, "No code actions available".to_owned()),
                &definition.name,
            )));
        }
        let mut lines = vec![format!("{} code action(s):", entries.len())];
        lines.extend(
            entries
                .iter()
                .enumerate()
                .map(|(index, entry)| format!("  {}", format_code_action(index, entry))),
        );
        return Ok(with_server(
            done(action, true, lines.join("\n")),
            &definition.name,
        ));
    }
    let query = selection.expect("selection was checked above");
    let index = query.parse::<usize>().ok();
    let selected = match index {
        Some(index) => entries.get(index),
        None => {
            let needle = query.to_lowercase();
            entries
                .iter()
                .find(|entry| entry.title().to_lowercase().contains(&needle))
        }
    };
    let Some(selected) = selected else {
        let mut lines = vec![format!(
            "No code action matches \"{query}\". Available actions:"
        )];
        lines.extend(
            entries
                .iter()
                .enumerate()
                .map(|(index, entry)| format!("  {}", format_code_action(index, entry))),
        );
        return Ok(with_server(
            done(action, false, lines.join("\n")),
            &definition.name,
        ));
    };
    let selected_title = selected.title().to_owned();
    let mut item = match selected {
        CodeActionEntry::Command(command) => {
            return Ok(command_only(action, &definition.name, command));
        }
        CodeActionEntry::Action(item) => ActionItem {
            raw: item.raw.clone(),
            title: item.title.clone(),
            kind: item.kind.clone(),
            preferred: item.preferred,
            disabled: item.disabled.clone(),
            edit: item.edit.clone(),
            command: item.command.clone(),
        },
    };
    if let Some(reason) = &item.disabled {
        return Ok(with_server(
            done(
                action,
                false,
                format!("Action \"{selected_title}\" is disabled: {reason}"),
            ),
            &definition.name,
        ));
    }
    if item.edit.is_none()
        && client.capabilities.code_action_resolve
        && (item.command.is_none() || item.raw.get("data").is_some())
    {
        // resolve only fills in the edit; on failure an incomplete action must not be applied.
        let mut service = deny_service();
        let resolved = client.operation(deadline, cancellation, &mut service, |op| {
            op.request("codeAction/resolve", item.raw.clone())
        })?;
        item = match parse_action_entry(&resolved)? {
            CodeActionEntry::Action(resolved) => resolved,
            CodeActionEntry::Command(command) => {
                return Ok(command_only(action, &definition.name, &command));
            }
        };
        if let Some(reason) = &item.disabled {
            return Ok(with_server(
                done(
                    action,
                    false,
                    format!("Action \"{selected_title}\" is disabled: {reason}"),
                ),
                &definition.name,
            ));
        }
    }
    if item.edit.is_none() && item.command.is_none() {
        return Ok(with_server(
            done(
                action,
                false,
                format!("Action \"{selected_title}\" has no workspace edit or command to apply"),
            ),
            &definition.name,
        ));
    }
    let mut result = match &item.edit {
        Some(edit) => suggestion_result(host, &client, action, edit, cancellation)?,
        None => {
            return Ok(command_only(
                action,
                &definition.name,
                item.command.as_ref().context("Code action has no edit")?,
            ));
        }
    };
    result["details"]["title"] = json!(selected_title);
    if let Some(command) = item.command {
        result["details"]["command"] = json!(command);
        result["details"]["complete"] = json!(false);
        let content = result["content"].as_str().unwrap_or_default().to_owned();
        result["content"] = json!(format!(
            "{content}\nThis action also requires a server command; the edit may be incomplete. The command was not executed."
        ));
    }
    Ok(result)
}

fn command_only(action: Action, server: &str, command: &Command) -> Value {
    let mut result = with_server(
        done(
            action,
            false,
            format!(
                "{} requires a server command, which LSP will not execute. Use existing tools to implement the intended change.",
                command.title
            ),
        ),
        server,
    );
    result["details"]["applied"] = json!(false);
    result["details"]["command"] = json!(command);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use pp_config::config::LspSettings;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// Isolated fixture registry; logs stay in the temp directory for pull-count assertions.
    fn fixture_registry(dir: &Path) -> Registry {
        std::fs::create_dir_all(dir.join(".pp")).unwrap();
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp_server.py");
        let log = dir.join("requests.jsonl");
        std::fs::write(
            dir.join(".pp/lsp.json"),
            json!({"servers": {"fixture": {
                "command": "python3",
                "args": [script, "--log", log],
                "fileTypes": [".rs"],
                "rootMarkers": [".pp"],
            }}})
            .to_string(),
        )
        .unwrap();
        let config = config::Config::load_with(dir, None).unwrap();
        Registry::from_config(
            dir.to_path_buf(),
            LspSettings::default(),
            Ok(Arc::new(config)),
        )
    }

    fn pull_count(dir: &Path) -> usize {
        std::fs::read_to_string(dir.join("requests.jsonl"))
            .unwrap()
            .lines()
            .filter(|line| {
                serde_json::from_str::<Value>(line)
                    .is_ok_and(|value| value["method"] == "textDocument/diagnostic")
            })
            .count()
    }

    #[test]
    fn lsp_code_actions_diagnostics_are_refetched_after_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = std::fs::canonicalize(dir.path()).unwrap();
        let registry = fixture_registry(&cwd);
        let mut cleanup = registry.cleanup();
        let path = cwd.join("sample.rs");
        std::fs::write(&path, "wrong\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let token = CancellationToken::new();
        let client = registry.ensure_client("fixture", deadline, &token).unwrap();

        // publish: one pull collection publishes the report for the current version into the cache.
        let outcome = client.collect_diagnostics(&path, deadline, &token).unwrap();
        assert!(
            outcome
                .diagnostics
                .as_ref()
                .is_some_and(|items| !items.is_empty()),
            "the fixture must publish an issue for the broken file"
        );
        assert_eq!(pull_count(&cwd), 1);
        let uri = crate::uri::path_to_uri(&path);

        // refresh: the server asks for a recomputation, so reports from the old generation no longer count as fresh.
        client.diagnostics.refresh();
        // The on-disk content was fixed at the same time: reusing the old cache would keep feeding the old error into the quick-fix context.
        std::fs::write(&path, "fine\n").unwrap();

        let diagnostics = code_action_diagnostics(&client, &path, &uri, deadline, &token).unwrap();
        assert!(
            diagnostics.is_empty(),
            "code actions must see the refreshed file, not the stale cache: {diagnostics:?}"
        );
        assert_eq!(
            pull_count(&cwd),
            2,
            "a refreshed epoch must force a fresh diagnostic pull"
        );
        cleanup.stop().unwrap();
    }
}
