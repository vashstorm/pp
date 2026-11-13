//! 一次 todo 调用的路由分类与分发。
use anyhow::Result;
use pp_core::plan::PlanDocument;
use pp_core::plan::PlanStatus;
use serde_json::Value;

use super::{ToolPolicy, Tools, todo};

/// How this todo call is classified for tracing and dispatch; no task
/// content is ever logged.
enum TodoRoute<'a> {
    /// Execute+Executed with a bound plan: the plan adapter with checkbox
    /// write-back.
    Executing(&'a PlanDocument),
    /// A plan document still bound via todo_plan_id but not executing
    /// (draft/ready under Execute, or a finished plan): read-only, except an
    /// explicit or inferred `init`, which detaches into a standalone list.
    Readonly(&'a PlanDocument, &'static str),
    /// No plan binding applies: ordinary standalone list.
    Standalone,
}

impl Tools {
    /// Route one todo call by the borrowed workflow state. The policy gate
    /// has already admitted the call; classification only decides which
    /// adapter runs. Detached standalone lists are never re-bound by an old
    /// plan document, and bound finished plans stay read-only unless the
    /// call starts a new standalone list.
    pub(super) fn run_todo(
        &self,
        policy: &ToolPolicy<'_>,
        arguments: &str,
    ) -> Result<(Value, Option<todo::TodoChange>)> {
        match self.todo_route(policy, arguments) {
            TodoRoute::Executing(document) => {
                let ToolPolicy::Execute { workflow } = policy else {
                    unreachable!("executing plans only occur under Execute policy");
                };
                tracing::debug!(event = "tool_todo", classification = "plan");
                todo::plan_adapter::run_plan(&self.workspace, document, &workflow.todos, arguments)
            }
            TodoRoute::Readonly(document, message) => {
                tracing::debug!(event = "tool_todo", classification = "readonly");
                todo::plan_adapter::run_readonly(document, arguments, message)
            }
            TodoRoute::Standalone => {
                let ToolPolicy::Execute { workflow } = policy else {
                    unreachable!("standalone todos only occur under Execute policy");
                };
                tracing::debug!(event = "tool_todo", classification = "standalone");
                let (value, change) = todo::run(&workflow.todos, arguments);
                // `change` carries plan: None; installing it detaches any
                // finished-plan binding without touching PlanDocument.
                Ok((value, change))
            }
        }
    }

    fn todo_route<'a>(&self, policy: &'a ToolPolicy<'a>, arguments: &str) -> TodoRoute<'a> {
        match policy {
            ToolPolicy::Plan { .. } | ToolPolicy::Review => {
                unreachable!("todo is gated out of planning and review")
            }
            ToolPolicy::Execute { workflow } => match workflow.plan.as_ref() {
                Some(document) if document.status == PlanStatus::Executed => {
                    TodoRoute::Executing(document)
                }
                Some(document)
                    if workflow.todo_plan_id.as_deref() == Some(document.id.as_str()) =>
                {
                    // Explicit or inferred `init` starts a standalone list
                    // and clears the binding at the commit boundary.
                    let starts_standalone = matches!(
                        todo::resolve_params(arguments, !workflow.todos.is_empty()),
                        Ok(params) if params.op == todo::TodoOperation::Init
                    );
                    if starts_standalone {
                        TodoRoute::Standalone
                    } else {
                        let message = match document.status {
                            PlanStatus::Completed | PlanStatus::Abandoned => {
                                "Plan todos are read-only; initialize a new standalone list."
                            }
                            _ => "Plan todos are read-only before execution.",
                        };
                        TodoRoute::Readonly(document, message)
                    }
                }
                _ => TodoRoute::Standalone,
            },
        }
    }
}
