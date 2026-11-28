//! Plain todo tool: reuses the oh-my-pi schema, diagnostics and summary, and supports multiple in-progress items.
//!
//! Argument parsing/target resolution (`resolve_params`/`resolve`) is deliberately separate from ordinal-based
//! state transitions (`apply_operation`): the plain path first resolves a [`TaskOrdinal`] from the complete
//! original text, while the plan adapter later passes the global ordinal resolved in document order directly,
//! instead of locating the item by matching the first occurrence of the same text.

use pp_core::todo::TodoPhase;
use serde_json::{Value, json};

mod params;
mod projection;
mod schema;
mod state;
mod summary;

pub use params::{PlanTodoChange, TodoChange, TodoOperation, TodoParams, resolve_params};
pub use projection::project_plan_todos;
pub(in crate::tools) use schema::definition;
use state::resolve;
pub use state::{
    Operation, TaskOrdinal, apply_operation, locate_task_matches, normalize_in_progress,
    normalize_reason,
};
pub use summary::format_summary;

/// Standalone todo entry: parses arguments against the strict schema, resolves
/// targets by verbatim content, and transforms a single transaction snapshot.
/// Success returns `{ok,op,content,phases}` plus the effect; any failure
/// returns `{ok:false,error,op?,content,phases}` with the pre-call state and
/// discards every modification.
pub fn run(current: &[TodoPhase], arguments: &str) -> (Value, Option<TodoChange>) {
    let has_existing = !current.is_empty();
    let params = match resolve_params(arguments, has_existing) {
        Ok(params) => params,
        Err(message) => {
            // Schema failures echo the error text, with op attached when the
            // raw arguments name a known operation.
            let op = identifiable_op(arguments);
            return (failure_value(current, op, message.clone(), message), None);
        }
    };
    let operation = match resolve(current, &params) {
        Ok(operation) => operation,
        Err(errors) => {
            let content = format_summary(current, &errors, params.op == TodoOperation::View);
            return (
                failure_value(
                    current,
                    Some(params.op.as_str()),
                    errors.join("; "),
                    content,
                ),
                None,
            );
        }
    };
    if operation.is_view() {
        // Pure reads: no normalization, no clone, no change.
        let content = format_summary(current, &[], true);
        let phases = serde_json::to_value(current).unwrap_or_default();
        return (
            json!({"ok": true, "op": "view", "content": content, "phases": phases}),
            None,
        );
    }

    let mut next = apply_operation(current, &operation);
    normalize_in_progress(&mut next);
    let content = format_summary(&next, &[], false);
    let phases = serde_json::to_value(&next).unwrap_or_default();
    let change = TodoChange {
        phases: next,
        plan: None,
    };
    (
        json!({"ok": true, "op": params.op.as_str(), "content": content, "phases": phases}),
        Some(change),
    )
}

fn identifiable_op(arguments: &str) -> Option<&'static str> {
    let value: Value = serde_json::from_str(arguments).ok()?;
    value
        .get("op")?
        .as_str()
        .and_then(TodoOperation::parse)
        .map(TodoOperation::as_str)
}

/// pp result JSON for a failed call: pre-call state, error text, and op when
/// identifiable. Shared with the plan adapter for identical failure shapes.
pub(crate) fn failure_value(
    current: &[TodoPhase],
    op: Option<&'static str>,
    error: String,
    content: String,
) -> Value {
    let mut value = json!({
        "ok": false,
        "error": error,
        "content": content,
        "phases": serde_json::to_value(current).unwrap_or_default(),
    });
    if let Some(op) = op {
        value["op"] = Value::String(op.into());
    }
    value
}

/// pp result JSON for a successful call: `{ok,op,content,phases}`. The plan
/// adapter layers `plan` info on top of this shape.
pub(crate) fn success_value(op: &str, content: String, phases: &[TodoPhase]) -> Value {
    json!({
        "ok": true,
        "op": op,
        "content": content,
        "phases": serde_json::to_value(phases).unwrap_or_default(),
    })
}

pub(super) mod plan_adapter;

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests;
