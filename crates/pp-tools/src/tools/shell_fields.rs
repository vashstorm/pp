//! UI display and redundant-copy stripping/restoring before model results are serialized.
//!
//! UI cards and `SessionJournal` still receive the full `Value`; only before building the JSON text fed back
//! to the model are fields the model does not need or that are already rendered taken aside, then restored in place after serialization to avoid cloning the whole tree.
use serde_json::Value;

pub(super) struct ModelProjection {
    shells: Vec<(Vec<String>, Value)>,
    top_fields: Vec<(&'static str, Value)>,
    rg_file_matches: Option<Value>,
}

impl ModelProjection {
    pub(super) fn strip(tool: &str, value: &mut Value) -> Self {
        let shells = if matches!(tool, "bash" | "bash_task" | "wait") {
            remove_shell_display(value)
        } else {
            Vec::new()
        };
        let mut top_fields = Vec::new();
        let mut rg_file_matches = None;

        if let Some(object) = value.as_object_mut() {
            let has_content = object.get("content").is_some_and(Value::is_string);
            match tool {
                "bash" | "bash_task" => {
                    // A running background task needs `pipes` kept even with no pipeline stages yet (it provides the `pipe://<run_id>` handle).
                    let running = object.get("status").and_then(Value::as_str) == Some("running");
                    if !running
                        && object.get("pipes").is_some_and(is_empty_pipes)
                        && let Some(pipes) = object.remove("pipes")
                    {
                        top_fields.push(("pipes", pipes));
                    }
                }
                "write" | "edit" => {
                    // Inline LSP diagnostics are already rendered by `content`, so drop the duplicate raw structured report object.
                    if has_content && let Some(diag) = object.remove("diagnostics") {
                        top_fields.push(("diagnostics", diag));
                    }
                }
                "lsp" => {
                    if has_content {
                        for key in ["diagnostics", "details"] {
                            if let Some(removed) = object.remove(key) {
                                top_fields.push((key, removed));
                            }
                        }
                    }
                }
                "web_search" => {
                    if has_content && let Some(details) = object.remove("details") {
                        top_fields.push(("details", details));
                    }
                }
                "todo" => {
                    if has_content && let Some(phases) = object.remove("phases") {
                        top_fields.push(("phases", phases));
                    }
                }
                "rg" => {
                    if has_content
                        && let Some(details) =
                            object.get_mut("details").and_then(Value::as_object_mut)
                        && matches!(
                            details.get("outputMode").and_then(Value::as_str),
                            Some("files_with_matches" | "count")
                        )
                    {
                        rg_file_matches = details.remove("fileMatches");
                    }
                }
                _ => {}
            }
        }

        Self {
            shells,
            top_fields,
            rg_file_matches,
        }
    }

    pub(super) fn restore(self, value: &mut Value) {
        for (path, shell) in self.shells {
            restore_shell_display(value, &path, shell);
        }
        if let Some(object) = value.as_object_mut() {
            for (key, field) in self.top_fields {
                object.insert(key.to_owned(), field);
            }
            if let Some(file_matches) = self.rg_file_matches
                && let Some(details) = object.get_mut("details").and_then(Value::as_object_mut)
            {
                details.insert("fileMatches".to_owned(), file_matches);
            }
        }
    }
}

fn is_empty_pipes(pipes: &Value) -> bool {
    let Some(object) = pipes.as_object() else {
        return false;
    };
    let no_pipelines = object
        .get("pipelines")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty);
    let no_omitted = object
        .get("omitted_pipelines")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        == 0;
    let no_error = object.get("capture_error").is_none_or(Value::is_null);
    let no_nonzero = object
        .get("nonzero_stages")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        == 0;
    let not_incomplete = object.get("incomplete").and_then(Value::as_bool) != Some(true);
    no_pipelines && no_omitted && no_error && no_nonzero && not_incomplete
}

/// Walks tool result objects only; same-named MCP fields do not go through this entry.
pub(super) fn remove_shell_display(value: &mut Value) -> Vec<(Vec<String>, Value)> {
    fn walk(value: &mut Value, path: &mut Vec<String>, saved: &mut Vec<(Vec<String>, Value)>) {
        match value {
            Value::Object(object) => {
                let display = object.get("shell").is_some_and(|shell| {
                    shell["version"].is_u64()
                        && shell["revision"].is_u64()
                        && shell["rows"].is_array()
                        && shell["running"].is_boolean()
                });
                if display && let Some(shell) = object.remove("shell") {
                    saved.push((path.clone(), shell));
                }
                for (key, child) in object {
                    path.push(key.clone());
                    walk(child, path, saved);
                    path.pop();
                }
            }
            Value::Array(values) => {
                for (index, child) in values.iter_mut().enumerate() {
                    path.push(index.to_string());
                    walk(child, path, saved);
                    path.pop();
                }
            }
            _ => {}
        }
    }
    let mut saved = Vec::new();
    walk(value, &mut Vec::new(), &mut saved);
    saved
}

pub(super) fn restore_shell_display(value: &mut Value, path: &[String], shell: Value) {
    let mut target = value;
    for key in path {
        target = if target.is_array() {
            &mut target[key
                .parse::<usize>()
                .expect("array paths are generated by the walker")]
        } else {
            &mut target[key.as_str()]
        };
    }
    target["shell"] = shell;
}

#[cfg(test)]
mod tests {
    use super::ModelProjection;
    use serde_json::json;

    #[test]
    fn model_projection_strips_redundant_fields_and_restores_verbatim_for_ui() {
        let cases = [
            (
                "bash",
                json!({
                    "ok": true,
                    "output": "hi\n",
                    "pipes": {"run_id":"r1","pipelines":[],"omitted_pipelines":0,"nonzero_stages":0,"incomplete":false},
                    "shell": {"version":1,"revision":1,"rows":[],"running":false}
                }),
                vec!["pipes", "shell"],
                vec!["output"],
            ),
            (
                "bash",
                json!({
                    "ok": true,
                    "output": "hi\n",
                    "pipes": {"run_id":"r1","pipelines":[{"id":1}],"omitted_pipelines":0,"nonzero_stages":0,"incomplete":false}
                }),
                vec![],
                vec!["pipes", "output"],
            ),
            (
                "write",
                json!({"ok": true, "content": "Diagnostics...", "diagnostics": {"servers":[]}}),
                vec!["diagnostics"],
                vec!["content"],
            ),
            (
                "lsp",
                json!({"ok": true, "content": "Found 1", "diagnostics": {}, "details": {"suggestion": {}}}),
                vec!["diagnostics", "details"],
                vec!["content"],
            ),
            (
                "web_search",
                json!({"ok": true, "content": "1. Result", "details": {"sources": [1, 2]}}),
                vec!["details"],
                vec!["content"],
            ),
            (
                "todo",
                json!({"ok": true, "content": "Overall: 1/2", "phases": [{"name":"P"}]}),
                vec!["phases"],
                vec!["content"],
            ),
        ];

        for (tool, original, stripped_keys, kept_keys) in cases {
            let mut value = original.clone();
            let projection = ModelProjection::strip(tool, &mut value);
            for key in stripped_keys {
                assert!(
                    value.get(key).is_none(),
                    "{tool} should strip `{key}` from model view: {value}"
                );
            }
            for key in kept_keys {
                assert!(
                    value.get(key).is_some(),
                    "{tool} should keep `{key}` in model view: {value}"
                );
            }
            projection.restore(&mut value);
            assert_eq!(value, original, "{tool} restore must be lossless");
        }
    }
}
