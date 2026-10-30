pub(crate) mod context;
mod debug;
mod history;
pub(crate) mod input;
pub(crate) mod persistence;
pub(crate) mod plan_management;
mod plan_open;
mod plan_review;
mod plan_sync;
mod plan_workflow;
mod subagents;
mod todo;
mod tool_images;
mod turn;

#[cfg(test)]
mod changes_tests;
#[cfg(test)]
mod read_tests;

use crate::{
    cancellation::{self, CancellationToken, Cancelled, RunOutcome},
    config::{EditMode, ModelCatalog, ModelChoice, ThinkLevel},
    context::{ContextUsage, TokenUsage, estimate_tokens},
    events::AgentEvent,
    llm::OpenAiClient,
    models::Protocol,
    tools::Tools,
};
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
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use turn::{
    RequestUpdate, TurnOutcome, TurnResources, append_developer_to, append_user_to,
    completion_turn, log_task_finished, reported_project_load, response_input, response_turn,
};

// Keep each API's native history, including reasoning items required by Responses.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "protocol", content = "request", rename_all = "lowercase")]
enum Session {
    Completion(Box<CreateChatCompletionRequest>),
    Response(Box<CreateResponse>),
}

#[derive(Clone)]
pub(crate) struct Bootstrap {
    pub(crate) dir: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) skills: crate::skills::SharedSkills,
    pub(crate) prompt: crate::system_prompt::Snapshot,
    pub(crate) approval: Option<crate::tools::approval::ApprovalMode>,
}

impl Bootstrap {
    pub(crate) fn build(&self) -> Result<Agent> {
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
        )?;
        if let Some(mode) = self.approval {
            agent.set_approval_override(mode);
        }
        Ok(agent)
    }
}

pub(crate) struct Agent {
    pub(crate) subagent_control: std::sync::Arc<std::sync::Mutex<Option<crate::subagent::Binding>>>,
    client: OpenAiClient,
    tools: Tools,
    session: Session,
    auto_compact: bool,
    auto_compact_threshold_percent: u8,
    last_request: Option<TokenUsage>,
    catalog: ModelCatalog,
    current_model: ModelChoice,
    think_level: ThinkLevel,
    base_system: String,
    prompt_source: Option<crate::system_prompt::Snapshot>,
    pending_instructions: Option<String>,
    pending_project_load: Option<String>,
    reported_project_load: Option<String>,
    request_skills: Vec<crate::skills::SkillHeader>,
    journal: persistence::SessionJournal,
    replay: Vec<persistence::ReplayRecord>,
    debug: Option<debug::DebugState>,
    workflow: crate::plan::WorkflowState,
    /// Fail-closed gate set when stopping MCP for Plan mode failed; blocked
    /// run/continue until a successful `/plan` retry clears it.
    plan_cleanup_error: Option<String>,
    plan_lease: Option<crate::tools::plan_catalog::PlanLease>,
}

/// Everything the UI must adopt after a mode transition that replaced the
/// managed MCP registry: the worker publishes this, installs nothing itself,
/// and waits for the UI's `ExecutionPrepared` acknowledgement before any
/// model request. `replay` is `None` for keep-context/off transitions,
/// `Some(empty)` for a fresh session, and `Some(replay)` for a resume.
pub(crate) struct ExecutionPreparation {
    pub(crate) workflow: crate::plan::WorkflowState,
    pub(crate) session_id: Option<String>,
    pub(crate) replay: Option<Vec<persistence::ReplayRecord>>,
    pub(crate) cleanup: crate::mcp::Cleanup,
    pub(crate) todo_snapshot: Option<AgentEvent>,
}

pub(crate) struct TreeNavigation {
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
                        .then_some(async_openai::types::responses::ReasoningSummary::Auto),
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
    pub(crate) fn bash_timeout_seconds(&self) -> u64 {
        self.tools.limits.bash_timeout_seconds
    }

    pub(crate) fn read_tool_result_preview(&self) -> bool {
        self.catalog.read_tool_result_preview()
    }
    pub(crate) fn mcp_cleanup(&self) -> Result<crate::mcp::Cleanup> {
        self.tools.mcp.cleanup()
    }

    pub(crate) fn background_cleanup(&self) -> crate::tools::BackgroundCleanup {
        self.tools.background_cleanup()
    }

    #[cfg(test)]
    pub(crate) fn new(catalog: ModelCatalog, tools: Tools, sessions_dir: PathBuf) -> Result<Self> {
        let lock_dir = sessions_dir.join(".locks");
        Self::new_with_prompt(
            catalog,
            tools,
            sessions_dir,
            lock_dir,
            crate::system_prompt::Snapshot::bundled(),
        )
    }

    pub(crate) fn new_with_prompt(
        catalog: ModelCatalog,
        mut tools: Tools,
        sessions_dir: PathBuf,
        lock_dir: PathBuf,
        prompt_source: crate::system_prompt::Snapshot,
    ) -> Result<Self> {
        let config = catalog.resolve(catalog.default_reference())?;
        tools.limits = config.settings.tool_limits();
        tools.approval.set_config(config.settings.tools.clone());
        let client = OpenAiClient::new(&config)?;
        tools.mcp = crate::mcp::Registry::connect(
            &config.settings.mcp_servers,
            tools.cwd(),
            &CancellationToken::new(),
        )?;
        let current_model = ModelChoice {
            reference: catalog.default_reference().to_owned(),
            model: config.model.clone(),
            protocol: config.protocol,
            thinking: config.thinking,
            context_window: config.context_window,
        };
        let edit_mode = tools.edit_mode();
        let think_level = if let Some(assignment) = catalog
            .model_roles()
            .get(&crate::config::ModelRole::Default)
            .filter(|assignment| assignment.enabled)
        {
            ensure!(
                config.thinking.supports(assignment.effort),
                "DEFAULT role effort 不受模型支持"
            );
            assignment.effort
        } else {
            config
                .thinking
                .effective_default(catalog.default_think_level())
        };
        // 项目文件在任务开始时加载，读取失败不会让整个交互会话永久不可用。
        let context = crate::system_prompt::RenderContext::new(
            tools.cwd(),
            &config.model,
            edit_mode,
            "execute",
            tools
                .all_definitions(crate::plan::WorkMode::Execute)
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
        session.set_tools(tools.all_definitions(crate::plan::WorkMode::Execute));
        session.set_think_level(think_level);
        Ok(Self {
            client,
            subagent_control: Default::default(),
            session,
            tools,
            auto_compact: config.settings.auto_compact,
            auto_compact_threshold_percent: config.settings.auto_compact_threshold_percent,
            last_request: None,
            catalog,
            current_model,
            think_level,
            base_system: system,
            prompt_source: Some(prompt_source),
            pending_instructions: None,
            pending_project_load: None,
            reported_project_load: None,
            request_skills: Vec::new(),
            journal: SessionJournal::with_locks(sessions_dir, lock_dir),
            replay: Vec::new(),
            debug: debug::DebugState::new(config.settings.debug, false),
            workflow: crate::plan::WorkflowState::default(),
            plan_cleanup_error: None,
            plan_lease: None,
        })
    }

    pub(crate) fn set_ask_handler(&mut self, handler: crate::interaction::AskHandler) {
        self.tools.ask_handler = Some(std::sync::Arc::from(handler));
        self.session
            .set_tools(self.tools.all_definitions(self.workflow.mode));
    }

    /// `--approval-mode` 覆盖 settings.yml 的 `tools.approvalMode`。
    pub(crate) fn set_approval_override(&mut self, mode: crate::tools::approval::ApprovalMode) {
        self.tools.approval.set_override(mode);
    }

    /// 审批 handler 镜像 ask：不改变 schema 列表，无需刷新工具定义。
    pub(crate) fn set_approval_handler(
        &mut self,
        handler: crate::tools::approval::ApprovalHandler,
    ) {
        self.tools.approval.set_handler(handler);
    }

    pub(crate) fn restore(
        catalog: ModelCatalog,
        mut tools: Tools,
        restored: persistence::RestoredSession,
    ) -> Result<Self> {
        let persistence::RestoredSession {
            mut checkpoint,
            mut journal,
            mut replay,
        } = restored;
        let config = catalog
            .resolve(&checkpoint.model_reference)
            .map_err(|_| anyhow::anyhow!("saved session model is unavailable or has changed"))?;
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
        if let Some(snapshot) =
            plan_sync::sync_restored_todos(&tools, &mut journal, &mut checkpoint.workflow, &replay)?
        {
            replay.push(persistence::ReplayRecord::Event(snapshot));
        }
        let client = OpenAiClient::new(&config)?;
        tools.limits = checkpoint.tool_limits;
        // 审批配置沿用当前 settings.yml，不恢复 checkpoint 中的旧值；
        // 会话级授权本来就不序列化。
        tools.approval.set_config(config.settings.tools.clone());
        if checkpoint.workflow.mode == crate::plan::WorkMode::Execute {
            // A restored Plan session never connects MCP; only Execute does.
            tools.mcp = crate::mcp::Registry::connect(
                &config.settings.mcp_servers,
                tools.cwd(),
                &CancellationToken::new(),
            )?;
        }
        checkpoint
            .session
            .set_tools(tools.all_definitions(checkpoint.workflow.mode));
        if let Session::Completion(request) = &mut checkpoint.session {
            request.stream_options = Some(async_openai::types::chat::ChatCompletionStreamOptions {
                include_usage: Some(true),
                include_obfuscation: None,
            });
        }
        let agent = Self {
            client,
            subagent_control: Default::default(),
            tools,
            session: checkpoint.session,
            auto_compact: config.settings.auto_compact,
            auto_compact_threshold_percent: config.settings.auto_compact_threshold_percent,
            last_request: None,
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
                    &agent.workflow.instructions(),
                )
                .context(crate::system_prompt::PromptFailure)?;
        }
        Ok(agent)
    }

    fn prepare_resume(&self, id: &str) -> Result<persistence::RestoredSession> {
        let dir = crate::config::global_dir()?;
        let mut restored = persistence::load(
            &dir.join("sessions"),
            self.journal.lock_dir(),
            Some(id),
            true,
        )?;
        let saved = &restored.checkpoint;
        let same_model = saved.model_reference == self.current_model.reference
            && match &saved.session {
                Session::Completion(request) => {
                    self.current_model.protocol == Protocol::Completion
                        && request.model == self.current_model.model
                }
                Session::Response(request) => {
                    self.current_model.protocol == Protocol::Response
                        && request.model.as_deref() == Some(self.current_model.model.as_str())
                }
            };
        // 跨模型时去掉供应商绑定的 Responses 状态，沿用已有的历史转换校验。
        let source = if same_model
            || matches!(saved.session, Session::Completion(_))
                && self.current_model.protocol == Protocol::Completion
        {
            restored.checkpoint.session
        } else {
            history::convert(
                &saved.session,
                self.current_model.model.clone(),
                self.current_model.protocol,
                self.tools.edit_mode(),
            )?
        };
        let system = self.compose_system(&restored.checkpoint.workflow)?;
        let mut session = Session::new(
            self.current_model.model.clone(),
            self.current_model.protocol,
            system,
            self.tools.edit_mode(),
        );
        match (&mut session, source) {
            (Session::Completion(target), Session::Completion(source)) => {
                let skip_system = usize::from(matches!(
                    source.messages.first(),
                    Some(ChatCompletionRequestMessage::System(_))
                ));
                target
                    .messages
                    .extend(source.messages.into_iter().skip(skip_system));
            }
            (Session::Response(target), Session::Response(source)) => target.input = source.input,
            _ => unreachable!("history converted to the current protocol"),
        }
        session.set_tools(
            self.tools
                .all_definitions(restored.checkpoint.workflow.mode),
        );
        session.set_think_level(self.think_level);
        restored.checkpoint.session = session;
        Ok(restored)
    }

    pub(crate) fn session_id(&self) -> Option<&str> {
        self.journal.id()
    }

    pub(crate) fn session_tree(&mut self) -> Result<persistence::tree::View> {
        self.journal.tree_view()
    }

    pub(crate) fn navigate_tree(&mut self, id: &str) -> Result<TreeNavigation> {
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
            cwd: self.tools.cwd(),
            model_reference: &self.current_model.reference,
            think_level: self.think_level,
            edit_mode: self.tools.edit_mode(),
            tool_limits: self.tools.limits,
            base_system: &self.base_system,
            prompt_source: self.prompt_source.as_ref(),
        };
        let keep_plan_todos = self.workflow.mode == crate::plan::WorkMode::Plan
            || self.workflow.plan.as_ref().is_some_and(|plan| {
                plan.status == crate::plan::PlanStatus::Executed
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
            && plan.status == crate::plan::PlanStatus::Ready
        {
            plan.status = crate::plan::PlanStatus::Draft;
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
        self.tools.clear_changes();
        self.last_request = None;
        self.pending_instructions = None;
        self.pending_project_load = None;
        self.reported_project_load = reported_project_load(&replay);
        Ok(TreeNavigation { replay, draft })
    }
    pub(crate) fn take_session_notice(&mut self) -> Option<String> {
        self.journal.take_notice()
    }
    pub(crate) fn take_replay(&mut self) -> Vec<persistence::ReplayRecord> {
        std::mem::take(&mut self.replay)
    }

    #[cfg(test)]
    pub(crate) fn skills(&self) -> crate::skills::SharedSkills {
        self.tools.skills.clone()
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
            &self.workflow.instructions(),
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

    pub(crate) fn reload_models(&mut self) -> Result<()> {
        self.stop_subagents()?;
        self.catalog.reload_models()
    }

    pub(crate) fn model_choices(&self) -> Vec<ModelChoice> {
        self.catalog.choices()
    }

    pub(crate) fn current_model(&self) -> &ModelChoice {
        &self.current_model
    }

    pub(crate) fn context_usage(&self) -> Result<ContextUsage> {
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
            auto_compact: self.auto_compact,
            threshold_percent: self.auto_compact_threshold_percent,
        })
    }

    fn publish_context(&self, emit: &mut dyn FnMut(AgentEvent)) -> Result<()> {
        emit(AgentEvent::ContextUpdated {
            usage: self.context_usage()?,
        });
        Ok(())
    }

    pub(crate) fn think_level(&self) -> ThinkLevel {
        self.think_level
    }

    pub(crate) fn default_think_level(&self) -> ThinkLevel {
        self.catalog.default_think_level()
    }

    pub(crate) fn save_default_think_level(&mut self, level: ThinkLevel) -> Result<()> {
        self.catalog.save_default_think_level(level)
    }

    pub(crate) fn default_model_reference(&self) -> &str {
        self.catalog.default_reference()
    }

    pub(crate) fn save_default_model(&mut self, reference: &str) -> Result<()> {
        self.catalog.save_default_model(reference)
    }

    pub(crate) fn model_roles(&self) -> &crate::config::ModelRoles {
        self.catalog.model_roles()
    }

    pub(crate) fn save_model_role(
        &mut self,
        role: crate::config::ModelRole,
        assignment: Option<crate::config::RoleAssignment>,
    ) -> Result<()> {
        self.catalog.save_model_role(role, assignment)?;
        if let Some(binding) = &self.tools.subagent {
            binding.set_model_roles(self.catalog.model_roles().clone());
        }
        Ok(())
    }

    pub(crate) fn favorite_models(&self) -> &[String] {
        self.catalog.favorite_models()
    }

    pub(crate) fn toggle_favorite_model(&mut self, reference: &str) -> Result<Vec<String>> {
        self.catalog.toggle_favorite_model(reference)
    }

    /// 实际尝试发送的逻辑模型请求累计次数；读取失败时不得伪装成零。
    pub(crate) fn model_usage(&self) -> Result<BTreeMap<String, u64>> {
        self.catalog.model_usage()
    }

    pub(crate) fn set_think_level(&mut self, level: ThinkLevel) -> Result<()> {
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
        Ok(())
    }

    pub(crate) fn switch_model(&mut self, reference: &str) -> Result<()> {
        self.switch_model_with_effort(reference, None)
    }

    pub(super) fn switch_model_with_effort(
        &mut self,
        reference: &str,
        effort: Option<ThinkLevel>,
    ) -> Result<()> {
        self.journal.check()?;
        let config = self.catalog.resolve(reference)?;
        if let Some(level) = effort {
            ensure!(
                config.thinking.supports(level),
                "角色 effort 不受目标模型支持"
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
            return Ok(());
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
            &self.workflow.instructions(),
            self.workflow.mode_name(),
            &choice.model,
            definitions.iter().map(|tool| tool.name.clone()).collect(),
        )?;
        self.stop_subagents()?;
        let metadata = Metadata {
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
        self.client = client;
        self.current_model = choice;
        self.think_level = target_level;
        self.last_request = None;
        self.pending_instructions = None;
        Ok(())
    }

    pub(crate) fn reset(&mut self) {
        if let Err(error) = self.stop_subagents() {
            self.plan_cleanup_error = Some(format!("子 Agent 清理失败: {error:#}"));
            return;
        }
        self.plan_lease = None;
        self.tools.clear_changes();
        self.workflow = crate::plan::WorkflowState {
            mode: self.workflow.mode,
            ..Default::default()
        };
        self.journal.reset();
        if let Some(debug) = &mut self.debug {
            debug.set_prompt_recorded(false);
        }
        self.replay.clear();
        self.last_request = None;
        self.pending_project_load = None;
        self.reported_project_load = None;
        match &mut self.session {
            Session::Completion(request) => request.messages.truncate(1),
            Session::Response(request) => response_input(request).clear(),
        }
    }

    pub(crate) fn workflow(&self) -> &crate::plan::WorkflowState {
        &self.workflow
    }

    pub(crate) fn changes(&self) -> crate::tools::ChangesView {
        self.tools.changes_view()
    }

    pub(crate) fn undo_file(
        &self,
        token: &crate::tools::UndoToken,
    ) -> Result<crate::tools::UndoResult> {
        ensure!(
            self.workflow.mode == crate::plan::WorkMode::Execute,
            "Undo is unavailable in Plan mode."
        );
        self.tools.undo_file(token)
    }

    pub(crate) fn run_shell(
        &mut self,
        command: &str,
        call_id: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        ensure!(
            self.workflow.mode == crate::plan::WorkMode::Execute,
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
        if self.workflow.mode == crate::plan::WorkMode::Execute
            && matches!(
                document.status,
                crate::plan::PlanStatus::Completed | crate::plan::PlanStatus::Abandoned
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
        if self.workflow.mode != crate::plan::WorkMode::Plan {
            return Ok(());
        }
        self.journal.check()?;
        // 裸 /plan 只待命：首条非空任务才分配文档并绑定 origin。
        let created = self.workflow.plan.is_none() && !prompt.trim().is_empty();
        let before_creation = created.then(|| self.workflow.clone());
        if created {
            self.workflow.plan = Some(crate::plan::PlanDocument::new()?);
        }
        if self.workflow.plan.is_none() {
            return Ok(());
        }
        let plan = self.workflow.plan.as_mut().expect("plan allocated");
        ensure!(
            !matches!(
                plan.status,
                crate::plan::PlanStatus::Executed | crate::plan::PlanStatus::Abandoned
            ),
            "This plan has already been handed off; use /plan to toggle."
        );
        // 列表打开的 Plans 也可继续讨论；接受讨论后统一降为 Draft，
        // 只开放规划工具，重新提交并明确批准前不得实施。
        let old_status = plan.status;
        let set_origin = plan.origin_prompt.is_empty() && !prompt.trim().is_empty();
        let persist = created || set_origin || old_status == crate::plan::PlanStatus::Ready;
        let previous = before_creation.or_else(|| persist.then(|| self.workflow.clone()));
        let plan = self.workflow.plan.as_mut().expect("plan allocated");
        if set_origin {
            plan.origin_prompt = prompt.to_owned();
        }
        if plan.status == crate::plan::PlanStatus::Ready {
            plan.status = crate::plan::PlanStatus::Draft;
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

    pub(crate) fn run(
        &mut self,
        prompt: &str,
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
            self.ensure_task_plan_lease()?;
            self.prepare_plan_task(prompt)?;
            self.ensure_task_plan_lease()?;
            self.refresh_instructions()?;
            self.publish_context(emit)?;
            self.tools.begin_changes_task();
            self.append_user(prompt);
            self.run_loop(Some(prompt), input, cancellation, emit)
        })();
        self.finish_task("run", started, result, cancellation, input, emit)
    }

    pub(crate) fn continue_session(
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
            self.ensure_task_plan_lease()?;
            self.prepare_plan_task("")?;
            self.ensure_task_plan_lease()?;
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
            plan_sync::sync_plan_todos(
                &self.tools,
                &mut self.journal,
                &mut self.workflow,
                emit,
                true,
            )?;
            self.refresh_instructions()?;
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
                    bail!("子 Agent 请求预算已耗尽；部分成果已保存在历史中。");
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
                tools: &self.tools,
                journal: &mut self.journal,
                workflow: &mut self.workflow,
                input,
                plan_dirty: &mut plan_dirty,
                reminder: &mut reminder,
            };
            let finished = match &mut self.session {
                Session::Completion(request) => {
                    completion_turn(resources, request, &metadata, update, cancellation, emit)?
                }
                Session::Response(request) => {
                    response_turn(resources, request, &metadata, update, cancellation, emit)?
                }
            };
            self.pending_instructions = None;
            self.journal.commit((&self.session).into())?;
            if plan_dirty {
                self.refresh_plan_progress(emit)?;
            }
            self.publish_context(emit)?;
            if self.tools.yielded.borrow().is_some() {
                if let Some(binding) = self.tools.subagent.clone()
                    && binding.pending()
                {
                    self.tools.yielded.borrow_mut().take();
                    while binding.pending() {
                        cancellation::check(cancellation)?;
                        if !self.receive_subagents()? {
                            binding.sleep();
                        }
                    }
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
                    .status = crate::plan::PlanStatus::Ready;
                if let Err(error) = self.journal.write_workflow(&self.workflow) {
                    self.workflow
                        .plan
                        .as_mut()
                        .expect("submitted plan exists")
                        .status = crate::plan::PlanStatus::Draft;
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
                    while binding.pending() {
                        cancellation::check(cancellation)?;
                        if self.receive_subagents()? {
                            break;
                        }
                        if input.is_some_and(|q| q.has_steering().unwrap_or(false)) {
                            break;
                        }
                        binding.sleep();
                    }
                    continue;
                }
                if self.tools.yield_schema.is_some() && self.tools.yielded.borrow().is_none() {
                    bail!("结构化子任务未调用 yield；结果不满足完成协议。");
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
                self.append_user(item.prompt());
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
