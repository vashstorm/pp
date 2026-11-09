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

#[cfg(test)]
mod changes_tests;
#[cfg(test)]
mod read_tests;

use crate::tools::Tools;
use anyhow::{Context, Result, bail, ensure};
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
use input::{InputQueue, PendingInput};
use persistence::{Metadata, SessionJournal, SessionRef};
use pp_config::config::EditMode;
use pp_config::config::ModelCatalog;
use pp_config::config::ModelChoice;
use pp_config::models::Protocol;
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::cancellation::RunOutcome;
use pp_core::context::ContextUsage;
use pp_core::context::TokenUsage;
use pp_core::context::estimate_tokens;
use pp_core::events::AgentEvent;
use pp_core::model::ThinkLevel;
use pp_llm::OpenAiClient;
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use turn::{
    RequestUpdate, TurnOutcome, TurnResources, append_developer_to, append_user_to,
    completion_turn, log_task_finished, reported_project_load, response_input, response_turn,
};

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
}

impl Bootstrap {
    pub fn build(&self) -> Result<Agent> {
        let catalog =
            ModelCatalog::from_files(&self.dir.join("settings.yml"), &self.dir.join("models.yml"))?;
        let mut tools = Tools::new(self.cwd.clone(), catalog.edit_mode())?;
        tools.skills = self.skills.clone();
        let mut agent = Agent::new_with_prompt(
            catalog,
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
    catalog: ModelCatalog,
    current_model: ModelChoice,
    think_level: ThinkLevel,
    base_system: String,
    prompt_source: Option<crate::system_prompt::Snapshot>,
    pending_instructions: Option<String>,
    pending_project_load: Option<String>,
    reported_project_load: Option<String>,
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

/// Everything the UI must adopt after a mode transition that replaced the
/// managed MCP registry: the worker publishes this, installs nothing itself,
/// and waits for the UI's `ExecutionPrepared` acknowledgement before any
/// model request. `replay` is `None` for keep-context/off transitions,
/// `Some(empty)` for a fresh session, and `Some(replay)` for a resume.
pub struct ExecutionPreparation {
    pub workflow: pp_core::plan::WorkflowState,
    pub session_id: Option<String>,
    pub replay: Option<Vec<persistence::ReplayRecord>>,
    pub cleanup: crate::mcp::Cleanup,
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

impl Agent {
    /// 按当前 settings 安装一次共享 Registry；配置无效只让 LSP 不可用，
    /// 不阻止 Agent 启动。
    fn install_lsp(tools: &mut Tools, catalog: &ModelCatalog) {
        if !catalog.lsp_settings().enabled || tools.lsp_registry().is_some() {
            return;
        }
        let registry = crate::lsp::Registry::new(
            tools.cwd().to_path_buf(),
            Some(catalog.lsp_path()),
            catalog.lsp_settings(),
        );
        tools.install_lsp(registry);
    }

    pub fn bash_timeout_seconds(&self) -> u64 {
        self.tools.limits.bash_timeout_seconds
    }

    pub fn read_tool_result_preview(&self) -> bool {
        self.catalog.read_tool_result_preview()
    }
    fn connect_pending_plan_mcp(&mut self, cancellation: &CancellationToken) -> Result<()> {
        if self.workflow.mode == pp_core::plan::WorkMode::Plan {
            let cwd = self.tools.cwd().to_owned();
            let result =
                self.tools
                    .mcp
                    .connect_enabled(&cwd, cancellation, crate::mcp::Access::ReadOnly);
            if let Err(error) = result {
                if error.is::<pp_core::cancellation::CleanupError>() {
                    self.plan_cleanup_error = Some(format!(
                        "MCP cleanup failed for Plan mode: {error:#}; run /plan again to retry."
                    ));
                }
                return Err(error);
            }
            self.session
                .set_tools(self.tools.all_definitions(self.workflow.mode));
        }
        Ok(())
    }

    pub fn mcp_cleanup(&self) -> Result<crate::mcp::Cleanup> {
        self.tools.mcp.cleanup()
    }

    /// TUI 后台预热使用的 Registry 克隆；`enabled=false` 时为 `None`。
    /// 克隆共享同一批受管理进程，不转移任何清理权。
    pub fn lsp_registry(&self) -> Option<crate::lsp::Registry> {
        self.tools.lsp_registry().cloned()
    }

    /// TUI/CLI 可持有的共享清理句柄；每次调用返回新的 armed 句柄，
    /// supersede 只能解除同一 Registry 的旧句柄。
    pub fn lsp_cleanup(&self) -> Option<crate::lsp::Cleanup> {
        self.tools.lsp_cleanup()
    }

    /// 显式回收全部 language server；仅 Registry owner 生效，借用父 Registry
    /// 的子 Agent 不会停掉父进程。错误保留 `CleanupError` 类型，调用方必须
    /// 合并进退出错误而不能降级为普通工具失败。
    pub fn stop_lsp(&self) -> Result<()> {
        match self.tools.lsp_cleanup() {
            Some(mut cleanup) => cleanup.stop(),
            None => Ok(()),
        }
    }

    pub fn background_cleanup(&self) -> crate::tools::BackgroundCleanup {
        self.tools.background_cleanup()
    }

    /// 根会话临时管道 registry；子 Agent 共享同一实例。
    pub fn pipe_store(&self) -> crate::tools::PipeStore {
        self.tools.pipe_store()
    }

    /// 测试构造入口：统计库随会话目录落在临时工作区，绝不写开发者真实 `~/.pp/stats`。
    #[cfg(test)]
    pub(crate) fn new(catalog: ModelCatalog, tools: Tools, sessions_dir: PathBuf) -> Result<Self> {
        let lock_dir = sessions_dir.join(".locks");
        let stats_db = sessions_dir.join(".stats/stats.db");
        Self::new_with_prompt(
            catalog,
            tools,
            sessions_dir,
            lock_dir,
            crate::system_prompt::Snapshot::bundled(),
            stats_db,
        )
    }

    pub(crate) fn new_with_prompt(
        catalog: ModelCatalog,
        tools: Tools,
        sessions_dir: PathBuf,
        lock_dir: PathBuf,
        prompt_source: crate::system_prompt::Snapshot,
        stats_db: PathBuf,
    ) -> Result<Self> {
        Self::new_with_mcp(
            catalog,
            tools,
            sessions_dir,
            lock_dir,
            prompt_source,
            None,
            stats_db,
        )
    }

    fn resolve_default_model(
        catalog: &ModelCatalog,
    ) -> Result<(
        pp_config::config::Config,
        ThinkLevel,
        Option<pp_config::config::RoleRoute>,
    )> {
        let config = catalog.resolve(catalog.default_reference())?;
        let route = catalog
            .role_route(pp_config::config::ModelRole::Default)
            .filter(|route| route.primary.model == config.model_reference);
        let effort = if let Some(route) = &route {
            ensure!(
                config.thinking.supports(route.primary.effort),
                "DEFAULT role effort is not supported by the model"
            );
            route.primary.effort
        } else {
            config
                .thinking
                .effective_default(catalog.default_think_level())
        };
        Ok((config, effort, route))
    }

    fn new_with_mcp(
        catalog: ModelCatalog,
        mut tools: Tools,
        sessions_dir: PathBuf,
        lock_dir: PathBuf,
        prompt_source: crate::system_prompt::Snapshot,
        mcp: Option<crate::mcp::Registry>,
        stats_db: PathBuf,
    ) -> Result<Self> {
        let (config, think_level, role_route) = Self::resolve_default_model(&catalog)?;
        tools.limits = config.settings.tool_limits();
        tools.approval.set_config(config.settings.tools.clone());
        let client = OpenAiClient::new(&config)?;
        tools.mcp = match mcp {
            Some(registry) => registry,
            None => crate::mcp::Registry::load(
                &catalog.mcp_path(),
                tools.cwd(),
                &CancellationToken::new(),
                crate::mcp::Access::Full,
            )?,
        };
        Self::install_lsp(&mut tools, &catalog);
        let current_model = ModelChoice {
            reference: catalog.default_reference().to_owned(),
            model: config.model.clone(),
            protocol: config.protocol,
            thinking: config.thinking,
            context_window: config.context_window,
        };
        let edit_mode = tools.edit_mode();
        // 项目文件在任务开始时加载，读取失败不会让整个交互会话永久不可用。
        let context = crate::system_prompt::RenderContext::new(
            tools.cwd(),
            &config.model,
            edit_mode,
            "execute",
            tools
                .all_definitions(pp_core::plan::WorkMode::Execute)
                .into_iter()
                .map(|tool| tool.name)
                .collect(),
        );
        let system = prompt_source
            .compose(
                &context,
                tools.limits,
                "",
                &tools
                    .skills
                    .lock()
                    .map_err(|_| anyhow::anyhow!("skill registry unavailable"))?
                    .summary(),
                "",
            )
            .context(crate::system_prompt::PromptFailure)?;
        let initial_system = system.clone();
        let mut session = Session::new(config.model, config.protocol, initial_system, edit_mode);
        session.set_tools(tools.all_definitions(pp_core::plan::WorkMode::Execute));
        session.set_think_level(think_level);
        Ok(Self {
            role_route,
            client,
            runtime_models: None,
            subagent_control: Default::default(),
            task_status: Default::default(),
            session,
            tools,
            auto_compact: config.settings.auto_compact,
            auto_compact_threshold_percent: config.settings.auto_compact_threshold_percent,
            last_request: None,
            session_output_tokens: 0,
            catalog,
            current_model,
            think_level,
            base_system: system,
            prompt_source: Some(prompt_source),
            pending_instructions: None,
            pending_project_load: None,
            reported_project_load: None,
            request_skills: Vec::new(),
            journal: SessionJournal::with_locks(sessions_dir, lock_dir, stats_db),
            replay: Vec::new(),
            debug: debug::DebugState::new(config.settings.debug, false),
            workflow: pp_core::plan::WorkflowState::default(),
            plan_cleanup_error: None,
            plan_lease: None,
            lsp_injected: Default::default(),
        })
    }

    #[cfg(debug_assertions)]
    pub(crate) fn new_mock(
        catalog: ModelCatalog,
        tools: Tools,
        sessions_dir: PathBuf,
        lock_dir: PathBuf,
        scripts: pp_llm::MockScripts,
    ) -> Result<Self> {
        let mut agent = Self::new_with_mcp(
            catalog,
            tools,
            sessions_dir.clone(),
            lock_dir,
            crate::system_prompt::Snapshot::bundled(),
            Some(crate::mcp::Registry::default()),
            sessions_dir.join(".stats/stats.db"),
        )?;
        agent.client.set_mock_scripts(scripts)?;
        Ok(agent)
    }

    #[cfg(debug_assertions)]
    pub(crate) fn stop_mock(&mut self) -> Result<()> {
        let mut failure: Option<anyhow::Error> = None;
        for result in [
            self.stop_subagents(),
            self.tools.stop_background_tasks(),
            self.tools.mcp.abort_connections(),
            self.stop_lsp(),
        ] {
            if let Err(error) = result {
                failure = Some(match failure {
                    Some(previous) => error.context(format!("{previous:#}")),
                    None => error,
                });
            }
        }
        match failure {
            Some(error) if error.is::<cancellation::CleanupError>() => Err(error),
            Some(error) => Err(cancellation::CleanupError(error).into()),
            None => Ok(()),
        }
    }

    pub fn set_ask_handler(&mut self, handler: pp_core::interaction::AskHandler) {
        self.tools.ask_handler = Some(std::sync::Arc::from(handler));
        self.session
            .set_tools(self.tools.all_definitions(self.workflow.mode));
    }

    /// `--approval-mode` 覆盖 settings.yml 的 `tools.approvalMode`。
    pub fn set_approval_override(&mut self, mode: pp_config::config::ApprovalMode) {
        self.tools.approval.set_override(mode);
    }

    /// 审批 handler 镜像 ask：不改变 schema 列表，无需刷新工具定义。
    pub fn set_approval_handler(&mut self, handler: crate::tools::approval::ApprovalHandler) {
        self.tools.approval.set_handler(handler);
    }

    pub fn restore(
        catalog: ModelCatalog,
        mut tools: Tools,
        restored: persistence::RestoredSession,
    ) -> Result<Self> {
        let persistence::RestoredSession {
            mut checkpoint,
            mut journal,
            mut replay,
        } = restored;
        let stale_plan_route = checkpoint.workflow.mode == pp_core::plan::WorkMode::Execute
            && checkpoint
                .role_route
                .as_ref()
                .is_some_and(|route| route.role == pp_config::config::ModelRole::Plan);
        let config = if stale_plan_route {
            let (config, effort, route) = Self::resolve_default_model(&catalog)?;
            checkpoint.think_level = effort;
            checkpoint.role_route = route;
            let compatible = match &checkpoint.session {
                Session::Completion(_) => config.protocol == Protocol::Completion,
                Session::Response(request) => {
                    config.protocol == Protocol::Response
                        && checkpoint.model_reference == config.model_reference
                        && request.model.as_deref() == Some(config.model.as_str())
                }
            };
            if !compatible {
                checkpoint.session = history::convert(
                    &checkpoint.session,
                    config.model.clone(),
                    config.protocol,
                    checkpoint.edit_mode,
                )?;
            }
            checkpoint
                .model_reference
                .clone_from(&config.model_reference);
            checkpoint.session.set_model(&config.model);
            checkpoint.session.set_think_level(checkpoint.think_level);
            config
        } else {
            let config = catalog.resolve(&checkpoint.model_reference).map_err(|_| {
                anyhow::anyhow!("saved session model is unavailable or has changed")
            })?;
            let matches = match &checkpoint.session {
                Session::Completion(request) => {
                    config.protocol == Protocol::Completion && config.model == request.model
                }
                Session::Response(request) => {
                    config.protocol == Protocol::Response
                        && request.model.as_deref() == Some(config.model.as_str())
                }
            };
            ensure!(matches, "saved session model is unavailable or has changed");
            ensure!(
                config.thinking.supports(checkpoint.think_level),
                "saved session thinking level is not supported by the configured model"
            );
            config
        };
        if let Some(snapshot) =
            plan_sync::sync_restored_todos(&tools, &mut journal, &mut checkpoint.workflow, &replay)?
        {
            replay.push(persistence::ReplayRecord::Event(snapshot));
        }
        let client = OpenAiClient::new(&config)?;
        tools.limits = checkpoint.tool_limits;
        // 恢复不承诺旧进程的临时管道正文：显式换一个空 registry。
        tools.reset_pipe_store();
        if let Some(id) = journal.id() {
            let _ = tools.artifacts.bind(journal.directory(), id);
        }
        // 审批配置沿用当前 settings.yml，不恢复 checkpoint 中的旧值；
        // 会话级授权本来就不序列化。
        tools.approval.set_config(config.settings.tools.clone());
        tools.mcp = crate::mcp::Registry::load(
            &catalog.mcp_path(),
            tools.cwd(),
            &CancellationToken::new(),
            crate::mcp::Access::for_mode(checkpoint.workflow.mode),
        )?;
        Self::install_lsp(&mut tools, &catalog);
        checkpoint
            .session
            .set_tools(tools.all_definitions(checkpoint.workflow.mode));
        if let Session::Completion(request) = &mut checkpoint.session {
            request.stream_options = Some(async_openai::types::chat::ChatCompletionStreamOptions {
                include_usage: Some(true),
                include_obfuscation: None,
            });
        }
        let mut agent = Self {
            role_route: checkpoint.role_route,
            client,
            runtime_models: None,
            subagent_control: Default::default(),
            task_status: Default::default(),
            tools,
            session: checkpoint.session,
            auto_compact: config.settings.auto_compact,
            auto_compact_threshold_percent: config.settings.auto_compact_threshold_percent,
            last_request: None,
            session_output_tokens: 0,
            current_model: ModelChoice {
                reference: checkpoint.model_reference,
                model: config.model,
                protocol: config.protocol,
                thinking: config.thinking,
                context_window: config.context_window,
            },
            catalog,
            think_level: checkpoint.think_level,
            base_system: checkpoint.base_system,
            prompt_source: checkpoint.prompt_source,
            pending_instructions: None,
            pending_project_load: None,
            reported_project_load: reported_project_load(&replay),
            request_skills: Vec::new(),
            journal,
            replay,
            debug: debug::DebugState::new(config.settings.debug, true),
            workflow: checkpoint.workflow,
            plan_cleanup_error: None,
            plan_lease: None,
            lsp_injected: Default::default(),
        };
        if let Some(source) = &agent.prompt_source {
            let context = crate::system_prompt::RenderContext::new(
                agent.tools.cwd(),
                &agent.current_model.model,
                agent.tools.edit_mode(),
                agent.workflow.mode_name(),
                agent.session.tool_names(),
            );
            source
                .compose(
                    &context,
                    agent.tools.limits,
                    "",
                    "",
                    &crate::subagent::catalog::workflow_instructions(&agent.workflow),
                )
                .context(crate::system_prompt::PromptFailure)?;
        }
        if stale_plan_route {
            let system = agent.compose_system(&agent.workflow)?;
            agent.session.set_instructions(system);
            let metadata = Metadata {
                role_route: agent.role_route.as_ref(),
                cwd: agent.tools.cwd(),
                model_reference: &agent.current_model.reference,
                think_level: agent.think_level,
                edit_mode: agent.tools.edit_mode(),
                tool_limits: agent.tools.limits,
                base_system: &agent.base_system,
                prompt_source: agent.prompt_source.as_ref(),
            };
            agent
                .journal
                .commit_plan_exit(&metadata, (&agent.session).into(), &agent.workflow)?;
        }
        Ok(agent)
    }

    pub fn session_id(&self) -> Option<&str> {
        self.journal.id()
    }

    pub fn session_tree(&mut self) -> Result<persistence::tree::View> {
        self.journal.tree_view()
    }

    pub fn navigate_tree(&mut self, id: &str) -> Result<TreeNavigation> {
        let previous_binding = self.subagents();
        self.stop_subagents()?;
        if let Some(binding) = previous_binding {
            let ids = binding
                .deliveries(&[])
                .into_iter()
                .map(|r| r.id)
                .collect::<Vec<_>>();
            binding.acknowledge(&ids)?;
        }
        let persistence::tree::Prepared {
            session: source,
            model_reference,
            replay,
            selection,
            draft,
            todos,
            todo_plan_id,
        } = self.journal.prepare_tree(id)?;
        plan_sync::sync_plan_todos(
            &self.tools,
            &mut self.journal,
            &mut self.workflow,
            &mut |_| {},
            false,
        )?;
        let compatible = match &source {
            Session::Completion(_) => self.current_model.protocol == Protocol::Completion,
            Session::Response(request) => {
                self.current_model.protocol == Protocol::Response
                    && model_reference == self.current_model.reference
                    && request.model.as_deref() == Some(self.current_model.model.as_str())
            }
        };
        let source = if compatible {
            source
        } else {
            history::convert(
                &source,
                self.current_model.model.clone(),
                self.current_model.protocol,
                self.tools.edit_mode(),
            )?
        };
        let system = self.compose_system(&self.workflow)?;
        let mut session = Session::new(
            self.current_model.model.clone(),
            self.current_model.protocol,
            system,
            self.tools.edit_mode(),
        );
        match (&mut session, source) {
            (Session::Completion(target), Session::Completion(source)) => {
                let skip = usize::from(matches!(
                    source.messages.first(),
                    Some(ChatCompletionRequestMessage::System(_))
                ));
                target
                    .messages
                    .extend(source.messages.into_iter().skip(skip));
            }
            (Session::Response(target), Session::Response(source)) => target.input = source.input,
            _ => unreachable!("tree history converted to current protocol"),
        }
        session.set_tools(self.tools.all_definitions(self.workflow.mode));
        session.set_think_level(self.think_level);
        let metadata = Metadata {
            role_route: self.role_route.as_ref(),
            cwd: self.tools.cwd(),
            model_reference: &self.current_model.reference,
            think_level: self.think_level,
            edit_mode: self.tools.edit_mode(),
            tool_limits: self.tools.limits,
            base_system: &self.base_system,
            prompt_source: self.prompt_source.as_ref(),
        };
        let keep_plan_todos = self.workflow.mode == pp_core::plan::WorkMode::Plan
            || self.workflow.plan.as_ref().is_some_and(|plan| {
                plan.status == pp_core::plan::PlanStatus::Executed
                    || self.workflow.todo_plan_id.as_deref() == Some(plan.id.as_str())
            });
        let previous_todos = if keep_plan_todos {
            None
        } else {
            let restored_todos = if todo_plan_id.is_none() {
                todos
            } else {
                Vec::new()
            };
            Some((
                std::mem::replace(&mut self.workflow.todos, restored_todos),
                self.workflow.todo_plan_id.take(),
            ))
        };
        let old_status = self.workflow.plan.as_ref().map(|plan| plan.status);
        if let Some(plan) = &mut self.workflow.plan
            && plan.status == pp_core::plan::PlanStatus::Ready
        {
            plan.status = pp_core::plan::PlanStatus::Draft;
        }
        if let Err(error) = self.journal.write_tree_state(
            &metadata,
            (&session).into(),
            &self.workflow,
            Some(&selection),
        ) {
            if let (Some(plan), Some(status)) = (&mut self.workflow.plan, old_status) {
                plan.status = status;
            }
            if let Some((todos, source)) = previous_todos {
                self.workflow.todos = todos;
                self.workflow.todo_plan_id = source;
            }
            return Err(error);
        }
        self.session = session;
        self.reset_lsp_diagnostics();
        self.tools.clear_changes();
        // 导航到另一会话根：旧根的临时管道正文不能跨根复用。
        self.tools.reset_pipe_store();
        self.clear_session_usage();
        if let Some(view) = &self.task_status.view {
            view.clear_title();
        }
        self.pending_instructions = None;
        self.pending_project_load = None;
        self.reported_project_load = reported_project_load(&replay);
        Ok(TreeNavigation { replay, draft })
    }
    pub fn take_session_notice(&mut self) -> Option<String> {
        self.journal.take_notice()
    }
    pub fn take_replay(&mut self) -> Vec<persistence::ReplayRecord> {
        std::mem::take(&mut self.replay)
    }

    fn refresh_instructions(&mut self) -> Result<()> {
        let definitions = if self.tools.subagent.is_some() {
            Some(self.tools.all_definitions(self.workflow.mode))
        } else {
            None
        };
        let names = definitions.as_ref().map_or_else(
            || self.session.tool_names(),
            |defs| defs.iter().map(|d| d.name.clone()).collect(),
        );
        let (system, skills, project) = self.compose_prompt_snapshot(
            &crate::subagent::catalog::workflow_instructions(&self.workflow),
            self.workflow.mode_name(),
            &self.current_model.model,
            names,
        )?;
        if let Some(definitions) = definitions {
            let before = serde_json::to_value(&self.session)?["request"]["tools"].clone();
            self.session.set_tools(definitions);
            let after = serde_json::to_value(&self.session)?["request"]["tools"].clone();
            if before != after {
                self.journal.tools_changed();
            }
        }
        self.request_skills = skills;
        if project.is_empty() {
            self.pending_project_load = None;
            self.reported_project_load = None;
        } else if self.reported_project_load.as_deref() == Some(project.as_str()) {
            self.pending_project_load = None;
        } else {
            self.pending_project_load = Some(project);
        }
        let unchanged = match &self.session {
            Session::Completion(request) => {
                matches!(request.messages.first(), Some(ChatCompletionRequestMessage::System(message)) if matches!(&message.content, async_openai::types::chat::ChatCompletionRequestSystemMessageContent::Text(text) if text == &system))
            }
            Session::Response(request) => request.instructions.as_deref() == Some(system.as_str()),
        };
        if unchanged {
            return Ok(());
        }
        match &mut self.session {
            Session::Completion(request) => {
                if let Some(ChatCompletionRequestMessage::System(message)) =
                    request.messages.first_mut()
                {
                    message.content = system.clone().into();
                }
            }
            Session::Response(request) => request.instructions = Some(system.clone()),
        }
        self.pending_instructions = Some(system);
        Ok(())
    }

    pub fn reload_models(&mut self) -> Result<()> {
        self.stop_subagents()?;
        self.catalog.reload_models()?;
        if self.task_status.accounting.is_some() {
            self.continue_task_status();
        }
        Ok(())
    }

    pub fn model_choices(&self) -> Vec<ModelChoice> {
        self.catalog.choices()
    }

    pub fn current_model(&self) -> &ModelChoice {
        &self.current_model
    }

    pub fn context_usage(&self) -> Result<ContextUsage> {
        // 空会话尚未发送上下文；有对话后才计入系统提示和工具等完整请求开销。
        let estimated_tokens = match &self.session {
            Session::Completion(request)
                if request.messages.iter().all(|message| {
                    matches!(
                        message,
                        ChatCompletionRequestMessage::System(_)
                            | ChatCompletionRequestMessage::Developer(_)
                    )
                }) =>
            {
                0
            }
            Session::Response(request)
                if match &request.input {
                    InputParam::Text(text) => text.is_empty(),
                    InputParam::Items(items) => items.is_empty(),
                } =>
            {
                0
            }
            Session::Completion(request) => estimate_tokens(request)?,
            Session::Response(request) => estimate_tokens(request)?,
        };
        Ok(ContextUsage {
            estimated_tokens,
            context_window: self.current_model.context_window,
            last_request: self.last_request,
            session_output_tokens: self.session_output_tokens,
            auto_compact: self.auto_compact,
            threshold_percent: self.auto_compact_threshold_percent,
        })
    }

    /// 会话根变化（新建、恢复、树导航、Plan 交接）同时丢弃最近请求指标与
    /// 会话累计输出；切模型、effort 和压缩只清最近请求指标。
    fn clear_session_usage(&mut self) {
        self.last_request = None;
        self.session_output_tokens = 0;
    }

    fn publish_context(&self, emit: &mut dyn FnMut(AgentEvent)) -> Result<()> {
        emit(AgentEvent::ContextUpdated {
            usage: self.context_usage()?,
        });
        Ok(())
    }

    pub fn think_level(&self) -> ThinkLevel {
        self.think_level
    }

    pub fn default_think_level(&self) -> ThinkLevel {
        self.catalog.default_think_level()
    }

    pub fn save_default_think_level(&mut self, level: ThinkLevel) -> Result<()> {
        self.catalog.save_default_think_level(level)
    }

    pub fn default_model_reference(&self) -> &str {
        self.catalog.default_reference()
    }

    pub fn save_default_model(&mut self, reference: &str) -> Result<()> {
        self.catalog.save_default_model(reference)
    }

    pub fn model_roles(&self) -> &pp_config::config::ModelRoles {
        self.catalog.model_roles()
    }

    pub fn save_model_role(
        &mut self,
        role: pp_config::config::ModelRole,
        assignment: Option<pp_config::config::RoleAssignment>,
    ) -> Result<()> {
        self.catalog.save_model_role(role, assignment)?;
        if let Some(binding) = &self.tools.subagent {
            binding.set_model_roles(self.catalog.subagent_roles());
        }
        Ok(())
    }

    pub fn favorite_models(&self) -> &[String] {
        self.catalog.favorite_models()
    }

    pub fn toggle_favorite_model(&mut self, reference: &str) -> Result<Vec<String>> {
        self.catalog.toggle_favorite_model(reference)
    }

    /// 实际尝试发送的逻辑模型请求累计次数；读取失败时不得伪装成零。
    pub fn model_usage(&self) -> Result<BTreeMap<String, u64>> {
        self.catalog.model_usage()
    }

    pub fn set_think_level(&mut self, level: ThinkLevel) -> Result<()> {
        if !self.current_model.thinking.supports(level) {
            let mut message =
                String::from("thinking level is not supported by the current model; options: ");
            for (index, allowed) in self.current_model.thinking.levels().enumerate() {
                if index != 0 {
                    message.push_str(", ");
                }
                message.push_str(allowed.as_str());
            }
            bail!(message);
        }
        self.journal.think(level)?;
        self.session.set_think_level(level);
        self.think_level = level;
        if let Some(route) = &mut self.role_route {
            if self.current_model.reference == route.primary.model {
                route.primary.effort = level;
            } else if let Some(fallback) = &mut route.fallback {
                fallback.effort = level;
            }
        }
        if self.role_route.is_some() {
            self.checkpoint_route()?;
        }
        Ok(())
    }

    pub fn model_catalog(&self) -> ModelCatalog {
        self.catalog.clone()
    }

    pub fn apply_runtime_models(&mut self) -> Result<()> {
        if let Some(catalog) = self
            .runtime_models
            .as_ref()
            .and_then(|binding| binding.registry.catalog())
        {
            self.catalog = catalog;
            if self.task_status.view.is_some() && self.task_status.accounting.is_some() {
                self.continue_task_status();
            }
        }
        if self
            .role_route
            .as_ref()
            .is_some_and(|route| self.catalog.role_route(route.role).is_none())
        {
            let previous = self.role_route.take();
            if let Err(error) = self.checkpoint_route() {
                self.role_route = previous;
                return Err(error);
            }
        }
        let Some((model, think)) = self
            .runtime_models
            .as_mut()
            .map(|binding| binding.take_targets())
        else {
            return Ok(());
        };
        // 两次意图是独立提交，按 revision 顺序应用（旧者先、新者后），高 revision 的 effort 胜出。
        match (model, think) {
            (Some(model), Some(think)) if think.revision < model.revision => {
                self.apply_think_target(&think);
                self.apply_model_target(model)
            }
            (Some(model), think) => {
                self.apply_model_target(model)?;
                if let Some(think) = &think {
                    self.apply_think_target(think);
                }
                Ok(())
            }
            (None, Some(think)) => {
                self.apply_think_target(&think);
                Ok(())
            }
            (None, None) => Ok(()),
        }
    }

    fn apply_think_target(&mut self, target: &crate::model_control::ThinkTarget) {
        // 失败只回报 ThinkFailed 并保留旧档位：不向外返回错误、不重试。
        let result = self.set_think_level(target.effort);
        let Some(binding) = self.runtime_models.as_ref() else {
            return;
        };
        match result {
            Ok(()) => binding.report(crate::model_control::Report::ThinkApplied {
                revision: target.revision,
                effort: self.think_level,
            }),
            Err(error) => binding.report(crate::model_control::Report::ThinkFailed {
                revision: target.revision,
                effort: target.effort,
                message: format!("{error:#}"),
            }),
        }
    }

    fn apply_model_target(&mut self, target: crate::model_control::Target) -> Result<()> {
        let old_catalog = std::mem::replace(&mut self.catalog, (*target.catalog).clone());
        let old_route = self.role_route.take();
        self.role_route = target.role.and_then(|role| self.catalog.role_route(role));
        let changed = self
            .switch_model_with_effort(&target.reference, target.effort)
            .and_then(|changed| {
                if !changed && old_route != self.role_route {
                    self.checkpoint_route()
                } else {
                    Ok(())
                }
            });
        if let Err(error) = changed {
            self.role_route = old_route;
            self.catalog = old_catalog;
            let binding = self
                .runtime_models
                .as_ref()
                .expect("模型绑定在应用期间保留");
            binding.report(crate::model_control::Report::Failed {
                agent: binding.agent.clone(),
                revision: target.revision,
                message: format!("{error:#}"),
            });
            return Ok(());
        }
        if let Some(supervisor) = &mut self.tools.subagent {
            supervisor.model = self.current_model.reference.clone();
            supervisor.thinking = self.think_level;
            if supervisor.depth > 0 {
                let supervisor = supervisor.clone();
                let snapshot = supervisor
                    .directory()
                    .join(format!("{}.session.json", supervisor.owner));
                if let Err(error) = self.save_child(&snapshot).and_then(|()| {
                    supervisor.update_model(
                        &self.current_model.reference,
                        self.think_level,
                        self.catalog.cost(&self.current_model.reference),
                    )
                }) {
                    let binding = self
                        .runtime_models
                        .as_ref()
                        .expect("模型绑定在应用期间保留");
                    binding.report(crate::model_control::Report::Failed {
                        agent: binding.agent.clone(),
                        revision: target.revision,
                        message: format!("Could not persist active model: {error:#}"),
                    });
                    return Err(error.context("persisting subagent model change"));
                }
            }
        }
        let binding = self
            .runtime_models
            .as_ref()
            .expect("模型绑定在应用期间保留");
        binding.report(crate::model_control::Report::Applied {
            agent: binding.agent.clone(),
            revision: target.revision,
            model: self.current_model.clone(),
            effort: self.think_level,
        });
        Ok(())
    }

    /// 应用模型与可选档位的唯一入口：先取走角色路由，切换失败时回滚。
    fn apply_model(&mut self, reference: &str, effort: Option<ThinkLevel>) -> Result<()> {
        let previous = self.role_route.take();
        let result = self
            .switch_model_with_effort(reference, effort)
            .and_then(|changed| {
                if !changed && previous.is_some() {
                    self.checkpoint_route()
                } else {
                    Ok(())
                }
            });
        if result.is_err() {
            self.role_route = previous;
        }
        result
    }

    pub fn switch_model(&mut self, reference: &str) -> Result<()> {
        self.apply_model(reference, None)
    }

    /// CLI `--model`：模糊选择后按“手动 /model + 显式档位”应用，返回最终选择供 CLI 显示。
    ///
    /// 显式传入解析后的档位，避免 `switch_model` 的“保留当前档位”语义；选择或应用失败时
    /// 不改变当前模型与档位。启动期尚无活动请求，journal 不会创建会话文件。
    pub fn select_model(&mut self, query: &str) -> Result<pp_config::config::ModelSelection> {
        let selection = self.catalog.select(query)?;
        self.apply_model(&selection.reference, Some(selection.effort))?;
        Ok(selection)
    }

    pub(super) fn switch_model_with_effort(
        &mut self,
        reference: &str,
        effort: Option<ThinkLevel>,
    ) -> Result<bool> {
        self.journal.check()?;
        let config = self.catalog.resolve(reference)?;
        if let Some(level) = effort {
            ensure!(
                config.thinking.supports(level),
                "role effort is not supported by the target model"
            );
        }
        let same_source = self.client.same_source(&config);
        if reference == self.current_model.reference
            && same_source
            && config.model == self.current_model.model
            && config.protocol == self.current_model.protocol
            && config.thinking == self.current_model.thinking
            && config.context_window == self.current_model.context_window
            && effort.is_none_or(|level| level == self.think_level)
        {
            return Ok(false);
        }
        let target_level = effort.unwrap_or_else(|| {
            config.thinking.explicit_default().unwrap_or_else(|| {
                if config.thinking.supports(self.think_level) {
                    self.think_level
                } else {
                    config
                        .thinking
                        .effective_default(self.catalog.default_think_level())
                }
            })
        });
        let client = OpenAiClient::new(&config)?;
        let same_provider = reference.split_once('/').map(|(provider, _)| provider)
            == self
                .current_model
                .reference
                .split_once('/')
                .map(|(provider, _)| provider);
        let portable = config.protocol != self.current_model.protocol
            || (config.protocol == Protocol::Response && (!same_provider || !same_source));
        let converted = if portable {
            Some(history::convert(
                &self.session,
                config.model.clone(),
                config.protocol,
                self.tools.edit_mode(),
            )?)
        } else {
            None
        };
        let choice = ModelChoice {
            reference: reference.to_owned(),
            model: config.model.clone(),
            protocol: config.protocol,
            thinking: config.thinking,
            context_window: config.context_window,
        };
        let definitions = self.tools.all_definitions(self.workflow.mode);
        let system = self.compose_prompt(
            &crate::subagent::catalog::workflow_instructions(&self.workflow),
            self.workflow.mode_name(),
            &choice.model,
            definitions.iter().map(|tool| tool.name.clone()).collect(),
        )?;
        let metadata = Metadata {
            role_route: self.role_route.as_ref(),
            cwd: self.tools.cwd(),
            model_reference: &choice.reference,
            think_level: target_level,
            edit_mode: self.tools.edit_mode(),
            tool_limits: self.tools.limits,
            base_system: &self.base_system,
            prompt_source: self.prompt_source.as_ref(),
        };
        if let Some(mut session) = converted {
            session.set_instructions(system);
            session.set_tools(definitions);
            session.set_think_level(target_level);
            self.journal
                .checkpoint(&metadata, (&session).into(), &self.workflow)?;
            self.session = session;
        } else {
            let old_system = plan_workflow::SavedInstructions::take(&mut self.session);
            self.session.set_instructions(system);
            // Move only the fields being changed; retain the full native reasoning
            // object so a failed checkpoint restores even provider-specific state.
            let (old_model, old_effort, old_reasoning) = match &mut self.session {
                Session::Completion(request) => (
                    Some(std::mem::replace(&mut request.model, config.model)),
                    request.reasoning_effort.take(),
                    None,
                ),
                Session::Response(request) => (
                    request.model.replace(config.model),
                    None,
                    request.reasoning.take(),
                ),
            };
            self.session.set_think_level(target_level);
            if let Err(error) =
                self.journal
                    .checkpoint(&metadata, (&self.session).into(), &self.workflow)
            {
                match &mut self.session {
                    Session::Completion(request) => {
                        request.model = old_model.expect("completion model is always present");
                        request.reasoning_effort = old_effort;
                    }
                    Session::Response(request) => {
                        request.model = old_model;
                        request.reasoning = old_reasoning;
                    }
                }
                old_system.restore(&mut self.session);
                return Err(error);
            }
        }
        let mut client = client;
        client.set_task_accounting(self.task_status.accounting.clone());
        self.client = client;
        self.current_model = choice;
        self.think_level = target_level;
        // 切模型只丢弃最近请求指标，会话累计输出跨模型保留。
        self.last_request = None;
        self.pending_instructions = None;
        Ok(true)
    }

    pub fn reset(&mut self) {
        if let Err(error) = self.stop_subagents() {
            self.plan_cleanup_error = Some(format!("subagent cleanup failed: {error:#}"));
            return;
        }
        self.plan_lease = None;
        self.reset_lsp_diagnostics();
        self.tools.clear_changes();
        self.workflow = pp_core::plan::WorkflowState {
            mode: self.workflow.mode,
            ..Default::default()
        };
        self.journal.reset();
        self.tools.artifacts = Default::default();
        // /new 是新的会话根：旧临时管道正文与 URI 一律失效。
        self.tools.reset_pipe_store();
        if let Some(debug) = &mut self.debug {
            debug.set_prompt_recorded(false);
        }
        self.replay.clear();
        self.clear_session_usage();
        self.clear_session_cost();
        self.pending_project_load = None;
        self.reported_project_load = None;
        match &mut self.session {
            Session::Completion(request) => request.messages.truncate(1),
            Session::Response(request) => response_input(request).clear(),
        }
    }

    pub fn workflow(&self) -> &pp_core::plan::WorkflowState {
        &self.workflow
    }

    pub fn changes(&self) -> crate::tools::ChangesView {
        self.tools.changes_view()
    }

    pub fn undo_file(&self, token: &crate::tools::UndoToken) -> Result<crate::tools::UndoResult> {
        ensure!(
            self.workflow.mode == pp_core::plan::WorkMode::Execute,
            "Undo is unavailable in Plan mode."
        );
        self.tools.undo_file(token)
    }

    pub fn run_shell(
        &mut self,
        command: &str,
        call_id: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        ensure!(
            self.workflow.mode == pp_core::plan::WorkMode::Execute,
            "Shell commands are unavailable in Plan mode."
        );
        let arguments = serde_json::json!({"command": command}).to_string();
        let result = self.tools.execute_with_fetch(
            "bash",
            &arguments,
            call_id,
            crate::tools::ToolPolicy::Execute {
                workflow: &self.workflow,
            },
            cancellation,
            emit,
            &mut |url, token| self.client.fetch_read_url(url, token),
        );
        match result.stop_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn ensure_task_plan_lease(&mut self) -> Result<()> {
        let Some(document) = &self.workflow.plan else {
            self.plan_lease = None;
            return Ok(());
        };
        if self.workflow.mode == pp_core::plan::WorkMode::Execute
            && matches!(
                document.status,
                pp_core::plan::PlanStatus::Completed | pp_core::plan::PlanStatus::Abandoned
            )
            && self.workflow.todo_plan_id.is_none()
        {
            self.plan_lease = None;
            return Ok(());
        }
        if self
            .plan_lease
            .as_ref()
            .is_some_and(|lease| lease.id() == document.id)
        {
            return Ok(());
        }
        self.plan_lease = None;
        self.plan_lease = Some(crate::tools::plan_catalog::PlanLease::acquire(
            self.tools.plan_workspace(),
            &document.id,
        )?);
        // 取得 lease 后按 ID 协调（锁顺序 lease → 登记表锁）：登记表
        // 重命名/移位/首次登记在任务与计划写工具访问前同步到绑定。
        self.reconcile_plan_location()
    }

    fn prepare_plan_task(&mut self, prompt: &str) -> Result<()> {
        if self.workflow.mode != pp_core::plan::WorkMode::Plan {
            return Ok(());
        }
        self.journal.check()?;
        // 裸 /plan 只待命：首条非空任务才分配文档并绑定 origin。
        let created = self.workflow.plan.is_none() && !prompt.trim().is_empty();
        let before_creation = created.then(|| self.workflow.clone());
        if created {
            self.workflow.plan = Some(pp_core::plan::PlanDocument::new()?);
        }
        if self.workflow.plan.is_none() {
            return Ok(());
        }
        let plan = self.workflow.plan.as_mut().expect("plan allocated");
        ensure!(
            !matches!(
                plan.status,
                pp_core::plan::PlanStatus::Executed | pp_core::plan::PlanStatus::Abandoned
            ),
            "This plan has already been handed off; use /plan to toggle."
        );
        // 列表打开的 Plans 也可继续讨论；接受讨论后统一降为 Draft，
        // 只开放规划工具，重新提交并明确批准前不得实施。
        let old_status = plan.status;
        let set_origin = plan.origin_prompt.is_empty() && !prompt.trim().is_empty();
        let persist = created || set_origin || old_status == pp_core::plan::PlanStatus::Ready;
        let previous = before_creation.or_else(|| persist.then(|| self.workflow.clone()));
        let plan = self.workflow.plan.as_mut().expect("plan allocated");
        if set_origin {
            plan.origin_prompt = prompt.to_owned();
        }
        if plan.status == pp_core::plan::PlanStatus::Ready {
            plan.status = pp_core::plan::PlanStatus::Draft;
        }
        if created {
            self.workflow.todo_plan_id = self.workflow.plan.as_ref().map(|plan| plan.id.clone());
            self.workflow.todos.clear();
        }
        if let Some(previous) = previous {
            self.commit_workflow(previous)?;
        }
        Ok(())
    }

    pub fn run(
        &mut self,
        prompt: &str,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        self.run_user(prompt, None, cancellation, input, emit)
    }

    pub fn input_limits(&self) -> pp_config::config::ToolLimits {
        self.tools.limits
    }

    pub fn run_input(
        &mut self,
        prepared: &crate::user_input::UserInput,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        self.run_user(&prepared.display, Some(prepared), cancellation, input, emit)
    }

    fn run_user(
        &mut self,
        prompt: &str,
        prepared: Option<&crate::user_input::UserInput>,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        tracing::info!(
            event = "task_started",
            pid = std::process::id(),
            operation = "run"
        );
        let started = std::time::Instant::now();
        let result = (|| -> Result<RunOutcome> {
            self.journal.check()?;
            if let Some(error) = &self.plan_cleanup_error {
                bail!("{error}");
            }
            ensure!(!prompt.trim().is_empty(), "prompt must not be blank");
            self.connect_pending_plan_mcp(cancellation)?;
            self.apply_runtime_models()?;
            self.restore_role_primary(emit)?;
            self.ensure_task_plan_lease()?;
            self.prepare_plan_task(prompt)?;
            self.ensure_task_plan_lease()?;
            self.refresh_instructions()?;
            self.begin_task_status(prompt, cancellation);
            self.publish_context(emit)?;
            self.tools.begin_changes_task();
            if let Some(prepared) = prepared {
                turn::append_input_to(&mut self.session, prepared);
            } else {
                self.append_user(prompt);
            }
            self.run_loop(Some(prompt), input, cancellation, emit)
        })();
        self.finish_task("run", started, result, cancellation, input, emit)
    }

    pub fn continue_session(
        &mut self,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        tracing::info!(
            event = "task_started",
            pid = std::process::id(),
            operation = "continue"
        );
        let started = std::time::Instant::now();
        let result = (|| -> Result<RunOutcome> {
            self.journal.check()?;
            if let Some(error) = &self.plan_cleanup_error {
                bail!("{error}");
            }
            self.connect_pending_plan_mcp(cancellation)?;
            self.ensure_task_plan_lease()?;
            self.prepare_plan_task("")?;
            self.ensure_task_plan_lease()?;
            self.continue_task_status();
            self.run_loop(None, input, cancellation, emit)
        })();
        self.finish_task("continue", started, result, cancellation, input, emit)
    }

    // run 与 continue_session 共用的任务收尾：关闭输入队列；上下文发布失败
    // 不得掩盖取消或真实任务错误。
    fn finish_task(
        &mut self,
        operation: &str,
        started: std::time::Instant,
        result: Result<RunOutcome>,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        self.finish_task_title();
        if self.task_status.view.is_none() {
            self.set_task_accounting(None);
        }
        let result = self.close_queue(input, result);
        let mut result = self.finish_run(result, cancellation);
        if let Err(error) = self.refresh_plan_progress(emit) {
            let error = match result {
                Err(original) => error.context(format!("Task also failed: {original:#}")),
                Ok(RunOutcome::Cancelled) => {
                    error.context("Task was cancelled; file changes were not rolled back.")
                }
                _ => error,
            };
            self.journal.error(&format!("{error:#}"));
            result = Err(error);
        }
        // 成功边界自动归档：仅当 close_queue、finish_run（含后台清理）与
        // 进度同步全部成功、结果确为 Completed 且取消未置位时尝试；计划
        // 未全部勾选或零任务时静默跳过（保留 Executed/Plans），任务集合
        // 与批准集合不一致及真实 IO/登记/journal 失败把结果转为错误。
        if matches!(&result, Ok(RunOutcome::Completed))
            && !cancellation.is_cancelled()
            && let Err(error) = self.archive_completed_plan(emit)
        {
            let error = error.context("Completed plan was not archived.");
            self.journal.error(&format!("{error:#}"));
            result = Err(error);
        }
        if !matches!(&result, Ok(RunOutcome::Completed)) {
            let _ = self.publish_context(emit);
        }
        log_task_finished(operation, started, &result);
        self.plan_lease = None;
        result
    }

    // 每条退出路径都关闭队列；关闭失败只在任务本身成功时成为主错误。
    fn close_queue(
        &self,
        input: Option<&InputQueue>,
        result: Result<RunOutcome>,
    ) -> Result<RunOutcome> {
        match input {
            None => result,
            Some(queue) => match (result, queue.close()) {
                (result, Ok(())) => result,
                (Ok(_), Err(error)) => Err(error),
                (Err(original), Err(_)) => Err(original),
            },
        }
    }

    fn append_user(&mut self, prompt: &str) {
        append_user_to(&mut self.session, prompt);
    }

    fn finish_run(
        &mut self,
        result: Result<RunOutcome>,
        cancellation: &CancellationToken,
    ) -> Result<RunOutcome> {
        if cancellation.is_cancelled() || result.is_err() {
            self.cancel_lsp_diagnostics();
        }
        if cancellation.is_cancelled() {
            if let Some(binding) = &self.tools.subagent {
                binding.cancel_owned()?;
            }
            self.client.cancel_pending_io();
        }
        let result = match self.tools.stop_background_tasks() {
            Ok(()) => result,
            Err(cleanup) => Err(match result {
                Err(original) if !original.is::<Cancelled>() => {
                    cleanup.context(format!("Task also failed: {original:#}"))
                }
                _ => cleanup,
            }),
        };
        let result = if !matches!(
            &result,
            Ok(RunOutcome::Completed | RunOutcome::AwaitingPlanReview)
        ) {
            if let Some(binding) = &self.tools.subagent {
                match binding.reap_owned() {
                    Ok(()) => result,
                    Err(cleanup) => Err(cleanup.context(format!(
                        "Parent run also stopped: {}",
                        result
                            .as_ref()
                            .err()
                            .map_or(String::new(), |e| format!("{e:#}"))
                    ))),
                }
            } else {
                result
            }
        } else {
            result
        };
        match result {
            Err(error) if error.is::<Cancelled>() => {
                self.journal.check()?;
                self.journal.cancelled()?;
                Ok(RunOutcome::Cancelled)
            }
            Err(error) => {
                self.journal.error(&format!("{error:#}"));
                self.journal.check()?;
                Err(error)
            }
            Ok(outcome) => {
                self.journal.check()?;
                Ok(outcome)
            }
        }
    }

    fn persist_pending_input(&mut self, user: Option<&str>) -> Result<()> {
        let metadata = Metadata {
            role_route: self.role_route.as_ref(),
            cwd: self.tools.cwd(),
            model_reference: &self.current_model.reference,
            think_level: self.think_level,
            edit_mode: self.tools.edit_mode(),
            tool_limits: self.tools.limits,
            base_system: &self.base_system,
            prompt_source: self.prompt_source.as_ref(),
        };
        self.journal.sent(
            &metadata,
            (&self.session).into(),
            &self.workflow,
            user,
            self.pending_instructions.as_deref(),
        )?;
        self.pending_instructions = None;
        Ok(())
    }

    fn run_loop(
        &mut self,
        mut user: Option<&str>,
        input: Option<&InputQueue>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        self.tools.yielded.borrow_mut().take();
        self.tools.start_wait_batch();
        let mut request_count = 0usize;
        let mut claim_boundary = true;
        let mut reminder = todo::TodoReminderState::default();
        loop {
            self.journal.check()?;
            self.ensure_subagents()?;
            self.receive_subagents()?;
            if cancellation.is_cancelled() {
                self.persist_pending_input(user.take())?;
                return Err(Cancelled.into());
            }
            if self
                .runtime_models
                .as_ref()
                .is_some_and(|binding| binding.pending())
            {
                self.persist_pending_input(user.take())?;
            }
            self.apply_runtime_models()?;
            plan_sync::sync_plan_todos(
                &self.tools,
                &mut self.journal,
                &mut self.workflow,
                emit,
                true,
            )?;
            self.refresh_instructions()?;
            self.receive_lsp_diagnostics(emit)?;
            // 补充边界：仅首次普通请求前，以及上一轮完整回复/整批工具结果成功提交之后。
            let claimed = match (input, claim_boundary) {
                (Some(queue), true) => queue.claim_steering(false)?,
                _ => VecDeque::new(),
            };
            claim_boundary = true;
            if !claimed.is_empty() {
                self.apply_pending(claimed, input, &mut user, cancellation, emit)?;
            }
            let usage = self.context_usage()?;
            emit(AgentEvent::ContextUpdated { usage });
            let should_compact = usage.auto_compact
                && usage.context_window.is_some_and(|window| {
                    u128::from(usage.estimated_tokens) * 100
                        >= u128::from(window) * u128::from(usage.threshold_percent)
                });
            if should_compact {
                // Save accepted input exactly once before cutoff, capacity, or
                // quota checks can fail. Never journal the temporary summary.
                self.persist_pending_input(user.take())?;
                self.compact_impl(true, cancellation, emit)?;
                cancellation::check(cancellation)?;
                // 摘要期间文件也可能被修改；重新加载、估算并检查阈值后再发普通请求。
                continue;
            }
            let metadata = Metadata {
                role_route: self.role_route.as_ref(),
                cwd: self.tools.cwd(),
                model_reference: &self.current_model.reference,
                think_level: self.think_level,
                edit_mode: self.tools.edit_mode(),
                tool_limits: self.tools.limits,
                base_system: &self.base_system,
                prompt_source: self.prompt_source.as_ref(),
            };
            if let Some(debug) = &mut self.debug
                && debug
                    .request(&self.session, &metadata, self.journal.id())
                    .is_err()
            {
                debug::DebugState::warn_unavailable();
                self.debug = None;
            }
            if let Some(binding) = &self.tools.subagent
                && binding.depth > 0
            {
                let budget = binding.request_budget();
                if budget > 0 && request_count >= budget.saturating_mul(3) / 2 {
                    bail!(
                        "subagent request budget exhausted; partial results are saved in history."
                    );
                }
                if budget > 0 && request_count == budget {
                    append_developer_to(&mut self.session,"Request budget reached. Finish your current work and return the partial findings now.".into());
                }
            }
            request_count += 1;
            emit(AgentEvent::AssistantStarted);
            let update = RequestUpdate {
                user: user.take(),
                instructions: self.pending_instructions.as_deref(),
                debug: &mut self.debug,
                pending_project_load: &mut self.pending_project_load,
                reported_project_load: &mut self.reported_project_load,
            };
            let mut plan_dirty = false;
            let resources = TurnResources {
                client: &mut self.client,
                skills: &self.request_skills,
                last_request: &mut self.last_request,
                session_output_tokens: &mut self.session_output_tokens,
                tools: &self.tools,
                journal: &mut self.journal,
                workflow: &mut self.workflow,
                input,
                plan_dirty: &mut plan_dirty,
                reminder: &mut reminder,
            };
            let finished = match &mut self.session {
                Session::Completion(request) => {
                    completion_turn(resources, request, &metadata, update, cancellation, emit)
                }
                Session::Response(request) => {
                    response_turn(resources, request, &metadata, update, cancellation, emit)
                }
            };
            let finished = match finished {
                Ok(finished) => finished,
                Err(error) if pp_llm::retry::model_request_failed(&error) => {
                    cancellation::check(cancellation)?;
                    self.pending_instructions = None;
                    if self.activate_fallback(emit)? {
                        claim_boundary = false;
                        request_count -= 1;
                        continue;
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            self.pending_instructions = None;
            self.journal.commit((&self.session).into())?;
            self.tools.commit_wait_results()?;
            self.apply_runtime_models()?;
            if plan_dirty {
                self.refresh_plan_progress(emit)?;
            }
            self.publish_context(emit)?;
            if self.tools.yielded.borrow().is_some() {
                if let Some(binding) = self.tools.subagent.clone()
                    && binding.pending()
                {
                    self.tools.yielded.borrow_mut().take();
                    self.wait_owned_subagents(cancellation)?;
                    append_developer_to(&mut self.session,"Owned background work completed after your submitted result. Review the new results and submit a fresh final yield.".into());
                    continue;
                }
                return Ok(RunOutcome::Completed);
            }
            if matches!(finished, TurnOutcome::PlanSubmitted) {
                self.workflow
                    .plan
                    .as_mut()
                    .expect("submitted plan exists")
                    .status = pp_core::plan::PlanStatus::Ready;
                if let Err(error) = self.journal.write_workflow(&self.workflow) {
                    self.workflow
                        .plan
                        .as_mut()
                        .expect("submitted plan exists")
                        .status = pp_core::plan::PlanStatus::Draft;
                    return Err(error);
                }
                turn::publish_todo_state(&self.workflow, emit);
                return Ok(RunOutcome::AwaitingPlanReview);
            }
            if let TurnOutcome::Completed {
                awaiting_user_answer,
            } = finished
            {
                plan_sync::sync_plan_todos(
                    &self.tools,
                    &mut self.journal,
                    &mut self.workflow,
                    emit,
                    true,
                )?;
                if let Some(binding) = self.tools.subagent.clone()
                    && binding.pending()
                {
                    self.wait_owned_subagents(cancellation)?;
                    continue;
                }
                if self.tools.yield_schema.is_some() && self.tools.yielded.borrow().is_none() {
                    bail!(
                        "structured subagent task did not call yield; result does not satisfy the completion protocol."
                    );
                }
                let remind = todo::eligible(&self.workflow, &reminder, awaiting_user_answer);
                // 原子“领取或关闭”：最终回复与新补充仅在这个短锁内仲裁。
                if let Some(queue) = input {
                    let batch = queue.claim_steering(!remind)?;
                    if !batch.is_empty() {
                        self.apply_pending(batch, input, &mut user, cancellation, emit)?;
                        // 本边界只消费固定批次；确认期间新到达的消息等下一边界。
                        claim_boundary = false;
                        continue;
                    }
                }
                if remind {
                    cancellation::check(cancellation)?;
                    let attempt = reminder.count + 1;
                    let text = todo::reminder_text(&self.workflow.todos, attempt);
                    append_developer_to(&mut self.session, text);
                    self.journal.commit((&self.session).into())?;
                    let event = AgentEvent::TodoReminder {
                        todos: todo::open_todos(&self.workflow.todos),
                        attempt,
                        max_attempts: todo::TODO_REMINDERS_MAX,
                    };
                    self.journal.event(&event);
                    self.journal.check()?;
                    reminder.count = attempt;
                    reminder.awaiting_progress = true;
                    emit(event);
                    continue;
                }
                return Ok(RunOutcome::Completed);
            }
            cancellation::check(cancellation)?;
        }
    }

    // 在提交边界应用一批补充：先记账尚未持久化的初始输入，再逐条 append、
    // 持久化并确认。已确认项永不归还；取消或持久化失败时归还未应用项——
    // 归还只在唯一出口执行，失败路径不必各自维护 push_front 配对。
    fn apply_pending(
        &mut self,
        mut batch: VecDeque<PendingInput>,
        input: Option<&InputQueue>,
        user: &mut Option<&str>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        let result = (|| -> Result<()> {
            if user.is_some()
                && let Err(error) = self.persist_pending_input(user.take())
            {
                return Err(error);
            }
            while !batch.is_empty() {
                if cancellation.is_cancelled() {
                    return Err(Cancelled.into());
                }
                let item = batch.front().expect("batch is non-empty");
                if let Some(prepared) = &item.prepared {
                    turn::append_input_to(&mut self.session, prepared);
                } else {
                    self.append_user(item.prompt());
                }
                if let Err(error) = self.persist_pending_input(Some(item.prompt())) {
                    self.remove_last_user();
                    return Err(error);
                }
                let item = batch.pop_front().expect("batch is non-empty");
                emit(AgentEvent::UserInputAccepted {
                    id: item.id,
                    text: item.into_display(),
                });
            }
            Ok(())
        })();
        if result.is_err() {
            self.restore_pending(input, batch);
        }
        result
    }

    fn restore_pending(&self, input: Option<&InputQueue>, batch: VecDeque<PendingInput>) {
        if let Some(queue) = input
            && let Err(error) = queue.restore_front(batch)
        {
            tracing::error!(event = "queue_restore_failed", error = format!("{error:#}"));
        }
    }

    // 回退尚未持久化的内存 user append；只移除确认属于用户身份的末条消息。
    fn remove_last_user(&mut self) {
        match &mut self.session {
            Session::Completion(request) => {
                if matches!(
                    request.messages.last(),
                    Some(ChatCompletionRequestMessage::User(_))
                ) {
                    request.messages.pop();
                }
            }
            Session::Response(request) => {
                let input = response_input(request);
                if matches!(
                    input.last(),
                    Some(InputItem::EasyMessage(message)) if matches!(message.role, Role::User)
                ) {
                    input.pop();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
