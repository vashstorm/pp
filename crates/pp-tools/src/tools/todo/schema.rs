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
Batch `todo` in parallel with real tool calls (never waste a solo turn). Identify tasks by verbatim content (`view` if forgotten, never generated IDs).
`init` for 3+ steps or explicit task lists (list every user item separately). Mutations preserve other active items (auto-start earliest pending if none remain); `start` parallel items explicitly; `blocked` never auto-starts; mark `done` only after verification.
Before `wait`, reconcile todos (`start` running items, mark verified `done`, `block` only external prerequisites—not running subagents).";

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
