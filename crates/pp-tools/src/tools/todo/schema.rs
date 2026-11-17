//! `todo` 工具 schema 与描述。
use async_openai::types::chat::FunctionObject;
use serde_json::{Value, json};

const TOOL_NAME: &str = "todo";
pub(super) const OPERATIONS: &[&str] = &[
    "init", "start", "done", "rm", "drop", "block", "unblock", "append", "view",
];
pub(super) const ALLOWED_FIELDS: &[&str] = &[
    "op",
    "list",
    "task",
    "phase",
    "items",
    "reason",
    "occurrence",
];

const TODO_DESCRIPTION: &str = "\
`todo` NEVER takes a turn of its own: emit it in the same response as your next real tool call (parallel tool calls), `init` beside the first read or action, `done`/`start` beside the next action or check. A response whose only call is `todo` wastes a full model round trip.
Tasks identified by verbatim content, NEVER generated IDs (task-1). Unique, stable task/phase names; lost text: view, NEVER guess.
Before work, init for 3+ steps, requested task sets, or new instructions. MUST list EVERY user item separately (phased/numbered/bulleted/N); NEVER omit or remember leftovers.
After successful mutation: preserve all active items; if none remain, earliest pending starts (phase order). Start changes only the addressed task, so call start for each actually running parallel item. Blocked NEVER starts automatically; unblock returns pending. Done out of order NEVER reopens completed work. Mark done immediately after verification; schedule work by dependencies rather than checklist order.
Before wait, reconcile existing todos with observed progress: mark every actually running item in progress, mark verified work done, and record real blockers. A running delegated task is not blocked merely because the parent is waiting for its result; never mark it done at dispatch or solely from a child success report. Block only work that cannot proceed until an external prerequisite is resolved, with an optional reason. Unblock when actionable; append a clearing task for agent-actionable blockers only in standalone lists, never in an approved plan.";

pub(in crate::tools) fn definition() -> FunctionObject {
    FunctionObject {
        name: TOOL_NAME.into(),
        description: Some(TODO_DESCRIPTION.into()),
        parameters: Some(schema_with_ops(OPERATIONS)),
        ..Default::default()
    }
}

fn schema_with_ops(ops: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": {
            "op": {
                "type": "string",
                "enum": ops,
                "description": "todo operation",
            },
            "list": {
                "type": "array",
                "description": "phases for init",
                "items": {
                    "type": "object",
                    "properties": {
                        "phase": {"type": "string"},
                        "items": {"type": "array", "items": {"type": "string"}, "minItems": 1},
                    },
                    "required": ["phase", "items"],
                    "additionalProperties": false,
                },
            },
            "task": {"type": "string", "description": "verbatim task content"},
            "phase": {"type": "string"},
            "items": {
                "type": "array",
                "items": {"type": "string"},
                "description": "tasks for flat init or append",
            },
            "reason": {"type": "string", "description": "blocker note for block"},
            "occurrence": {
                "type": "integer",
                "minimum": 1,
                "description": "one-based pick among identical task texts after optional phase filtering; supported for standalone and plan tasks",
            },
        },
        "required": ["op"],
        "additionalProperties": false,
    })
}
