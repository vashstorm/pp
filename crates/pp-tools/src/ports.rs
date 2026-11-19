//! `pp-tools` 对上层执行域（hooks、输入队列、子 Agent）的窄端口 trait。

use anyhow::Result;
use async_openai::types::chat::FunctionObject;
use pp_core::cancellation::CancellationToken;
use pp_core::events::AgentEvent;
use pp_core::plan::WorkMode;
use serde_json::{Value, json};
use std::path::Path;

/// 必须中止本批的行为：`continue:false` 或无法放行的 hook 故障。
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

/// 嵌套工具执行的前置 Hook 决定。
#[derive(Default)]
pub struct PreToolOutcome {
    pub deny_reason: Option<String>,
    pub ask_reason: Option<String>,
    pub additional_context: Vec<String>,
    pub updated_arguments: Option<String>,
    pub stop_reason: Option<String>,
}

/// 单次工具执行携带的 hook 上下文。
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
    /// `eval` 等复合工具内部发起的嵌套工具调用；默认不触发前置 Hook。
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

/// `plan_submit` 的队列封闭检查边界，由运行中输入队列实现。
pub trait PlanSubmitGate {
    fn seal_for_plan_review(&self) -> Result<bool>;
}

/// 工具层访问子 Agent 绑定生命周期的端口 trait，由 `pp-runtime` 的 `Binding` 实现。
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
