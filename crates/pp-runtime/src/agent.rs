mod auto_review;
pub mod context;
pub mod context_diagnosis;
mod debug;
pub mod dump;
mod fallback;
mod history;
pub(crate) mod hooks;
pub mod input;
mod lsp;
mod mcp_management;
mod output_schema;
pub mod persistence;
pub mod plan_management;
mod plan_open;
mod plan_review;
mod plan_sync;
mod plan_workflow;
mod prune;
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
pub use dump::{DumpFile, DumpMode, DumpOutput};
#[cfg(debug_assertions)]
pub use tool_access::run_web_search_standalone;
mod turn_loop;
mod usage;

#[cfg(test)]
mod read_tests;

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
use pp_tools::Tools;
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
    pub permission: std::sync::Arc<pp_permission::Gate>,
    pub sandbox_override: Option<pp_config::config::SandboxMode>,
    /// Process-wide shared sandbox singleton; initialized on first config load, and auth/rebuild keeps the session override.
    pub sandbox: pp_sandbox::SharedSandbox,
    /// Process-wide shared MCP singleton; the root agent, subagents and restored sessions all use it.
    pub mcp: std::sync::Arc<pp_mcp::McpManager>,
    /// Process-wide shared model state: reused after the first successful load, so rebuilding an agent keeps manual overrides.
    pub models: pp_config::model_state::SharedModels,
    /// Optional tool capability allowlist (non-interactive `-p` defaults to the read-only set or `--allowed-tools`).
    pub allowed_tools: Option<pp_tools::AllowedTools>,
    /// Whether subagents may be initialized and dispatched (off by default in non-interactive `-p`, enabled only by `--allowed-tools Task`).
    pub subagents_enabled: bool,
    /// Whether lifecycle hooks are enabled (`--no-hooks` passes `false` to skip loading and execution).
    pub hooks_enabled: bool,
    /// Whether to run in memory-only mode without persistence (`--ephemeral` never writes to disk or acquires a session lock).
    pub ephemeral: bool,
    /// Maximum model request rounds for a non-interactive one-off task (`--max-turns`).
    pub max_turns: Option<usize>,
    /// JSON Schema constraint for non-interactive final output (`--output-schema`).
    pub output_schema: Option<serde_json::Value>,
}

impl Bootstrap {
    /// The single in-process model state: reused after the first successful load, so auth/login agent rebuilds keep manual overrides.
    pub fn model_state(&self) -> Result<std::sync::Arc<ModelState>> {
        self.models
            .get_or_load(&self.dir.join("settings.yml"), &self.dir.join("models.yml"))
    }

    pub fn build(&self) -> Result<Agent> {
        self.build_with_stats(pp_stats::Database::new(pp_stats::database_path()?))
    }

    /// Applies the successfully parsed permission and sandbox settings on their own when the model catalog has not loaded yet;
    /// after the shared sandbox is initialized once, the running manual selection and overrides are kept and not overwritten again.
    pub fn apply_tools_settings(&self, tools: &pp_config::config::ToolsSettings) {
        self.permission.set_config(tools.clone());
        let _ = self.sandbox.get_or_init_with_override(
            &tools.sandbox,
            &self.cwd,
            self.sandbox_override,
        );
    }

    /// Injects the process-wide stats database handle: requests, tools, hooks, titles and subagents reuse the same connection.
    ///
    /// The root `main::run` calls this after constructing the handle; [`Self::build`] keeps the self-resolving path entry point.
    pub fn build_with_stats(&self, stats: pp_stats::Database) -> Result<Agent> {
        let models = self.model_state()?;
        let catalog = models.catalog_snapshot();
        let mut tools = Tools::with_mcp(self.cwd.clone(), catalog.edit_mode(), self.mcp.clone())?;
        pp_core::logging::perf::mark("tools_ready");
        tools.skills = self.skills.clone();
        tools.permission = self.permission.as_ref().clone();
        tools.allowed_tools = self.allowed_tools.clone();
        let sandbox = self.sandbox.get_or_init_with_override(
            catalog.sandbox_settings(),
            &self.cwd,
            self.sandbox_override,
        );
        pp_core::logging::perf::mark("sandbox_ready");
        tools.set_sandbox(sandbox);
        let mut agent = Agent::new_with_prompt_with_options(
            models,
            tools,
            self.dir.join("sessions"),
            persistence::locks::directory()?,
            self.prompt.clone(),
            stats,
            self.hooks_enabled,
            self.ephemeral,
        )?;
        agent.set_subagents_enabled(self.subagents_enabled);
        agent.set_max_turns(self.max_turns);
        agent.set_output_schema(self.output_schema.clone())?;
        Ok(agent)
    }
}

pub struct Agent {
    task_status: task_status::Runtime,
    context_snapshot: Option<context_diagnosis::Snapshot>,
    role_route: Option<pp_config::config::RoleRoute>,
    pub runtime_models: Option<crate::model_control::Binding>,
    pub subagent_control: std::sync::Arc<std::sync::Mutex<Option<crate::subagent::Binding>>>,
    pub(crate) subagent: Option<crate::subagent::Binding>,
    subagents_enabled: bool,
    client: OpenAiClient,
    tools: Tools,
    session: Session,
    auto_compact: bool,
    auto_compact_threshold_percent: u8,
    auto_prune: bool,
    auto_prune_threshold_percent: u8,
    last_request: Option<TokenUsage>,
    /// Raw estimated tokens from the last real `last_request` (used to calibrate the usage threshold and cleared together with `last_request`).
    last_request_raw_tokens: Option<u64>,
    /// Accumulated output tokens for plain requests in this session; cleared only when the session root changes.
    session_output_tokens: u64,
    /// Thinking/reasoning records of committed rounds in this session (for real `/context` statistics across both protocols and restores).
    thinking_history: Vec<context_diagnosis::ThinkingEntry>,
    /// The single in-process authority for model resolution (manual override `a` → configured default `b`).
    models: std::sync::Arc<ModelState>,
    /// Source of the currently effective model; only fallback drops back to `resolve()` at task boundaries.
    model_source: ModelSource,
    catalog: ModelCatalog,
    current_model: ModelChoice,
    think_level: ThinkLevel,
    prompt_cache_key_enabled: bool,
    prompt_cache_retention: Option<pp_config::models::PromptCacheRetention>,
    base_system: String,
    prompt_source: Option<crate::system_prompt::Snapshot>,
    pending_instructions: Option<String>,
    pending_project_load: Vec<pp_tools::project_instructions::Document>,
    reported_project_load: turn::ReportedProjectLoads,
    /// Date string frozen when the session is created/reset, so crossing midnight cannot rewrite the system prompt prefix and break the cache.
    session_date: String,
    /// Static project rules block frozen into system/instructions on the first round; it stays fixed once the session has history,
    /// and later newly activated or changed `AGENTS.md` files are appended incrementally as developer messages.
    static_project_prompt: Option<String>,
    /// `(path -> body)` snapshot already delivered to the model through the system prefix or a developer message.
    delivered_project_docs: std::collections::BTreeMap<PathBuf, String>,
    request_skills: Vec<pp_config::skills::SkillHeader>,
    journal: persistence::SessionJournal,
    replay: Vec<persistence::ReplayRecord>,
    debug: Option<debug::DebugState>,
    workflow: pp_core::plan::WorkflowState,
    /// Fail-closed gate set when stopping MCP for Plan mode failed; blocked
    /// run/continue until a successful `/plan` retry clears it.
    plan_cleanup_error: Option<String>,
    plan_lease: Option<pp_tools::plan_catalog::PlanLease>,
    /// Late reports already appended in this run but not yet committed natively, so an error retry does not duplicate the body.
    lsp_injected: std::collections::HashSet<u64>,
    /// User-level hook configuration frozen in-process; subagents clone the same instance.
    hooks: std::sync::Arc<crate::hooks::Runner>,
    /// Role definition name of the subagent (`agent_type`); `None` for the root agent.
    hook_agent_type: Option<String>,
    ephemeral: bool,
    max_turns: Option<usize>,
    output_schema: Option<output_schema::OutputSchema>,
    /// Shared state of the independent safety reviewer in `auto_review` mode.
    auto_review: Option<std::sync::Arc<auto_review::AutoReviewState>>,
    /// Declared after `tools`/`subagent` so subagents shut down before the temporary directory is cleaned up.
    ephemeral_subagent_dir: Option<tempfile::TempDir>,
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.stop_subagents();
    }
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

pub(in crate::agent) const PROJECT_INSTRUCTIONS_PREFIX: &str = "[pp-project-instructions]\n";

pub(in crate::agent) fn current_session_date() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

pub(in crate::agent) fn generate_prompt_cache_key(session_id: Option<&str>) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    if let Some(id) = session_id {
        let clean: String = id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .take(48)
            .collect();
        if !clean.is_empty() {
            return format!("pp-{clean}");
        }
    }
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut hasher = DefaultHasher::new();
    std::process::id().hash(&mut hasher);
    seq.hash(&mut hasher);
    nanos.hash(&mut hasher);
    let h1 = hasher.finish();
    (h1 ^ 0x9e37_79b9_7f4a_7c15).hash(&mut hasher);
    let h2 = hasher.finish();
    format!("pp-{h1:016x}{h2:016x}")
}

/// Generates a stable shared `prompt_cache_key` for concurrent/follow-up subagents under the same role, shared context and plan,
/// so batches with the same prefix hit the OpenAI Responses server-side prefix cache.
pub(in crate::agent) fn generate_subagent_prompt_cache_key(
    definition: &str,
    shared_context: &str,
    plan: &str,
    model: &str,
) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    "pp-subagent-v1".hash(&mut hasher);
    definition.hash(&mut hasher);
    shared_context.hash(&mut hasher);
    plan.hash(&mut hasher);
    model.hash(&mut hasher);
    let h1 = hasher.finish();
    (h1 ^ 0x9e37_79b9_7f4a_7c15).hash(&mut hasher);
    let h2 = hasher.finish();
    format!("pp-sub-{h1:016x}{h2:016x}")
}

/// Selects the lowest thinking level the target model allows for internal helper requests such as summaries, titles and automatic review.
pub(in crate::agent) fn lowest_think_level(
    thinking: &pp_config::models::ModelThinking,
    fallback: ThinkLevel,
) -> ThinkLevel {
    for candidate in [
        ThinkLevel::None,
        ThinkLevel::Low,
        ThinkLevel::Medium,
        ThinkLevel::High,
        ThinkLevel::Xhigh,
        ThinkLevel::Max,
    ] {
        if thinking.supports(candidate) {
            return candidate;
        }
    }
    fallback
}

impl Session {
    fn has_history(&self) -> bool {
        match self {
            Self::Completion(request) => request.messages.len() > 1,
            Self::Response(request) => match &request.input {
                InputParam::Items(items) => !items.is_empty(),
                InputParam::Text(text) => !text.is_empty(),
            },
        }
    }

    pub(in crate::agent) fn prompt_cache_key(&self) -> Option<&str> {
        match self {
            Self::Completion(request) => request.prompt_cache_key.as_deref(),
            Self::Response(request) => request.prompt_cache_key.as_deref(),
        }
    }

    pub(in crate::agent) fn set_prompt_cache_key(&mut self, key: String) {
        match self {
            Self::Completion(request) => request.prompt_cache_key = Some(key),
            Self::Response(request) => request.prompt_cache_key = Some(key),
        }
    }

    pub(in crate::agent) fn clear_prompt_cache_key(&mut self) {
        match self {
            Self::Completion(request) => request.prompt_cache_key = None,
            Self::Response(request) => request.prompt_cache_key = None,
        }
    }

    #[allow(deprecated)]
    pub(in crate::agent) fn prompt_cache_retention(
        &self,
    ) -> Option<async_openai::types::responses::PromptCacheRetention> {
        match self {
            Self::Completion(_) => None,
            Self::Response(request) => request.prompt_cache_retention,
        }
    }

    #[allow(deprecated)]
    pub(in crate::agent) fn set_prompt_cache_retention_raw(
        &mut self,
        retention: Option<async_openai::types::responses::PromptCacheRetention>,
    ) {
        if let Self::Response(request) = self {
            request.prompt_cache_retention = retention;
        }
    }

    pub(in crate::agent) fn set_prompt_cache_retention(
        &mut self,
        retention: Option<pp_config::models::PromptCacheRetention>,
    ) {
        let mapped = retention.map(|value| match value {
            pp_config::models::PromptCacheRetention::InMemory => {
                async_openai::types::responses::PromptCacheRetention::InMemory
            }
            pp_config::models::PromptCacheRetention::Hours24 => {
                async_openai::types::responses::PromptCacheRetention::Hours24
            }
        });
        self.set_prompt_cache_retention_raw(mapped);
    }

    pub(in crate::agent) fn apply_cache_policy(
        &mut self,
        enabled: bool,
        retention: Option<pp_config::models::PromptCacheRetention>,
        fallback_key: impl FnOnce() -> String,
    ) {
        self.set_prompt_cache_retention(retention);
        if enabled {
            if self.prompt_cache_key().is_none() {
                self.set_prompt_cache_key(fallback_key());
            }
        } else {
            self.clear_prompt_cache_key();
        }
    }

    pub(in crate::agent) fn set_max_output_tokens(&mut self, limit: u32) {
        match self {
            Self::Completion(request) => request.max_completion_tokens = Some(limit),
            Self::Response(request) => request.max_output_tokens = Some(limit),
        }
    }

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

    fn chat_tools(
        definitions: Vec<async_openai::types::chat::FunctionObject>,
    ) -> Vec<ChatCompletionTools> {
        definitions
            .into_iter()
            .map(|function| ChatCompletionTools::Function(ChatCompletionTool { function }))
            .collect()
    }

    fn response_tools(definitions: Vec<async_openai::types::chat::FunctionObject>) -> Vec<Tool> {
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
            .collect()
    }

    pub(in crate::agent) fn function_from_response_tool(
        tool: &FunctionTool,
    ) -> async_openai::types::chat::FunctionObject {
        async_openai::types::chat::FunctionObject {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
            strict: tool.strict,
        }
    }

    fn set_tools(&mut self, definitions: Vec<async_openai::types::chat::FunctionObject>) {
        let _ = self.update_tools(definitions);
    }

    /// Updates the tool list and reports whether anything materially changed; compares only the tool definitions themselves, avoiding serialization of the whole session history.
    fn update_tools(
        &mut self,
        definitions: Vec<async_openai::types::chat::FunctionObject>,
    ) -> bool {
        match self {
            Self::Completion(request) => {
                let next = (!definitions.is_empty()).then(|| Self::chat_tools(definitions));
                let changed = request.tools != next;
                if changed {
                    request.tools = next;
                }
                changed
            }
            Self::Response(request) => {
                let next = (!definitions.is_empty()).then(|| Self::response_tools(definitions));
                let changed = request.tools != next;
                if changed {
                    request.tools = next;
                }
                changed
            }
        }
    }

    fn new(model: String, protocol: Protocol, system: String, edit_mode: EditMode) -> Self {
        let definitions = Tools::definitions(edit_mode, pp_config::config::ToolLimits::default());
        Self::new_with_tools(model, protocol, system, Some(definitions))
    }

    fn new_without_tools(model: String, protocol: Protocol, system: String) -> Self {
        Self::new_with_tools(model, protocol, system, None)
    }

    fn new_with_tools(
        model: String,
        protocol: Protocol,
        system: String,
        definitions: Option<Vec<async_openai::types::chat::FunctionObject>>,
    ) -> Self {
        let has_tools = definitions.is_some();
        match protocol {
            Protocol::Completion => Session::Completion(Box::new(CreateChatCompletionRequest {
                model,
                messages: vec![ChatCompletionRequestMessage::System(
                    ChatCompletionRequestSystemMessage {
                        content: system.into(),
                        ..Default::default()
                    },
                )],
                tools: definitions.map(Self::chat_tools),
                tool_choice: has_tools.then_some(ChatCompletionToolChoiceOption::Mode(
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
                tools: definitions.map(Self::response_tools),
                tool_choice: has_tools
                    .then_some(ToolChoiceParam::Option(ResponseToolChoiceOptions::Auto)),
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
