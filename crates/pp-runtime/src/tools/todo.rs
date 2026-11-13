//! 普通 todo 工具：沿用 oh-my-pi 的 schema、诊断与 summary，支持多个进行项。
//!
//! 参数解析/目标定位（`resolve_params`/`resolve`）与按序数的状态转换
//! （`apply_operation`）刻意分离：普通路径先按完整原文定位出
//! [`TaskOrdinal`]，计划适配器后续直接传入按文档顺序解析出的全局序数，
//! 不再通过首条同文案匹配定位。

use pp_core::todo::TodoPhase;
use serde_json::{Value, json};

mod params;
mod projection;
mod schema;
mod state;
mod summary;

pub(crate) use params::{PlanTodoChange, TodoChange, TodoOperation, TodoParams, resolve_params};
pub(crate) use projection::project_plan_todos;
pub(in crate::tools) use schema::definition;
use state::resolve;
pub(crate) use state::{
    Operation, TaskOrdinal, apply_operation, locate_task_matches, normalize_in_progress,
    normalize_reason,
};
pub use summary::format_summary;

/// Standalone todo entry: parses arguments against the strict schema, resolves
/// targets by verbatim content, and transforms a single transaction snapshot.
/// Success returns `{ok,op,content,phases}` plus the effect; any failure
/// returns `{ok:false,error,op?,content,phases}` with the pre-call state and
/// discards every modification.
pub(crate) fn run(current: &[TodoPhase], arguments: &str) -> (Value, Option<TodoChange>) {
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
