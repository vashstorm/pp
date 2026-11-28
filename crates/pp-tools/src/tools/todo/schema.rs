//! `todo` tool schema and description.
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
Manage the session task checklist (batch with real tool calls; never spend a solo turn). Identify tasks by verbatim text (`view` if forgotten, never invented IDs).
Use `init` for 3+ steps or explicit task lists (`list` for phased, `items` for flat). Ops: `init`, `append` (`phase` + `items`), `start` (single `task`), `done` (after verification), `drop` (abandon), `rm` (delete), `block` (with `reason`, external blockers only), `unblock`, `view`. `done`/`drop`/`rm`/`block`/`unblock` accept `task` or `phase` (all items in phase). Mutations preserve other active items and auto-start the next pending item when none remain; `blocked` never auto-starts.";

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
                "description": "Operation to perform.",
            },
            "list": {
                "type": "array",
                "description": "Phased task list for `init`.",
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
            "task": {"type": "string", "description": "Verbatim task text."},
            "phase": {"type": "string", "description": "Phase name (target for `append`, optional disambiguator or batch target)."},
            "items": {
                "type": "array",
                "items": {"type": "string"},
                "description": "Task list for flat `init` or `append`.",
            },
            "reason": {"type": "string", "description": "Blocker note for `block`."},
            "occurrence": {
                "type": "integer",
                "minimum": 1,
                "description": "1-based index among identical task texts (after optional `phase` filter).",
            },
        },
        "required": ["op"],
        "additionalProperties": false,
    })
}
