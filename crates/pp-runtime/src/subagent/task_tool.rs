//! `task` 工具的 schema、参数结构与调用入口。

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
    pub(crate) fn definitions(&self, _mode: WorkMode) -> Vec<FunctionObject> {
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
            .map(|(def, role)| {
                let label = format!("{}: {}", def.name, def.description);
                match role.and_then(|role| roles.get(&role)) {
                    Some(assignment) => format!(
                        "{label} ({}:{})",
                        assignment.model,
                        assignment.effort.as_str()
                    ),
                    None => label,
                }
            })
            .collect::<Vec<_>>()
            .join("; ");
        let review_help = if names.contains(&"review") {
            " Select tasks[].role=review for read-only code review."
        } else {
            ""
        };
        let example = json!({
            "context": "Current workspace, relevant files, constraints and required evidence.",
            "tasks": [{"role": names[0], "task": "Investigate the assigned question and report file references."}]
        });
        vec![function(
            "task",
            &format!(
                "Delegate independent work to subagents. {TASK_ARGUMENTS_GUIDANCE}\nExample arguments: {example}\nAvailable roles: {help}.{review_help}\nTo wait for this batch, pass the returned wait_args object as the complete arguments to wait. Subagent run_id values go in wait.run_ids; agent_id and output_uri are for read/write. wait.task_ids is only for background Bash, not this tool.\n\nChildren receive the shared context and their own task, not the parent conversation. Include all information needed to work independently. Tasks are asynchronous unless the custom role sets blocking=true. Final results automatically return to you; continue independent work and use wait when blocked. Plan-mode children are always read-only.\n\nFor an existing TODO list, identify the items each child covers; the parent owns checklist updates. After dispatch and before wait, use todo start for every actually running item and reconcile verified completions and real blockers. Multiple items may be in progress concurrently. Do not start queued or dependency-blocked items, mark delegation as completion, or block a running child merely because the parent awaits its result. Plan mode must not call todo."
            ),
            json!({
                "context": {"type":"string", "description":"Shared background for every child; include relevant files, constraints and prior findings. Children do not inherit the parent conversation."},
                "tasks": {
                    "type":"array", "minItems":1, "maxItems":32,
                    "description":"Task objects. Even one task must be wrapped in this array; role and task belong inside each item.",
                    "items": {
                        "type":"object",
                        "properties": {
                            "name": {"type":"string", "description":"Optional non-empty name, unique within this batch."},
                            "role": {"type":"string", "enum":names, "description":"Required role for this task item. Select a value from this enum."},
                            "task": {"type":"string", "description":"Required non-empty assignment: scope, expected result and verification."},
                            "isolated": {"type":"boolean", "description":"Use an isolated worktree. Not allowed in Plan mode."},
                            "readonly": {"type":"boolean", "description":"Limit this task to read-only investigation without delegation."},
                            "output_schema": {"description":"Optional JSON Schema for the child's final structured result."}
                        },
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
