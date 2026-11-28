use super::{Database, warn};
use serde_json::Value;
use std::path::Path;

/// Collection status of a tool execution; errors and durations are written only after the runtime
/// actually runs the tool.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Ok,
    Error,
    Cancelled,
}

impl ToolStatus {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One tool execution record. `request_key` links the model request that started the call; without
/// it the call can only be aggregated per tool.
pub struct ToolCall<'a> {
    pub request_key: Option<&'a str>,
    pub session_file: &'a Path,
    pub call_id: &'a str,
    pub tool: &'a str,
    pub status: ToolStatus,
    pub error_message: Option<&'a str>,
    pub timestamp: i64,
    pub duration_ms: f64,
    /// Character count of the result text fed back to the model; None for history or uncollected
    /// results, and the body itself is never stored.
    pub result_chars: Option<u64>,
}

/// Character count of tool call arguments: strings count characters, null is 0 and everything else
/// counts the JSON text, matching the previous aggregation.
fn args_chars(args: &Value) -> u64 {
    match args {
        Value::String(args) => args.chars().count() as u64,
        Value::Null => 0,
        args => args.to_string().chars().count() as u64,
    }
}

/// Extracts the tool calls issued by this request from the native model output; calls are not
/// deduplicated by id.
///
/// Chat reads `tool_calls`; Responses reads the items with `type == "function_call"` in the output
/// array (or the `output` array).
pub(super) fn call_tools(output: &Value) -> Vec<(String, u64)> {
    if let Some(calls) = output["tool_calls"].as_array() {
        return calls
            .iter()
            .filter_map(|call| {
                let name = call["function"]["name"].as_str()?;
                Some((name.to_owned(), args_chars(&call["function"]["arguments"])))
            })
            .collect();
    }
    output
        .as_array()
        .or_else(|| output["output"].as_array())
        .into_iter()
        .flatten()
        .filter(|call| call["type"] == "function_call")
        .filter_map(|call| {
            let name = call["name"].as_str()?;
            Some((name.to_owned(), args_chars(&call["arguments"])))
        })
        .collect()
}

/// Extracts tool calls from a committed session entry snapshot in a legacy database; only used during
/// migration.
///
/// Matches the pre-migration aggregation order: blocks with `type == "toolCall"` in
/// `message.content` come first; [`maintain`] falls back to the model output of [`call_tools`] when
/// the snapshot is empty.
pub(super) fn saved_tool_calls(entry: &Value) -> Vec<(String, u64)> {
    entry
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|entry| entry["message"]["content"].as_array().into_iter().flatten())
        .filter(|call| call["type"] == "toolCall")
        .filter_map(|call| {
            let name = call["name"].as_str()?;
            Some((name.to_owned(), args_chars(&call["arguments"])))
        })
        .collect()
}

/// Records one tool execution; reuses the connection in the handle, and a storage failure only warns
/// without changing the tool result.
pub fn tool(database: &Database, call: &ToolCall<'_>) {
    if database.with_store(|store| store.tool(call)).is_err() {
        warn();
    }
}
