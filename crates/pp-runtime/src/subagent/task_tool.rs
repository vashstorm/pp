//! Schema, argument structures and call entry point of the `task` tool.

use anyhow::{Context, Result, bail};
use async_openai::types::chat::FunctionObject;
use pp_core::cancellation::CancellationToken;
use pp_core::plan::WorkMode;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Binding, function};

pub(super) const TASK_ARGUMENTS_GUIDANCE: &str = "The top-level JSON object accepts only context (string) and tasks (array of 1-32 objects), both required. Every tasks[] item requires role and task; role must be from the current tool schema. Put role, task, name, readonly, isolated and output_schema inside tasks[] items, never at the top level. Use the tasks array even for a single task.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Batch {
    pub(super) context: String,
    pub(super) tasks: Vec<Task>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Task {
    pub(super) role: String,
    pub(super) task: String,
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) isolated: bool,
    #[serde(default)]
    pub(super) readonly: bool,
    pub(super) output_schema: Option<Value>,
}

impl Binding {
    pub(crate) fn definitions(&self, mode: WorkMode) -> Vec<FunctionObject> {
        if self.depth != 0
            || self.depth >= self.inner.settings.max_recursion_depth
            || self.spawns.is_empty()
            || self.readonly
        {
            return Vec::new();
        }
        let roles = self.model_roles();
        let custom = self.custom_roles();
        let available = self.available_roles(&roles, &custom).collect::<Vec<_>>();
        if available.is_empty() {
            return Vec::new();
        }
        let names = available
            .iter()
            .map(|(def, _)| def.name.as_str())
            .collect::<Vec<_>>();
        let help = available
            .iter()
            .map(|(def, _)| format!("{}: {}", def.name, def.description))
            .collect::<Vec<_>>()
            .join("; ");
        let example = json!({
            "context": "Workspace, relevant files, constraints, and required evidence.",
            "tasks": [{"role": names[0], "task": "Investigate the assigned question and report file references."}]
        });
        let plan_mode = mode == WorkMode::Plan;
        let mode_note = if plan_mode {
            "Plan-mode children are always read-only."
        } else {
            "Pass the returned `wait_args` to `wait` when blocked; `agent://<id>` works with `read` (latest result) and `write` (follow-up)."
        };
        let mut item_properties = json!({
            "name": {"type":"string", "description":"Optional unique name within this batch."},
            "role": {"type":"string", "enum":names, "description":"Required role from the available enum."},
            "task": {"type":"string", "description":"Required assignment: scope, expected output, and verification."},
            "output_schema": {"description":"Optional JSON Schema for structured final output."}
        });
        if !plan_mode {
            item_properties["isolated"] =
                json!({"type":"boolean", "description":"Run in an isolated git worktree."});
            item_properties["readonly"] =
                json!({"type":"boolean", "description":"Restrict this child to read-only tools."});
        }
        vec![function(
            "task",
            &format!(
                "Delegate independent work to subagents (always wrap even a single item in `tasks[]`). Children see only `context` + their `task`, not the parent conversation. Runs are asynchronous unless a custom role sets `blocking: true`; final results return automatically. {mode_note}\nRoles: {help}.\nExample: {example}"
            ),
            json!({
                "context": {"type":"string", "description":"Shared background for every child (files, constraints, prior findings)."},
                "tasks": {
                    "type":"array", "minItems":1, "maxItems":32,
                    "description":"1-32 task objects (`role` and `task` inside each item).",
                    "items": {
                        "type":"object",
                        "properties": item_properties,
                        "required":["task","role"], "additionalProperties":false
                    }
                }
            }),
            &["context", "tasks"],
        )]
    }

    pub(crate) fn execute(
        &self,
        name: &str,
        arguments: &str,
        plan: bool,
        token: &CancellationToken,
    ) -> Result<Value> {
        match name {
            "task" => {
                let args: Batch = serde_json::from_str(arguments).with_context(|| {
                    format!("Invalid task arguments. {TASK_ARGUMENTS_GUIDANCE} No tasks were started; correct the arguments before retrying.")
                })?;
                self.spawn(args, plan, token)
            }
            _ => bail!("unknown subagent tool"),
        }
    }
}
