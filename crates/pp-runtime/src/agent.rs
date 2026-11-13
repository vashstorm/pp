pub mod context;
mod debug;
mod fallback;
mod history;
pub mod input;
mod lsp;
mod mcp_management;
pub mod persistence;
pub mod plan_management;
mod plan_open;
mod plan_review;
mod plan_sync;
mod plan_workflow;
mod subagents;
mod task_status;
mod todo;
mod tool_images;
mod turn;

mod bootstrap;
mod model_management;
mod model_switch;
mod session_lifecycle;
mod task_entry;
mod tool_access;
mod turn_loop;
mod usage;

#[cfg(test)]
mod changes_tests;
#[cfg(test)]
mod read_tests;

use crate::tools::Tools;
#[cfg(debug_assertions)]
use anyhow::Context;
use anyhow::{Result, bail, ensure};
use async_openai::types::{
    chat::{
        ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessageContent,
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestToolMessage, ChatCompletionRequestUserMessage, ChatCompletionTool,
        ChatCompletionToolChoiceOption, ChatCompletionTools, CreateChatCompletionRequest,
        ToolChoiceOptions,
    },
    responses::{
        CreateResponse, EasyInputMessage, FunctionCallOutputItemParam, FunctionTool, IncludeEnum,
        InputItem, InputParam, Item, Role, Tool, ToolChoiceOptions as ResponseToolChoiceOptions,
        ToolChoiceParam,
    },
};
use input::InputQueue;
use persistence::{Metadata, SessionJournal, SessionRef};
use pp_config::config::EditMode;
use pp_config::config::ModelCatalog;
use pp_config::config::ModelChoice;
use pp_config::config::ModelRole;
use pp_config::config::RoleModel;
use pp_config::model_state::ModelState;
use pp_config::models::Protocol;
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::cancellation::RunOutcome;
use pp_core::context::TokenUsage;
use pp_core::events::AgentEvent;
use pp_core::model::ModelSource;
use pp_core::model::ThinkLevel;
use pp_llm::OpenAiClient;
use std::cell::RefCell;
use std::path::PathBuf;
#[cfg(test)]
use turn::response_input;
use turn::{append_developer_to, append_user_to};

// Keep each API's native history, including reasoning items required by Responses.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "protocol", content = "request", rename_all = "lowercase")]
enum Session {
    Completion(Box<CreateChatCompletionRequest>),
    Response(Box<CreateResponse>),
}

#[derive(Clone)]
pub struct Bootstrap {
    pub dir: PathBuf,
    pub cwd: PathBuf,
    pub skills: pp_config::skills::SharedSkills,
    pub prompt: crate::system_prompt::Snapshot,
    pub approval: Option<pp_config::config::ApprovalMode>,
    /// 进程级共享的 MCP 单例；根 Agent、子 Agent 与恢复的会话都用它。
    pub mcp: std::sync::Arc<crate::mcp::McpManager>,
    /// 进程级共享的模型状态：首次成功加载后一直复用，重建 Agent 不丢失手动覆盖。
    pub models: pp_config::model_state::SharedModels,
}

impl Bootstrap {
    /// 进程内唯一模型状态：首次成功加载后复用，认证/登录重建 Agent 不丢失手动覆盖。
    pub fn model_state(&self) -> Result<std::sync::Arc<ModelState>> {
        self.models
            .get_or_load(&self.dir.join("settings.yml"), &self.dir.join("models.yml"))
    }

    pub fn build(&self) -> Result<Agent> {
        let models = self.model_state()?;
        let catalog = models.catalog_snapshot();
        let mut tools = Tools::with_mcp(self.cwd.clone(), catalog.edit_mode(), self.mcp.clone())?;
        tools.skills = self.skills.clone();
        let mut agent = Agent::new_with_prompt(
            models,
            tools,
            self.dir.join("sessions"),
            persistence::locks::directory()?,
            self.prompt.clone(),
            pp_stats::database_path()?,
        )?;
        if let Some(mode) = self.approval {
            agent.set_approval_override(mode);
        }
        Ok(agent)
    }
}

pub struct Agent {
    task_status: task_status::Runtime,
    role_route: Option<pp_config::config::RoleRoute>,
    pub runtime_models: Option<crate::model_control::Binding>,
    pub subagent_control: std::sync::Arc<std::sync::Mutex<Option<crate::subagent::Binding>>>,
    client: OpenAiClient,
    tools: Tools,
    session: Session,
    auto_compact: bool,
    auto_compact_threshold_percent: u8,
    last_request: Option<TokenUsage>,
    /// 本次会话普通请求的 output tokens 累计；会话根变化才清空。
    session_output_tokens: u64,
    /// 进程内唯一的模型决议权威（手动覆盖 `a` → 配置默认 `b`）。
    models: std::sync::Arc<ModelState>,
    /// 当前生效模型的来源；只有 fallback 会在任务边界按 `resolve()` 回落。
    model_source: ModelSource,
    catalog: ModelCatalog,
    current_model: ModelChoice,
    think_level: ThinkLevel,
    base_system: String,
    prompt_source: Option<crate::system_prompt::Snapshot>,
    pending_instructions: Option<String>,
    pending_project_load: Vec<crate::project_instructions::Document>,
    reported_project_load: turn::ReportedProjectLoads,
    request_skills: Vec<pp_config::skills::SkillHeader>,
    journal: persistence::SessionJournal,
    replay: Vec<persistence::ReplayRecord>,
    debug: Option<debug::DebugState>,
    workflow: pp_core::plan::WorkflowState,
    /// Fail-closed gate set when stopping MCP for Plan mode failed; blocked
    /// run/continue until a successful `/plan` retry clears it.
    plan_cleanup_error: Option<String>,
    plan_lease: Option<crate::tools::plan_catalog::PlanLease>,
    /// 本次运行已追加但尚未完成 native commit 的迟到报告，避免错误重试复制正文。
    lsp_injected: std::collections::HashSet<u64>,
}

/// Everything the UI must adopt after a mode transition: the worker publishes
/// this snapshot set, installs nothing itself, and waits for the UI's
/// `ExecutionPrepared` acknowledgement before any model request. `replay` is
/// `None` for keep-context/off transitions, `Some(empty)` for a fresh session,
/// and `Some(replay)` for a resume.
pub struct ExecutionPreparation {
    pub workflow: pp_core::plan::WorkflowState,
    pub session_id: Option<String>,
    pub replay: Option<Vec<persistence::ReplayRecord>>,
    pub todo_snapshot: Option<AgentEvent>,
}

pub struct TreeNavigation {
    pub replay: Vec<persistence::ReplayRecord>,
    pub draft: Option<String>,
}

impl Session {
    fn tool_names(&self) -> Vec<String> {
        match self {
            Self::Completion(request) => request
                .tools
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter_map(|tool| match tool {
                    ChatCompletionTools::Function(tool) => Some(tool.function.name.clone()),
                    _ => None,
                })
                .collect(),
            Self::Response(request) => request
                .tools
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter_map(|tool| match tool {
                    Tool::Function(tool) => Some(tool.name.clone()),
                    _ => None,
                })
                .collect(),
        }
    }

    fn restore_tools(&mut self, tools: serde_json::Value) -> Result<()> {
        match self {
            Self::Completion(request) => request.tools = serde_json::from_value(tools)?,
            Self::Response(request) => request.tools = serde_json::from_value(tools)?,
        }
        Ok(())
    }
    fn set_model(&mut self, model: &str) {
        match self {
            Self::Completion(request) => {
                request.model.clear();
                request.model.push_str(model);
            }
            Self::Response(request) => request.model = Some(model.to_owned()),
        }
    }

    fn set_instructions(&mut self, system: String) {
        match self {
            Self::Completion(request) => {
                if let Some(ChatCompletionRequestMessage::System(message)) =
                    request.messages.first_mut()
                {
                    message.content = system.into();
                }
            }
            Self::Response(request) => request.instructions = Some(system),
        }
    }

    fn set_think_level(&mut self, level: ThinkLevel) {
        match self {
            Self::Completion(request) => request.reasoning_effort = Some(level.effort()),
            Self::Response(request) => {
                request.reasoning = Some(async_openai::types::responses::Reasoning {
                    effort: Some(level.effort()),
                    summary: (level != ThinkLevel::None)
                        .then_some(async_openai::types::responses::ReasoningSummary::Concise),
                    ..Default::default()
                });
            }
        }
    }

    fn set_tools(&mut self, definitions: Vec<async_openai::types::chat::FunctionObject>) {
        match self {
            Self::Completion(request) => {
                request.tools = Some(
                    definitions
                        .into_iter()
                        .map(|function| {
                            ChatCompletionTools::Function(ChatCompletionTool { function })
                        })
                        .collect(),
                );
            }
            Self::Response(request) => {
                request.tools = Some(
                    definitions
                        .into_iter()
                        .map(|function| {
                            Tool::Function(FunctionTool {
                                name: function.name,
                                description: function.description,
                                parameters: function.parameters,
                                strict: function.strict,
                                ..Default::default()
                            })
                        })
                        .collect(),
                );
            }
        }
    }

    fn new(model: String, protocol: Protocol, system: String, edit_mode: EditMode) -> Self {
        let definitions = Tools::definitions(edit_mode);
        match protocol {
            Protocol::Completion => Session::Completion(Box::new(CreateChatCompletionRequest {
                model,
                messages: vec![ChatCompletionRequestMessage::System(
                    ChatCompletionRequestSystemMessage {
                        content: system.into(),
                        ..Default::default()
                    },
                )],
                tools: Some(
                    definitions
                        .into_iter()
                        .map(|function| {
                            ChatCompletionTools::Function(ChatCompletionTool { function })
                        })
                        .collect(),
                ),
                tool_choice: Some(ChatCompletionToolChoiceOption::Mode(
                    ToolChoiceOptions::Auto,
                )),
                stream: Some(true),
                stream_options: Some(async_openai::types::chat::ChatCompletionStreamOptions {
                    include_usage: Some(true),
                    include_obfuscation: None,
                }),
                ..Default::default()
            })),
            Protocol::Response => Session::Response(Box::new(CreateResponse {
                model: Some(model),
                instructions: Some(system),
                input: InputParam::Items(Vec::new()),
                tools: Some(
                    definitions
                        .into_iter()
                        .map(|function| {
                            Tool::Function(FunctionTool {
                                name: function.name,
                                description: function.description,
                                parameters: function.parameters,
                                strict: function.strict,
                                ..Default::default()
                            })
                        })
                        .collect(),
                ),
                tool_choice: Some(ToolChoiceParam::Option(ResponseToolChoiceOptions::Auto)),
                stream: Some(true),
                store: Some(false),
                include: Some(vec![IncludeEnum::ReasoningEncryptedContent]),
                ..Default::default()
            })),
        }
    }
}

#[cfg(test)]
mod tests;
