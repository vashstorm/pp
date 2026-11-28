//! Edit results and the applyEdit callback: suggestion results, formatting suggestions and multi-server conflicts.

use crate::Registry;
use crate::client::Client;
use crate::edits;
use crate::types::TextEdit;
use crate::types::WorkspaceEdit;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Instant;

use pp_core::cancellation::CancellationToken;

use super::servers::{real_servers_for, resolve_path};
use super::{
    Action, LspToolHost, Params, deny_service, done, is_control, with_server, with_useless,
};

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
    host: &dyn LspToolHost,
    client: &Client,
    action: Action,
    edit: &WorkspaceEdit,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let suggestion = edits::suggest(host.workspace(), client, edit, cancellation)?;
    let summary = render_suggestion_summary(&suggestion);
    let mut result = with_server(
        done(
            action,
            true,
            format!(
                "Modification suggestions only; no files were changed. Read files again before using edit/write/bash.\n{summary}"
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
    host: &dyn LspToolHost,
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
                let value = op.request("textDocument/formatting",json!({"textDocument":{"uri":state.uri},"options":crate::format_options::resolve(&path,&state.text)}))?;
                let edits: Vec<TextEdit> = if value.is_null() {Vec::new()} else {serde_json::from_value(value).context("Invalid formatting edits")?};
                Ok(WorkspaceEdit{changes:None,document_changes:Some(vec![json!({"textDocument":{"uri":state.uri,"version":state.version},"edits":edits})])})
            })?;
            suggestion_result(host, &client, Action::Format, &edit, cancellation)
        })();
        match outcome {
            Ok(value)=>results.push(value),
            Err(error) if is_control(&error)=>return Err(error),
            Err(error)=>failures.push(json!({"server":definition.name,"error":pp_core::logging::diagnostic_text(&format!("{error:#}"))})),
        }
    }
    let conflicts = suggestion_conflicts(&results);
    let summary = render_format_summary(&results, &conflicts, &failures);
    let mut result = done(
        Action::Format,
        !results.is_empty() && failures.is_empty(),
        format!("Formatting suggestions only; no files were changed.\n{summary}"),
    );
    result["details"]["complete"] =
        json!(!results.is_empty() && failures.is_empty() && conflicts.is_empty());
    result["details"]["conflicts"] = json!(conflicts);
    result["details"]["applied"] = json!(false);
    result["details"]["suggestions"] = json!(results);
    result["details"]["failures"] = json!(failures);
    Ok(result)
}

/// Render a read-only suggestion as compact diff text (without the full `before`/`after` payloads or redundant JSON).
fn render_suggestion_summary(suggestion: &Value) -> String {
    use pp_core::events::{DiffLineKind, FileDiff};

    let mut sections = Vec::new();
    if let Some(summary) = suggestion.get("summary").and_then(Value::as_array) {
        let items: Vec<&str> = summary.iter().filter_map(Value::as_str).collect();
        if !items.is_empty() {
            sections.push(format!("Summary:\n- {}", items.join("\n- ")));
        }
    }
    if let Some(warnings) = suggestion.get("warnings").and_then(Value::as_array) {
        let items: Vec<&str> = warnings.iter().filter_map(Value::as_str).collect();
        if !items.is_empty() {
            sections.push(format!("Warnings:\n- {}", items.join("\n- ")));
        }
    }
    if let Some(files) = suggestion.get("files").and_then(Value::as_array) {
        for file in files {
            let path = file["path"].as_str().unwrap_or("<unknown>");
            let mut block = Vec::new();
            if let Some(resource) = file.get("resource").and_then(Value::as_str) {
                block.push(format!("File: {path} ({resource})"));
            } else {
                block.push(format!("File: {path}"));
            }
            if let Ok(diff) = serde_json::from_value::<FileDiff>(file["diff"].clone()) {
                match diff {
                    FileDiff::Text {
                        created,
                        deleted,
                        added,
                        removed,
                        hunks,
                    } => {
                        let mut flags = Vec::new();
                        if created {
                            flags.push("created");
                        }
                        if deleted {
                            flags.push("deleted");
                        }
                        let flag_str = if flags.is_empty() {
                            String::new()
                        } else {
                            format!(" [{}]", flags.join(", "))
                        };
                        block.push(format!("  (+{added} -{removed}{flag_str})"));
                        for hunk in hunks {
                            let start_old = hunk
                                .lines
                                .iter()
                                .find_map(|line| line.old_line)
                                .unwrap_or(0);
                            let start_new = hunk
                                .lines
                                .iter()
                                .find_map(|line| line.new_line)
                                .unwrap_or(0);
                            block.push(format!("@@ -{start_old} +{start_new} @@"));
                            for line in hunk.lines {
                                let prefix = match line.kind {
                                    DiffLineKind::Context => ' ',
                                    DiffLineKind::Added => '+',
                                    DiffLineKind::Removed => '-',
                                };
                                let text = line.text.trim_end_matches(['\r', '\n']);
                                block.push(format!("{prefix}{text}"));
                            }
                        }
                    }
                    FileDiff::Unavailable { reason } => {
                        block.push(format!("  (diff unavailable: {reason})"));
                    }
                }
            }
            sections.push(block.join("\n"));
        }
    }
    if sections.is_empty() {
        "No changes suggested.".to_owned()
    } else {
        sections.join("\n\n")
    }
}

fn render_format_summary(results: &[Value], conflicts: &[Value], failures: &[Value]) -> String {
    let mut sections = Vec::new();
    for item in results {
        let server = item["details"]["serverName"].as_str().unwrap_or("unknown");
        let suggestion = &item["details"]["suggestion"];
        sections.push(format!(
            "Server: {server}\n{}",
            render_suggestion_summary(suggestion)
        ));
    }
    if !conflicts.is_empty() {
        let lines: Vec<String> = conflicts
            .iter()
            .map(|c| {
                let path = c["path"].as_str().unwrap_or("<unknown>");
                format!("- Conflict on {path}: {}", c["servers"])
            })
            .collect();
        sections.push(format!("Conflicts:\n{}", lines.join("\n")));
    }
    if !failures.is_empty() {
        let lines: Vec<String> = failures
            .iter()
            .map(|f| {
                format!(
                    "- {}: {}",
                    f["server"].as_str().unwrap_or("unknown"),
                    f["error"].as_str().unwrap_or("failed")
                )
            })
            .collect();
        sections.push(format!("Failures:\n{}", lines.join("\n")));
    }
    if sections.is_empty() {
        "No formatting suggestions produced.".to_owned()
    } else {
        sections.join("\n\n")
    }
}

/// Candidates from several servers are compared per file; only conflicts are reported, without picking a priority or merging edits.
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

#[cfg(test)]
mod tests {
    use super::render_suggestion_summary;
    use serde_json::json;

    #[test]
    fn render_suggestion_summary_emits_compact_diff_without_full_before_after_bodies() {
        let huge_unchanged = "unchanged_context_sentinel_line\n".repeat(40);
        let before = format!("{huge_unchanged}let old_name = 1;\n");
        let after = format!("{huge_unchanged}let new_name = 1;\n");
        let diff = pp_workspace::diff::build(Some(&before), &after);
        let suggestion = json!({
            "summary": ["1 file changed"],
            "warnings": [],
            "files": [{
                "path": "src/lib.rs",
                "before": before,
                "after": after,
                "diff": diff,
            }]
        });
        let rendered = render_suggestion_summary(&suggestion);
        assert!(rendered.contains("File: src/lib.rs"), "{rendered}");
        assert!(rendered.contains("-let old_name = 1;"), "{rendered}");
        assert!(rendered.contains("+let new_name = 1;"), "{rendered}");
        assert!(
            rendered.matches("unchanged_context_sentinel_line").count() <= 4,
            "summary must not inline full before/after file bodies: {rendered}"
        );
    }
}
