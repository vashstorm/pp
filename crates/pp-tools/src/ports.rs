//! Narrow port traits from `pp-tools` to the upper execution domain (hooks, input queue, subagents).

use anyhow::Result;
use async_openai::types::chat::FunctionObject;
use pp_core::cancellation::CancellationToken;
use pp_core::events::AgentEvent;
use pp_core::plan::WorkMode;
use serde_json::{Value, json};
use std::path::Path;

/// Behaviour that must abort this batch: `continue:false` or a hook failure that cannot be allowed through.
pub enum HookHalt {
    Stop(String),
    Failure(anyhow::Error),
}

pub struct PostToolCall<'a> {
    pub is_error: bool,
    pub name: &'a str,
    pub call_id: &'a str,
    pub arguments: &'a str,
    pub value: &'a Value,
}

pub struct PostToolOutcome {
    pub additional_context: Vec<String>,
    pub updated_output: Option<Value>,
    pub stop_reason: Option<String>,
}

/// Pre-tool hook decision for a nested tool execution.
#[derive(Default)]
pub struct PreToolOutcome {
    pub deny_reason: Option<String>,
    pub ask_reason: Option<String>,
    pub additional_context: Vec<String>,
    pub updated_arguments: Option<String>,
    pub stop_reason: Option<String>,
}

/// Hook context carried by a single tool execution.
pub trait ToolHookCall {
    fn deny_reason(&self) -> Option<&str>;
    fn ask_reason(&self) -> Option<&str>;
    fn pre_context(&self) -> &[String];
    fn run_post_tool(
        &self,
        call: PostToolCall<'_>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<PostToolOutcome>>;
    /// Nested tool call issued inside a composite tool such as `eval`; the pre-tool hook does not fire by default.
    fn run_pre_nested_tool(
        &self,
        _name: &str,
        _call_id: &str,
        _arguments: &str,
        _cancellation: &CancellationToken,
        _emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<PreToolOutcome>> {
        Ok(None)
    }
}

/// Queue-seal check boundary for `plan_submit`, implemented by the runtime input queue.
pub trait PlanSubmitGate {
    fn seal_for_plan_review(&self) -> Result<bool>;
}

/// Port trait for the tool layer to reach subagent binding lifetimes, implemented by `pp-runtime`'s `Binding`.
pub trait SubagentHost: Send + Sync {
    fn definitions(&self, mode: WorkMode) -> Vec<FunctionObject>;
    fn pending(&self) -> bool;
    fn plan_mode(&self) -> bool;
    fn is_root(&self) -> bool;
    fn bind_session(&self, dir: &Path, id: &str);
    fn execute_task(
        &self,
        arguments: &str,
        plan: bool,
        cancellation: &CancellationToken,
    ) -> Result<Value>;
    fn uri(
        &self,
        tool: &str,
        path: &str,
        content: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Value>;
    fn read_agent_result(
        &self,
        source: &str,
        cancellation: &CancellationToken,
    ) -> Result<(Value, Option<Value>)>;
    fn wait_jobs(&self, ids: &[String]) -> Result<Vec<pp_workspace::WaitJob>>;
    fn acknowledge(&self, ids: &[String]) -> Result<()>;
    fn check_lsp_cleanup(&self) -> Result<()>;
    fn cancel_runs(&self, ids: &[String]) -> Result<()>;
    fn reap_runs(&self, ids: &[String]) -> Result<()>;
    fn shutdown(&self) -> Result<()>;
}

pub fn is_subagent_uri(path: &str) -> bool {
    ["agent://", "history://", "proc://"]
        .iter()
        .any(|p| path.starts_with(p))
}

pub fn yield_definition() -> FunctionObject {
    FunctionObject {
        name: "yield".into(),
        description: Some(
            "Submit the final structured result in data. Successful submission completes the assigned run."
                .into(),
        ),
        parameters: Some(json!({
            "type": "object",
            "properties": {"data": {}},
            "required": ["data"],
            "additionalProperties": false,
        })),
        strict: None,
    }
}

pub fn validate_schema(schema: &Value) -> Result<()> {
    jsonschema::validator_for(schema)
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!("invalid output schema: {error}"))
}

pub fn validate_output(schema: &Value, data: &Value) -> Result<()> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|error| anyhow::anyhow!("invalid output schema: {error}"))?;
    validator
        .validate(data)
        .map_err(|error| anyhow::anyhow!("output does not match schema: {error}"))
}
