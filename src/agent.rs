pub(crate) mod context;
mod debug;
mod history;
pub(crate) mod input;
pub(crate) mod persistence;
pub(crate) mod plan_management;
mod plan_open;
mod plan_review;
mod plan_workflow;
mod tool_images;

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
use std::collections::VecDeque;
use std::path::PathBuf;

// Keep each API's native history, including reasoning items required by Responses.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "protocol", content = "request", rename_all = "lowercase")]
enum Session {
    Completion(Box<CreateChatCompletionRequest>),
    Response(Box<CreateResponse>),
}

pub(crate) struct Agent {
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
        Self::new_with_prompt(
            catalog,
            tools,
            sessions_dir,
            crate::system_prompt::Snapshot::bundled(),
        )
    }

    pub(crate) fn new_with_prompt(
        catalog: ModelCatalog,
        mut tools: Tools,
        sessions_dir: PathBuf,
        prompt_source: crate::system_prompt::Snapshot,
    ) -> Result<Self> {
        let config = catalog.resolve(catalog.default_reference())?;
        tools.limits = config.settings.tool_limits();
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
        let think_level = config
            .thinking
            .effective_default(config.settings.think_level);
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
            journal: SessionJournal::new(sessions_dir),
            replay: Vec::new(),
            debug: debug::DebugState::new(config.settings.debug, false),
            workflow: crate::plan::WorkflowState::default(),
            plan_cleanup_error: None,
            plan_lease: None,
        })
    }

    pub(crate) fn set_ask_handler(&mut self, handler: crate::interaction::AskHandler) {
        self.tools.ask_handler = Some(handler);
        self.session
            .set_tools(self.tools.all_definitions(self.workflow.mode));
    }

    pub(crate) fn restore(
        catalog: ModelCatalog,
        mut tools: Tools,
        restored: persistence::RestoredSession,
    ) -> Result<Self> {
        let persistence::RestoredSession {
            mut checkpoint,
            journal,
            replay,
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
        let client = OpenAiClient::new(&config)?;
        tools.limits = checkpoint.tool_limits;
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
        let mut restored = persistence::load(&dir.join("sessions"), Some(id), true)?;
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
        let persistence::tree::Prepared {
            session: source,
            model_reference,
            replay,
            selection,
            draft,
        } = self.journal.prepare_tree(id)?;
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
        let (system, skills, project) = self.compose_prompt_snapshot(
            &self.workflow.instructions(),
            self.workflow.mode_name(),
            &self.current_model.model,
            self.session.tool_names(),
        )?;
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

    pub(crate) fn save_default_think_level(&self, level: ThinkLevel) -> Result<()> {
        self.catalog.save_default_think_level(level)
    }

    pub(crate) fn default_model_reference(&self) -> &str {
        self.catalog.default_reference()
    }

    pub(crate) fn save_default_model(&self, reference: &str) -> Result<()> {
        self.catalog.save_default_model(reference)
    }

    pub(crate) fn favorite_models(&self) -> &[String] {
        self.catalog.favorite_models()
    }

    pub(crate) fn toggle_favorite_model(&mut self, reference: &str) -> Result<Vec<String>> {
        self.catalog.toggle_favorite_model(reference)
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
        if reference == self.current_model.reference {
            return Ok(());
        }
        let config = self.catalog.resolve(reference)?;
        let target_level = config.thinking.explicit_default().unwrap_or_else(|| {
            if config.thinking.supports(self.think_level) {
                self.think_level
            } else {
                config
                    .thinking
                    .effective_default(config.settings.think_level)
            }
        });
        let client = OpenAiClient::new(&config)?;
        let same_provider = reference.split_once('/').map(|(provider, _)| provider)
            == self
                .current_model
                .reference
                .split_once('/')
                .map(|(provider, _)| provider);
        let portable = config.protocol != self.current_model.protocol
            || (config.protocol == Protocol::Response && !same_provider);
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
        let system = self.compose_prompt(
            &self.workflow.instructions(),
            self.workflow.mode_name(),
            &choice.model,
            self.tools
                .all_definitions(self.workflow.mode)
                .into_iter()
                .map(|tool| tool.name)
                .collect(),
        )?;
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
            session.set_tools(self.tools.all_definitions(self.workflow.mode));
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
        self.plan_lease = None;
        self.tools.clear_changes();
        self.workflow.plan = None;
        self.workflow.review = None;
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
            crate::tools::ToolPolicy::Execute,
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
        let previous = persist.then(|| self.workflow.clone());
        let plan = self.workflow.plan.as_mut().expect("plan allocated");
        if set_origin {
            plan.origin_prompt = prompt.to_owned();
        }
        if plan.status == crate::plan::PlanStatus::Ready {
            plan.status = crate::plan::PlanStatus::Draft;
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
        let mut claim_boundary = true;
        loop {
            self.journal.check()?;
            if cancellation.is_cancelled() {
                self.persist_pending_input(user.take())?;
                return Err(Cancelled.into());
            }
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
            emit(AgentEvent::AssistantStarted);
            let update = RequestUpdate {
                user: user.take(),
                instructions: self.pending_instructions.as_deref(),
                debug: &mut self.debug,
                pending_project_load: &mut self.pending_project_load,
                reported_project_load: &mut self.reported_project_load,
            };
            let before_revision = self.workflow.plan.as_ref().map(|plan| plan.revision);
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
                return Ok(RunOutcome::AwaitingPlanReview);
            }
            if before_revision != self.workflow.plan.as_ref().map(|plan| plan.revision) {
                self.journal.write_workflow(&self.workflow)?;
            }
            if matches!(finished, TurnOutcome::Completed) {
                // 原子“领取或关闭”：最终回复与新补充仅在这个短锁内仲裁。
                if let Some(queue) = input {
                    let batch = queue.claim_steering(true)?;
                    if !batch.is_empty() {
                        self.apply_pending(batch, input, &mut user, cancellation, emit)?;
                        // 本边界只消费固定批次；确认期间新到达的消息等下一边界。
                        claim_boundary = false;
                        continue;
                    }
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

fn reported_project_load(replay: &[persistence::ReplayRecord]) -> Option<String> {
    replay.iter().rev().find_map(|record| match record {
        persistence::ReplayRecord::Event(AgentEvent::ProjectInstructionsLoaded {
            content, ..
        }) => Some(content.clone()),
        _ => None,
    })
}

fn publish_project_load(
    pending: &mut Option<String>,
    reported: &mut Option<String>,
    journal: &mut SessionJournal,
    cwd: &std::path::Path,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<()> {
    let Some(content) = pending.as_ref() else {
        return Ok(());
    };
    let event = AgentEvent::ProjectInstructionsLoaded {
        path: cwd.join("AGENTS.md").display().to_string(),
        content: content.clone(),
    };
    journal.event(&event);
    journal.check()?;
    emit(event);
    *reported = pending.take();
    Ok(())
}

struct RequestUpdate<'a> {
    user: Option<&'a str>,
    instructions: Option<&'a str>,
    debug: &'a mut Option<debug::DebugState>,
    pending_project_load: &'a mut Option<String>,
    reported_project_load: &'a mut Option<String>,
}

impl RequestUpdate<'_> {
    fn record_prompt(&mut self, session: SessionRef<'_>, metadata: &Metadata<'_>, id: &str) {
        if let Some(debug) = self.debug
            && debug.prompt(session, metadata, id).is_err()
        {
            debug::DebugState::warn_unavailable();
            *self.debug = None;
        }
    }
}

fn log_task_finished(operation: &str, started: std::time::Instant, result: &Result<RunOutcome>) {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let pid = std::process::id();
    match result {
        Ok(RunOutcome::Completed) => tracing::info!(
            event = "task_finished",
            pid,
            operation,
            status = "ok",
            elapsed_ms
        ),
        Ok(RunOutcome::AwaitingPlanReview) => tracing::info!(
            event = "task_finished",
            pid,
            operation,
            status = "awaiting_plan_review",
            elapsed_ms
        ),
        Ok(RunOutcome::Cancelled) => tracing::info!(
            event = "task_finished",
            pid,
            operation,
            status = "cancelled",
            elapsed_ms
        ),
        Err(_) => tracing::error!(
            event = "task_finished",
            pid,
            operation,
            status = "error",
            elapsed_ms
        ),
    }
}

fn log_model_request_finished<T>(
    protocol: &str,
    started: std::time::Instant,
    outcome: &anyhow::Result<T>,
) {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let pid = std::process::id();
    match outcome {
        Ok(_) => tracing::info!(
            event = "model_request_finished",
            pid,
            protocol,
            status = "ok",
            elapsed_ms
        ),
        Err(error) if error.is::<Cancelled>() => tracing::info!(
            event = "model_request_finished",
            pid,
            protocol,
            status = "cancelled",
            elapsed_ms
        ),
        Err(_) => tracing::error!(
            event = "model_request_finished",
            pid,
            protocol,
            status = "error",
            elapsed_ms
        ),
    }
}

// 两种协议在一次模型／工具轮次中使用相同的同步依赖。
struct TurnResources<'a> {
    skills: &'a [crate::skills::SkillHeader],
    client: &'a mut OpenAiClient,
    last_request: &'a mut Option<TokenUsage>,
    tools: &'a Tools,
    journal: &'a mut SessionJournal,
    workflow: &'a mut crate::plan::WorkflowState,
    input: Option<&'a InputQueue>,
    plan_dirty: &'a mut bool,
}

fn tool_policy<'a>(
    workflow: &'a crate::plan::WorkflowState,
    sealed: bool,
    input: Option<&'a InputQueue>,
) -> Result<crate::tools::ToolPolicy<'a>> {
    Ok(match workflow.mode {
        crate::plan::WorkMode::Execute => crate::tools::ToolPolicy::Execute,
        crate::plan::WorkMode::Plan => crate::tools::ToolPolicy::Plan {
            document: workflow
                .plan
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No current plan."))?,
            sealed,
            input,
        },
    })
}

enum TurnOutcome {
    Continue,
    Completed,
    PlanSubmitted,
}

/// Plan file tracked during execution: only an Execute-mode run with an
/// Executed plan watches `FileChanged` events for plan-progress refreshes.
/// The actual short path is cached so the progress callback stays independent
/// of the workflow borrow without cloning the whole document.
fn tracked_plan_path(workflow: &crate::plan::WorkflowState) -> Option<String> {
    if workflow.mode == crate::plan::WorkMode::Execute {
        workflow
            .plan
            .as_ref()
            .filter(|plan| plan.status == crate::plan::PlanStatus::Executed)
            .and_then(|plan| plan.path())
    } else {
        None
    }
}

/// Whether one progress event touches the tracked plan file.
fn plan_file_touched(tools: &Tools, plan_path: &Option<String>, event: &AgentEvent) -> bool {
    match (event, plan_path) {
        (AgentEvent::FileChanged { path, .. }, Some(expected_path)) => {
            tools.is_plan_path(path, expected_path)
        }
        _ => false,
    }
}

fn apply_plan_change(
    workflow: &mut crate::plan::WorkflowState,
    sealed: &mut bool,
    change: Option<crate::tools::PlanChange>,
) -> Result<()> {
    let Some(change) = change else {
        return Ok(());
    };
    ensure!(
        workflow.mode == crate::plan::WorkMode::Plan && !*sealed,
        "Invalid plan tool transition."
    );
    let plan = workflow
        .plan
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("No current plan."))?;
    match change {
        crate::tools::PlanChange::Written {
            title,
            file_name,
            content,
        } => {
            plan.revision = plan.next_revision()?;
            plan.title = title;
            plan.file_name = Some(file_name);
            plan.content = content;
            plan.status = crate::plan::PlanStatus::Draft;
        }
        crate::tools::PlanChange::Submitted => *sealed = true,
    }
    Ok(())
}

fn completion_turn(
    resources: TurnResources<'_>,
    request: &mut CreateChatCompletionRequest,
    metadata: &Metadata<'_>,
    mut update: RequestUpdate<'_>,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<TurnOutcome> {
    let TurnResources {
        client,
        skills,
        last_request,
        tools,
        journal,
        workflow,
        input: queue,
        plan_dirty,
    } = resources;
    tracing::info!(
        event = "model_request_started",
        pid = std::process::id(),
        protocol = "completion"
    );
    let request_started = std::time::Instant::now();
    let response = {
        let journal = RefCell::new(&mut *journal);
        let emit = RefCell::new(&mut *emit);
        let mut sent = || {
            let mut journal = journal.borrow_mut();
            journal.sent(
                metadata,
                SessionRef::Completion(request),
                workflow,
                update.user,
                update.instructions,
            )?;
            publish_project_load(
                update.pending_project_load,
                update.reported_project_load,
                &mut journal,
                metadata.cwd,
                &mut **emit.borrow_mut(),
            )?;
            update.record_prompt(
                SessionRef::Completion(request),
                metadata,
                journal.id().expect("sent request has a session ID"),
            );
            journal.event(&AgentEvent::AssistantStarted);
            journal.check()
        };
        let mut progress = |event| {
            journal.borrow_mut().event(&event);
            (*emit.borrow_mut())(event);
        };
        client.set_stats_context(journal.borrow().stats_context(metadata, "task", skills));
        let outcome = client.complete(request, cancellation, &mut sent, &mut progress);
        journal
            .borrow_mut()
            .track_stats(client.take_stats_key(), outcome.is_ok());
        log_model_request_finished("completion", request_started, &outcome);
        outcome?
    };
    cancellation::check(cancellation)?;
    let usage = response.usage;
    let response = response.message;
    let progress_plan_path = tracked_plan_path(workflow);
    let mut progress = |event| {
        if plan_file_touched(tools, &progress_plan_path, &event) {
            *plan_dirty = true;
        }
        journal.event(&event);
        emit(event);
    };
    let emit: &mut dyn FnMut(AgentEvent) = &mut progress;
    let text = match &response.content {
        Some(ChatCompletionRequestAssistantMessageContent::Text(text)) => text.clone(),
        _ => String::new(),
    };
    emit(AgentEvent::AssistantFinished { text });
    let calls = response.tool_calls.as_deref().unwrap_or_default();
    if calls.is_empty() {
        request
            .messages
            .push(ChatCompletionRequestMessage::Assistant(response));
        *last_request = usage;
        return Ok(TurnOutcome::Completed);
    }
    let count = calls.len();
    let assistant_index = request.messages.len();
    request
        .messages
        .push(ChatCompletionRequestMessage::Assistant(response));
    let mut stop_error = None;
    let mut sealed = false;
    let mut attachments = Vec::new();
    for index in 0..count {
        let ChatCompletionRequestMessage::Assistant(response) = &request.messages[assistant_index]
        else {
            bail!("agent invariant: missing assistant calls");
        };
        let Some(ChatCompletionMessageToolCalls::Function(call)) = response
            .tool_calls
            .as_ref()
            .and_then(|calls| calls.get(index))
        else {
            bail!("agent invariant: missing function call");
        };
        let content = tools.execute_with_fetch(
            &call.function.name,
            &call.function.arguments,
            &call.id,
            tool_policy(workflow, sealed, queue)?,
            cancellation,
            emit,
            &mut |url, token| client.fetch_read_url(url, token),
        );
        apply_plan_change(workflow, &mut sealed, content.plan_change)?;
        if stop_error.is_none() {
            stop_error = content.stop_error;
        }
        let tool_call_id = call.id.clone();
        attachments.push((tool_call_id.clone(), content.images));
        request.messages.push(ChatCompletionRequestMessage::Tool(
            ChatCompletionRequestToolMessage {
                tool_call_id,
                content: content.output.into(),
            },
        ));
    }
    tool_images::append_chat(&mut request.messages, attachments);
    *last_request = usage;
    if let Some(error) = stop_error {
        journal.commit(SessionRef::Completion(request))?;
        return Err(error);
    }
    Ok(if sealed {
        TurnOutcome::PlanSubmitted
    } else {
        TurnOutcome::Continue
    })
}

fn response_turn(
    resources: TurnResources<'_>,
    request: &mut CreateResponse,
    metadata: &Metadata<'_>,
    mut update: RequestUpdate<'_>,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<TurnOutcome> {
    let TurnResources {
        client,
        skills,
        last_request,
        tools,
        journal,
        workflow,
        input: queue,
        plan_dirty,
    } = resources;
    tracing::info!(
        event = "model_request_started",
        pid = std::process::id(),
        protocol = "response"
    );
    let request_started = std::time::Instant::now();
    let turn = {
        let journal = RefCell::new(&mut *journal);
        let emit = RefCell::new(&mut *emit);
        let mut sent = || {
            let mut journal = journal.borrow_mut();
            journal.sent(
                metadata,
                SessionRef::Response(request),
                workflow,
                update.user,
                update.instructions,
            )?;
            publish_project_load(
                update.pending_project_load,
                update.reported_project_load,
                &mut journal,
                metadata.cwd,
                &mut **emit.borrow_mut(),
            )?;
            update.record_prompt(
                SessionRef::Response(request),
                metadata,
                journal.id().expect("sent request has a session ID"),
            );
            journal.event(&AgentEvent::AssistantStarted);
            journal.check()
        };
        let mut progress = |event| {
            journal.borrow_mut().event(&event);
            (*emit.borrow_mut())(event);
        };
        client.set_stats_context(journal.borrow().stats_context(metadata, "task", skills));
        let outcome = client.respond(request, cancellation, &mut sent, &mut progress);
        journal
            .borrow_mut()
            .track_stats(client.take_stats_key(), outcome.is_ok());
        log_model_request_finished("response", request_started, &outcome);
        outcome?
    };
    cancellation::check(cancellation)?;
    let progress_plan_path = tracked_plan_path(workflow);
    let mut progress = |event| {
        if plan_file_touched(tools, &progress_plan_path, &event) {
            *plan_dirty = true;
        }
        journal.event(&event);
        emit(event);
    };
    let emit: &mut dyn FnMut(AgentEvent) = &mut progress;
    emit(AgentEvent::AssistantFinished { text: turn.text });
    let input = response_input(request);
    let start = input.len();
    input.extend(turn.output.into_iter().map(InputItem::from));
    if !turn.has_calls {
        *last_request = turn.usage;
        return Ok(TurnOutcome::Completed);
    }
    let end = input.len();
    let mut stop_error = None;
    let mut sealed = false;
    for index in start..end {
        let InputItem::Item(Item::FunctionCall(call)) = &input[index] else {
            continue;
        };
        let output = tools.execute_with_fetch(
            &call.name,
            &call.arguments,
            &call.call_id,
            tool_policy(workflow, sealed, queue)?,
            cancellation,
            emit,
            &mut |url, token| client.fetch_read_url(url, token),
        );
        apply_plan_change(workflow, &mut sealed, output.plan_change)?;
        if stop_error.is_none() {
            stop_error = output.stop_error;
        }
        let call_id = Some(call.call_id.clone());
        input.push(InputItem::Item(Item::FunctionCallOutput(
            FunctionCallOutputItemParam {
                call_id,
                output: tool_images::response_output(output.output, output.images),
                id: None,
                status: None,
                name: None,
                namespace: None,
                caller: None,
            },
        )));
    }
    *last_request = turn.usage;
    if let Some(error) = stop_error {
        journal.commit(SessionRef::Response(request))?;
        return Err(error);
    }
    Ok(if sealed {
        TurnOutcome::PlanSubmitted
    } else {
        TurnOutcome::Continue
    })
}

/// Append a user message to either protocol's native history; shared by the
/// agent loop and the fresh-context execution candidate session.
fn append_user_to(session: &mut Session, prompt: &str) {
    match session {
        Session::Completion(request) => request.messages.push(ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: prompt.into(),
                ..Default::default()
            },
        )),
        Session::Response(request) => {
            response_input(request).push(InputItem::EasyMessage(EasyInputMessage {
                role: Role::User,
                content: prompt.into(),
                ..Default::default()
            }))
        }
    }
}

fn response_input(request: &mut CreateResponse) -> &mut Vec<InputItem> {
    let InputParam::Items(input) = &mut request.input else {
        unreachable!("agent always uses an item array for Responses history");
    };
    input
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn instruction_loading_cards_failed_requests_record_frozen_body_once() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("AGENTS.md");
            std::fs::write(&path, "loaded-before-network-result").unwrap();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
            for (expected, next, expected_loads) in [
                (
                    "loaded-before-network-result",
                    "changed-after-request-open",
                    1,
                ),
                (
                    "changed-after-request-open",
                    "changed-after-request-open",
                    1,
                ),
                (
                    "changed-after-request-open",
                    "changed-after-request-open",
                    0,
                ),
            ] {
                let mut events = Vec::new();
                std::thread::scope(|scope| {
                    scope.spawn(|| {
                        let mut request = server
                            .recv_timeout(std::time::Duration::from_secs(5))
                            .unwrap()
                            .expect("actual provider request");
                        let mut body = String::new();
                        request.as_reader().read_to_string(&mut body).unwrap();
                        let body: Value = serde_json::from_str(&body).unwrap();
                        let system = if protocol == "response" {
                            body["instructions"].as_str().unwrap()
                        } else {
                            body["messages"][0]["content"].as_str().unwrap()
                        };
                        assert!(system.contains(expected));
                        // 首次建连结果返回前改写磁盘；卡片必须仍使用已发送的快照。
                        std::fs::write(&path, next).unwrap();
                        request.respond(tiny_http::Response::empty(500)).unwrap();
                    });
                    assert!(
                        agent
                            .run(
                                "actual request",
                                &CancellationToken::new(),
                                None,
                                &mut |event| { events.push(event) }
                            )
                            .is_err()
                    );
                });
                let loads: Vec<_> = events
                    .iter()
                    .filter_map(|event| match event {
                        AgentEvent::ProjectInstructionsLoaded { content, .. } => {
                            Some(content.as_str())
                        }
                        _ => None,
                    })
                    .collect();
                assert_eq!(loads, vec![expected; expected_loads]);
            }
            let id = agent.session_id().unwrap().to_owned();
            drop(agent);
            let restored =
                persistence::load(&dir.path().join("sessions"), Some(&id), true).unwrap();
            let loads: Vec<_> = restored
                .replay
                .iter()
                .filter_map(|record| match record {
                    persistence::ReplayRecord::Event(AgentEvent::ProjectInstructionsLoaded {
                        content,
                        ..
                    }) => Some(content.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                loads,
                ["loaded-before-network-result", "changed-after-request-open"]
            );
        }
    }

    #[test]
    fn resume_preparation_preserves_current_session_and_releases_failed_candidate() {
        const FIXTURE: &str = "PP_TEST_RESUME_PREPARATION";
        let Ok(root) = std::env::var(FIXTURE) else {
            let home = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "agent::tests::resume_preparation_preserves_current_session_and_releases_failed_candidate", "--nocapture"])
                .env("HOME", home.path())
                .env(FIXTURE, home.path())
                .status().unwrap();
            assert!(status.success());
            return;
        };
        let root = std::path::Path::new(&root);
        let global = root.join(".pp/agent");
        let sessions = global.join("sessions");
        std::fs::create_dir_all(&global).unwrap();
        let models = global.join("models.yml");
        let settings = global.join("settings.yml");
        let configuration = json!({"providers":{"fixture": {"api_key":"fixture","models":{"one": {"model":"one"}}}}}).to_string();
        std::fs::write(&models, &configuration).unwrap();
        let mut ids = Vec::new();
        let mut agents = Vec::new();
        for name in ["old", "saved"] {
            let cwd = root.join(name);
            std::fs::create_dir(&cwd).unwrap();
            let mut agent = Agent::new(
                ModelCatalog::from_files(&settings, &models).unwrap(),
                Tools::new(cwd, EditMode::Hashline).unwrap(),
                sessions.clone(),
            )
            .unwrap();
            let Session::Response(request) = &mut agent.session else {
                panic!()
            };
            response_input(request).push(InputItem::EasyMessage(EasyInputMessage {
                role: Role::User,
                content: name.into(),
                ..Default::default()
            }));
            let metadata = Metadata {
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
                .sent(
                    &metadata,
                    (&agent.session).into(),
                    &agent.workflow,
                    Some(name),
                    None,
                )
                .unwrap();
            ids.push(agent.session_id().unwrap().to_owned());
            agents.push(agent);
        }
        let saved = agents.pop().unwrap();
        let expected = serde_json::to_value(&saved.session).unwrap();
        drop(saved);
        let old = agents.pop().unwrap();
        let before = serde_json::to_value(&old.session).unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert!(old.prepare_resume(&ids[0]).is_err());
        // 运行中的恢复使用当前配置，不重新读取磁盘上的模型文件。
        std::fs::write(&models, "invalid configuration").unwrap();
        let candidate = old.prepare_resume(&ids[1]).unwrap();
        let mut expected = expected;
        expected["request"]["instructions"] = before["request"]["instructions"].clone();
        assert_eq!(candidate.journal.id(), Some(ids[1].as_str()));
        assert_eq!(
            serde_json::to_value(&candidate.checkpoint.session).unwrap(),
            expected
        );
        assert_eq!(old.session_id(), Some(ids[0].as_str()));
        assert_eq!(serde_json::to_value(&old.session).unwrap(), before);
        assert_eq!(std::env::current_dir().unwrap(), cwd);
        assert!(persistence::load(&sessions, Some(&ids[0]), true).is_err());
        assert!(persistence::load(&sessions, Some(&ids[1]), true).is_err());
        drop(candidate);
        assert!(persistence::load(&sessions, Some(&ids[1]), true).is_ok());
    }

    #[test]
    fn cancellation_before_request_saves_accepted_input_and_replays_terminal() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let settings = dir.path().join("settings.yml");
            let models = dir.path().join("models.yml");
            let sessions = dir.path().join("sessions");
            std::fs::write(
                &models,
                json!({"providers":{"fixture": { "api_key":"fixture-key", "protocol":protocol,
                    "base_url":"http://127.0.0.1:1/v1", "models":{"one": {"model":"one"}}
                }}})
                .to_string(),
            )
            .unwrap();
            let mut agent = Agent::new(
                ModelCatalog::from_files(&settings, &models).unwrap(),
                Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
                sessions.clone(),
            )
            .unwrap();
            let cancellation = CancellationToken::new();
            cancellation.cancel();
            let outcome = agent
                .run("accepted input", &cancellation, None, &mut |event| {
                    assert!(
                        !matches!(
                            event,
                            AgentEvent::AssistantStarted | AgentEvent::ToolStarted { .. }
                        ),
                        "no request or tool may start"
                    );
                })
                .unwrap();
            assert_eq!(outcome, RunOutcome::Cancelled);
            let id = agent.session_id().unwrap().to_owned();
            drop(agent);
            let restored = persistence::load(&sessions, Some(&id), true).unwrap();
            assert!(
                matches!(restored.replay.as_slice(), [persistence::ReplayRecord::User(text), persistence::ReplayRecord::Cancelled] if text == "accepted input")
            );
            let native = serde_json::to_value(&restored.checkpoint.session).unwrap();
            let entries = if protocol == "completion" {
                native["request"]["messages"].as_array().unwrap()
            } else {
                native["request"]["input"].as_array().unwrap()
            };
            assert_eq!(
                entries
                    .iter()
                    .filter(|entry| entry["role"] == "user")
                    .count(),
                1
            );
            assert!(!persistence::list(&sessions).unwrap().sessions[0].done);
        }
    }

    #[test]
    fn background_finish_run_reclaims_processes_for_every_outcome() {
        let dir = tempfile::tempdir().unwrap();
        // 不发真实请求，仅借共享 fixture 构造 Agent；地址占位即可。
        let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
        let mut previous = None;
        for outcome in [0, 1, 2] {
            let token = CancellationToken::new();
            let started = agent.tools.execute("bash", &json!({"command":"printf '%s\\n%s' \"$$\" \"$TMPDIR\" > marker.pending; mv marker.pending marker; exec sleep 60","mode":"background"}).to_string(), "start", crate::tools::ToolPolicy::Execute, &token, &mut |_| {});
            let snapshot: serde_json::Value = serde_json::from_str(&started.output).unwrap();
            let id = snapshot["task_id"].as_str().unwrap().to_owned();
            assert_ne!(previous.as_ref(), Some(&id));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !dir.path().join("marker").exists() {
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let marker = std::fs::read_to_string(dir.path().join("marker")).unwrap();
            let (pid, scratch) = marker.split_once('\n').unwrap();
            let pid = rustix::process::Pid::from_raw(pid.parse().unwrap()).unwrap();
            let result = match outcome {
                0 => Ok(RunOutcome::Completed),
                1 => Err(anyhow::anyhow!("provider failed")),
                _ => {
                    token.cancel();
                    Err(Cancelled.into())
                }
            };
            let finished = agent.finish_run(result, &token);
            match outcome {
                0 => assert_eq!(finished.unwrap(), RunOutcome::Completed),
                1 => assert!(
                    finished
                        .unwrap_err()
                        .to_string()
                        .contains("provider failed")
                ),
                _ => assert_eq!(finished.unwrap(), RunOutcome::Cancelled),
            }
            assert_eq!(
                rustix::process::test_kill_process(pid),
                Err(rustix::io::Errno::SRCH)
            );
            assert!(!std::path::Path::new(scratch).exists());
            let old = agent.tools.execute(
                "bash_task",
                &json!({"task_id":id,"action":"status"}).to_string(),
                "status",
                crate::tools::ToolPolicy::Execute,
                &CancellationToken::new(),
                &mut |_| {},
            );
            assert!(old.output.contains("Unknown background task"));
            previous = Some(id);
            std::fs::remove_file(dir.path().join("marker")).unwrap();
        }
    }

    #[test]
    fn finish_task_archives_completed_plan_only_on_success_boundary() {
        use crate::plan::{PlanLocation, PlanStatus, plan_tasks};
        use crate::tools::plan_catalog::{self, PlanCatalogEntry, PlanCatalogLock};

        let archive_agent = |dir: &std::path::Path, protocol: &str| -> Agent {
            let mut agent = runtime_input_agent(dir, protocol, "127.0.0.1:1", "");
            let mut workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
            workflow.mode = crate::plan::WorkMode::Execute;
            {
                let document = workflow.plan.as_mut().expect("plan bound");
                document.file_name = Some("create-canary.md".into());
                document.approved_tasks = Some(
                    plan_tasks(PLAN_CONTENT)
                        .into_iter()
                        .map(|task| task.text)
                        .collect(),
                );
            }
            agent.workflow = workflow;
            let workspace = agent.tools.plan_workspace();
            let _lock = PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let mut catalog = snapshot.catalog;
            let id = agent.workflow.plan.as_ref().unwrap().id.clone();
            catalog.entries.push(PlanCatalogEntry {
                id,
                file_name: "create-canary.md".into(),
                location: PlanLocation::Plans,
                deleted: false,
                deleted_file_identity: None,
            });
            plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
            drop(_lock);
            seed_session(&mut agent, EXPLORATION);
            write_plan_file(&agent, &PLAN_CONTENT.replace("- [ ]", "- [x]"));
            agent
        };

        for protocol in ["completion", "response"] {
            // Completed 且未取消：归档到 done。
            let dir = tempfile::tempdir().unwrap();
            let mut agent = archive_agent(dir.path(), protocol);
            let cancellation = CancellationToken::new();
            let result = agent.finish_task(
                "test",
                std::time::Instant::now(),
                Ok(RunOutcome::Completed),
                &cancellation,
                None,
                &mut |_| {},
            );
            assert_eq!(result.unwrap(), RunOutcome::Completed);
            assert!(!dir.path().join(".pp/plans/create-canary.md").exists());
            assert!(dir.path().join(".pp/done/create-canary.md").exists());
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(document.status, PlanStatus::Completed);
            assert_eq!(document.location, PlanLocation::Done);

            // Completed 但取消已置位：进度刷新保留，归档不发生。
            let dir = tempfile::tempdir().unwrap();
            let mut agent = archive_agent(dir.path(), protocol);
            let cancellation = CancellationToken::new();
            cancellation.cancel();
            let result = agent.finish_task(
                "test",
                std::time::Instant::now(),
                Ok(RunOutcome::Completed),
                &cancellation,
                None,
                &mut |_| {},
            );
            assert_eq!(result.unwrap(), RunOutcome::Completed);
            assert!(dir.path().join(".pp/plans/create-canary.md").exists());
            assert!(!dir.path().join(".pp/done/create-canary.md").exists());
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Executed
            );

            // 任务失败：不归档。
            let dir = tempfile::tempdir().unwrap();
            let mut agent = archive_agent(dir.path(), protocol);
            let result = agent.finish_task(
                "test",
                std::time::Instant::now(),
                Err(anyhow::anyhow!("provider failed")),
                &CancellationToken::new(),
                None,
                &mut |_| {},
            );
            assert!(result.unwrap_err().to_string().contains("provider failed"));
            assert!(dir.path().join(".pp/plans/create-canary.md").exists());
            assert!(!dir.path().join(".pp/done/create-canary.md").exists());

            // AwaitingPlanReview：不归档。
            let dir = tempfile::tempdir().unwrap();
            let mut agent = archive_agent(dir.path(), protocol);
            let result = agent.finish_task(
                "test",
                std::time::Instant::now(),
                Ok(RunOutcome::AwaitingPlanReview),
                &CancellationToken::new(),
                None,
                &mut |_| {},
            );
            assert_eq!(result.unwrap(), RunOutcome::AwaitingPlanReview);
            assert!(dir.path().join(".pp/plans/create-canary.md").exists());
            assert!(!dir.path().join(".pp/done/create-canary.md").exists());
        }
    }

    #[test]
    fn system_prompt_render_failure_preserves_native_session_and_journal() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let settings = dir.path().join("settings.yml");
            let models = dir.path().join("models.yml");
            std::fs::write(&settings, "default_model: fixture/one\n").unwrap();
            std::fs::write(
                &models,
                json!({"providers":{"fixture":{
                    "protocol":protocol, "api_key":"fixture-key",
                    "models":{"one":{"model":"one"},"two":{"model":"two"}}
                }}})
                .to_string(),
            )
            .unwrap();
            std::fs::write(dir.path().join("base.md"), "{{#if (eq model \"two\")}}{{missing}}{{/if}}{{#if (eq work_mode \"plan\")}}{{missing}}{{/if}}custom {{model}}").unwrap();
            let source = crate::system_prompt::Snapshot::discover(
                dir.path(),
                dir.path(),
                None,
                Some("base.md"),
                None,
            )
            .unwrap();
            let mut agent = Agent::new_with_prompt(
                ModelCatalog::from_files(&settings, &models).unwrap(),
                Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
                dir.path().join("sessions"),
                source,
            )
            .unwrap();
            seed_session(&mut agent, "preserve this input");
            let native = serde_json::to_value(&agent.session).unwrap();
            let path = dir
                .path()
                .join("sessions")
                .join(format!("{}.jsonl", agent.session_id().unwrap()));
            let journal = std::fs::read(&path).unwrap();
            assert!(
                format!("{:#}", agent.switch_model("fixture/two").unwrap_err()).contains("base.md")
            );
            assert_eq!(agent.current_model.reference, "fixture/one");
            assert_eq!(serde_json::to_value(&agent.session).unwrap(), native);
            assert_eq!(std::fs::read(&path).unwrap(), journal);
            agent.workflow.mode = crate::plan::WorkMode::Plan;
            assert!(agent.refresh_instructions().is_err());
            assert_eq!(serde_json::to_value(&agent.session).unwrap(), native);
            assert_eq!(std::fs::read(&path).unwrap(), journal);
        }
    }

    #[test]
    fn model_switch_failure_keeps_native_history_and_selection() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.yml");
        let models = dir.path().join("models.yml");
        std::fs::write(
            &models,
            json!({"providers": {
                "a": { "api_key":"fixture-key", "models":{
                    "one": {"model":"one"}, "two": {"model":"two"}}},
                "bad": { "api_key":"secret-marker\ninvalid", "models":{
                    "one": {"model":"bad"}}}}})
            .to_string(),
        )
        .unwrap();
        let catalog = ModelCatalog::from_files(&settings, &models).unwrap();
        let mut agent = Agent::new(
            catalog,
            Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
            dir.path().join("sessions"),
        )
        .unwrap();
        let Session::Response(request) = &mut agent.session else {
            panic!()
        };
        response_input(request).push(InputItem::EasyMessage(EasyInputMessage {
            role: Role::User,
            content: "remember me".into(),
            ..Default::default()
        }));
        let before = serde_json::to_value(&request).unwrap();
        for reference in ["bad/one", "missing/secret-marker", "secret-marker"] {
            let error = agent.switch_model(reference).unwrap_err().to_string();
            assert!(!error.contains("secret-marker"));
            assert_eq!(agent.current_model().reference, "a/one");
            let Session::Response(request) = &agent.session else {
                panic!()
            };
            assert_eq!(serde_json::to_value(request).unwrap(), before);
        }
        agent.switch_model("a/one").unwrap();
        let Session::Response(request) = &agent.session else {
            panic!()
        };
        assert_eq!(serde_json::to_value(request).unwrap(), before);
        agent.switch_model("a/two").unwrap();
        let Session::Response(request) = &agent.session else {
            panic!()
        };
        let mut expected = before;
        expected["model"] = json!("two");
        assert_eq!(serde_json::to_value(request).unwrap(), expected);
        agent.reset();
        assert_eq!(agent.current_model().reference, "a/two");
        let Session::Response(request) = &agent.session else {
            panic!()
        };
        assert!(matches!(&request.input, InputParam::Items(items) if items.is_empty()));
        assert_eq!(
            serde_json::to_value(request).unwrap()["instructions"],
            expected["instructions"]
        );
    }

    pub(super) fn runtime_input_agent(
        dir: &std::path::Path,
        protocol: &str,
        server_addr: &str,
        settings_extra: &str,
    ) -> Agent {
        let settings = dir.join("settings.yml");
        let models = dir.join("models.yml");
        std::fs::write(
            &settings,
            format!("# 测试设置\ndefault_model: fixture/one\n{settings_extra}"),
        )
        .unwrap();
        let model = json!({"model":"one"});
        std::fs::write(
            &models,
            json!({"providers":{"fixture": {
                "api_key":"fixture-key",
                "protocol":protocol,
                "base_url":format!("http://{server_addr}/v1"),
                "models":{"one":model}
            }}})
            .to_string(),
        )
        .unwrap();
        Agent::new(
            ModelCatalog::from_files(&settings, &models).unwrap(),
            Tools::new(dir.to_owned(), EditMode::Replace).unwrap(),
            dir.join("sessions"),
        )
        .unwrap()
    }

    #[test]
    fn bang_shell_agent_is_local_and_plan_is_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:9", "");
        let before = serde_json::to_value(&agent.session).unwrap();
        let mut events = Vec::new();
        agent
            .run_shell(
                "printf local; exit 7",
                "local",
                &CancellationToken::new(),
                &mut |event| events.push(event),
            )
            .unwrap();
        assert!(events.iter().any(|event| matches!(event, AgentEvent::ToolFinished { result, .. } if result["exit_code"] == 7)));
        assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
        assert!(!dir.path().join("sessions").exists());
        agent.workflow.mode = crate::plan::WorkMode::Plan;
        let error = agent
            .run_shell(
                "touch forbidden",
                "local",
                &CancellationToken::new(),
                &mut |_| {},
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Shell commands are unavailable in Plan mode."
        );
        assert!(!dir.path().join("forbidden").exists());
    }

    // --- shared Plan-mode test fixtures (plan_review / plan_workflow) ---

    pub(super) const PLAN_CONTENT: &str =
        "# Create hello\n\n- [ ] Create hello.txt and read back hello.\n";
    pub(super) const ORIGIN: &str = "ORIGIN-MARKER create hello with newline question";
    pub(super) const EXPLORATION: &str = "EXPLORATION-MARKER planning chat";

    pub(super) fn seed_session(agent: &mut Agent, user: &str) {
        // A real task appends the user message natively before the journal
        // records the projection and state.
        super::append_user_to(&mut agent.session, user);
        agent
            .session
            .set_tools(agent.tools.all_definitions(agent.workflow.mode));
        let metadata = Metadata {
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
            .sent(
                &metadata,
                (&agent.session).into(),
                &agent.workflow,
                Some(user),
                None,
            )
            .unwrap();
    }

    pub(super) fn plan_workflow(
        content: &str,
        origin: &str,
        status: crate::plan::PlanStatus,
    ) -> crate::plan::WorkflowState {
        let mut document = crate::plan::PlanDocument::new().unwrap();
        if !content.is_empty() {
            document.revision = 1;
            document.title = "Create hello".into();
            document.content = content.into();
        }
        document.origin_prompt = origin.into();
        document.status = status;
        // 旧会话语义：无 AI 文件名，位于 Plans，走 <id>.md 兜底路径。
        document.location = crate::plan::PlanLocation::Plans;
        crate::plan::WorkflowState {
            mode: crate::plan::WorkMode::Plan,
            plan: Some(document),
            review: None,
        }
    }

    pub(super) fn write_plan_file(agent: &Agent, content: &str) {
        let document = agent.workflow.plan.as_ref().unwrap();
        let path = agent
            .tools
            .cwd()
            .join(document.path().expect("fixture plan has a saved path"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Latest persisted workflow record, read directly from the JSONL journal
    /// without taking the session's exclusive flock (the agent holds it).
    pub(super) fn journal_workflow(dir: &std::path::Path, id: &str) -> crate::plan::WorkflowState {
        let path = dir.join("sessions").join(format!("{id}.jsonl"));
        let text = std::fs::read_to_string(path).unwrap();
        let mut latest = None;
        for line in text.lines() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            if matches!(
                record["customType"].as_str(),
                Some("pp.workflow" | "pp.session_state")
            ) {
                latest = Some(serde_json::from_value(record["data"]["workflow"].clone()).unwrap());
            }
        }
        latest.expect("workflow record persisted")
    }

    /// Error text for result types without a `Debug` implementation.
    pub(super) fn err_message<T>(result: Result<T>) -> String {
        match result {
            Ok(_) => panic!("expected an error"),
            Err(error) => error.to_string(),
        }
    }

    pub(super) fn sse_stream(request: tiny_http::Request, events: &[Value]) {
        use std::io::Write as _;
        let mut writer = request.into_writer();
        writer
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                  Transfer-Encoding: chunked\r\n\r\n",
            )
            .unwrap();
        for event in events {
            let frame = match event {
                Value::String(text) => format!("data: {text}\n\n"),
                event => format!("data: {event}\n\n"),
            };
            write!(writer, "{:x}\r\n{}\r\n", frame.len(), frame).unwrap();
        }
        writer.write_all(b"0\r\n\r\n").unwrap();
        writer.flush().unwrap();
    }

    pub(super) fn completion_stream(request: tiny_http::Request, message: Value, finish: &str) {
        let chunk = |delta: Value, finish: Option<&str>| {
            json!({
                "id":"chatcmpl-fixture", "object":"chat.completion.chunk",
                "created":1, "model":"one",
                "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
            })
        };
        sse_stream(
            request,
            &[
                chunk(message, None),
                chunk(json!({}), Some(finish)),
                Value::String("[DONE]".into()),
            ],
        );
    }

    /// `emit_part_done` covers both server shapes: the reviewer fixture omits
    /// `response.content_part.done`, while the chat fixture sends it.
    pub(super) fn response_stream(
        request: tiny_http::Request,
        output: &[Value],
        emit_part_done: bool,
    ) {
        let mut events = Vec::new();
        let mut sequence = 0u64;
        for (output_index, item) in output.iter().enumerate() {
            if let Some(parts) = item["content"].as_array() {
                for (content_index, part) in parts.iter().enumerate() {
                    let (event_type, text) = match part["type"].as_str() {
                        Some("output_text") => {
                            ("response.output_text.delta", part["text"].as_str())
                        }
                        Some("refusal") => ("response.refusal.delta", part["refusal"].as_str()),
                        _ => continue,
                    };
                    events.push(json!({
                        "type":event_type, "sequence_number":sequence,
                        "item_id":item["id"], "output_index":output_index,
                        "content_index":content_index, "delta":text.unwrap(),
                        "logprobs":[]
                    }));
                    sequence += 1;
                    if emit_part_done {
                        events.push(json!({
                            "type":"response.content_part.done", "sequence_number":sequence,
                            "item_id":item["id"], "output_index":output_index,
                            "content_index":content_index, "part":part
                        }));
                        sequence += 1;
                    }
                }
            }
            events.push(json!({
                "type":"response.output_item.done", "sequence_number":sequence,
                "output_index":output_index, "item":item
            }));
            sequence += 1;
        }
        events.push(json!({
            "type":"response.completed", "sequence_number":sequence,
            "response":{
                "id":"resp-fixture", "object":"response", "created_at":1,
                "model":"one", "status":"completed", "output":output
            }
        }));
        sse_stream(request, &events);
    }

    pub(super) fn final_for(
        protocol: &str,
        request: tiny_http::Request,
        text: &str,
        emit_part_done: bool,
    ) {
        match protocol {
            "completion" => {
                completion_stream(request, json!({"role":"assistant","content":text}), "stop")
            }
            _ => response_stream(
                request,
                std::slice::from_ref(&json!({
                    "type":"message", "id":"msg-final", "role":"assistant",
                    "phase":"final_answer", "status":"completed",
                    "content":[{"type":"output_text","text":text,
                                "annotations":[],"logprobs":null}]
                })),
                emit_part_done,
            ),
        }
    }

    fn tool_call_for(protocol: &str, request: tiny_http::Request, name: &str, arguments: &str) {
        match protocol {
            "completion" => completion_stream(
                request,
                json!({"role":"assistant","content":null,"tool_calls":[{
                    "id":"call-1", "type":"function", "index":0,
                    "function":{"name":name,"arguments":arguments}
                }]}),
                "tool_calls",
            ),
            _ => response_stream(
                request,
                std::slice::from_ref(&json!({
                    "type":"function_call", "id":"fc-1", "call_id":"call-1",
                    "name":name, "arguments":arguments, "status":"completed"
                })),
                true,
            ),
        }
    }

    fn message_text(item: &Value) -> String {
        match &item["content"] {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts
                .iter()
                .map(|part| match part["type"].as_str().unwrap() {
                    "input_text" | "output_text" => part["text"].as_str().unwrap(),
                    other => panic!("unexpected content type {other}"),
                })
                .collect(),
            content => panic!("unexpected message content {content}"),
        }
    }

    fn user_messages(protocol: &str, body: &Value) -> Vec<String> {
        let entries = match protocol {
            "completion" => body["messages"].as_array().unwrap(),
            _ => body["input"].as_array().unwrap(),
        };
        entries
            .iter()
            .filter(|entry| entry["role"] == "user")
            .map(message_text)
            .collect()
    }

    #[test]
    fn plan_submit_seals_batch_without_another_model_request() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
            agent.workflow.mode = crate::plan::WorkMode::Plan;
            agent.workflow.plan = Some(crate::plan::PlanDocument::new().unwrap());
            agent
                .session
                .set_tools(agent.tools.all_definitions(agent.workflow.mode));
            let content = "# Create hello\n\n- [ ] Create hello.txt and read back hello.\n";
            let serving = protocol.to_owned();
            let worker = std::thread::spawn(move || {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let _: Value = serde_json::from_reader(request.as_reader()).unwrap();
                tool_call_for(
                    &serving,
                    request,
                    "plan_write",
                    &json!({"title":"Create hello","file_name":"create-canary.md","content":content,"expected_revision":0})
                        .to_string(),
                );
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let _: Value = serde_json::from_reader(request.as_reader()).unwrap();
                let calls = [
                    ("submit", "plan_submit", json!({"expected_revision":1})),
                    ("shell", "bash", json!({"command":"touch forbidden"})),
                    (
                        "rewrite",
                        "plan_write",
                        json!({"title":"Wrong","file_name":"create-canary.md","content":"- [ ] wrong","expected_revision":1}),
                    ),
                ];
                if serving == "completion" {
                    let calls: Vec<_> = calls.into_iter().enumerate().map(|(index,(id,name,args))| json!({"id":id,"type":"function","index":index,"function":{"name":name,"arguments":args.to_string()}})).collect();
                    completion_stream(
                        request,
                        json!({"role":"assistant","content":null,"tool_calls":calls}),
                        "tool_calls",
                    );
                } else {
                    let calls: Vec<_> = calls.into_iter().map(|(id,name,args)| json!({"type":"function_call","id":format!("fc-{id}"),"call_id":id,"name":name,"arguments":args.to_string(),"status":"completed"})).collect();
                    response_stream(request, &calls, true);
                }
                server
            });
            let queue = InputQueue::default();
            queue.begin_run().unwrap();
            let mut results = Vec::new();
            let outcome = agent
                .run(
                    "Plan hello",
                    &CancellationToken::new(),
                    Some(&queue),
                    &mut |event| {
                        if let AgentEvent::ToolFinished { id, result } = event {
                            results.push((id, result));
                        }
                    },
                )
                .unwrap();
            let server = worker.join().unwrap();
            assert_eq!(outcome, RunOutcome::AwaitingPlanReview);
            assert!(server.try_recv().unwrap().is_none());
            assert_eq!(results.len(), 4);
            assert_eq!(results[1].0, "submit");
            assert_eq!(results[1].1["ok"], true);
            for (id, result) in &results[2..] {
                assert!(matches!(id.as_str(), "shell" | "rewrite"));
                assert_eq!(result["error"], "Not executed: plan submitted for review.");
            }
            let plan = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(plan.revision, 1);
            assert_eq!(plan.status, crate::plan::PlanStatus::Ready);
            assert_eq!(plan.file_name.as_deref(), Some("create-canary.md"));
            assert_eq!(
                std::fs::read_to_string(
                    dir.path().join(plan.path().as_deref().unwrap_or_default())
                )
                .unwrap(),
                content
            );
            assert!(dir.path().join(".pp/drafts/create-canary.md").exists());
            assert!(!dir.path().join("forbidden").exists());
            let saved: Value = serde_json::to_value(&agent.session).unwrap();
            let history = &saved["request"][if protocol == "completion" {
                "messages"
            } else {
                "input"
            }];
            let outputs = history
                .as_array()
                .unwrap()
                .iter()
                .filter(|item| item["role"] == "tool" || item["type"] == "function_call_output")
                .count();
            assert_eq!(outputs, 4);
            let id = agent.session_id().unwrap().to_owned();
            let expected = agent.workflow.clone();
            drop(agent);
            let restored =
                persistence::load(&dir.path().join("sessions"), Some(&id), true).unwrap();
            assert_eq!(restored.checkpoint.workflow, expected);
            assert!(server.try_recv().unwrap().is_none());
        }
    }

    #[test]
    fn prepare_plan_task_allocates_only_on_first_non_blank_task() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
        seed_session(&mut agent, EXPLORATION);
        agent.workflow.mode = crate::plan::WorkMode::Plan;
        // 空待命下的空任务（continue/steering 边界）不分配文档。
        agent.prepare_plan_task("").unwrap();
        assert!(agent.workflow.plan.is_none());
        // 首条非空任务建立绑定并记录 origin。
        agent.prepare_plan_task("Research the toggle flow").unwrap();
        let plan = agent.workflow.plan.as_ref().unwrap();
        assert_eq!(plan.revision, 0);
        assert_eq!(plan.origin_prompt, "Research the toggle flow");
        // 后续空任务沿用当前绑定，不新建、不重置 origin。
        agent.prepare_plan_task("").unwrap();
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().origin_prompt,
            "Research the toggle flow"
        );
    }

    #[test]
    fn plan_protocols_deny_side_effects_and_continue_reading() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("canary"), "original\n").unwrap();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
            let mut document = crate::plan::PlanDocument::new().unwrap();
            document.origin_prompt = "Research canary".into();
            agent.workflow.mode = crate::plan::WorkMode::Plan;
            agent.workflow.plan = Some(document);
            agent
                .session
                .set_tools(agent.tools.all_definitions(agent.workflow.mode));
            let serving = protocol.to_owned();
            let worker = std::thread::spawn(move || {
                for (name, args) in [
                    ("write", json!({"path":"canary","content":"changed"})),
                    (
                        "edit",
                        json!({"path":"canary","edits":[{"oldText":"original","newText":"changed"}]}),
                    ),
                    ("bash", json!({"command":"touch marker"})),
                    (
                        "bash",
                        json!({"command":"touch marker","mode":"background"}),
                    ),
                    ("bash_task", json!({"task_id":"unknown","action":"status"})),
                    ("plan_search", json!({"query":"original","path":"canary"})),
                    ("find", json!({"args":[".","-exec","touch","marker",";"]})),
                    ("mcp__unseen__write", json!({})),
                    ("read", json!({"path":"canary"})),
                ] {
                    let mut request = server
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap()
                        .unwrap();
                    let _: Value = serde_json::from_reader(request.as_reader()).unwrap();
                    tool_call_for(&serving, request, name, &args.to_string());
                }
                let request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                final_for(&serving, request, "Read-only research completed", true);
            });
            let mut results = Vec::new();
            let result = agent.run(
                "Research canary",
                &CancellationToken::new(),
                None,
                &mut |event| {
                    if let AgentEvent::ToolFinished { result, .. } = event {
                        results.push(result);
                    }
                },
            );
            assert_eq!(result.unwrap(), RunOutcome::Completed);
            worker.join().unwrap();
            assert_eq!(results.len(), 9);
            for result in &results[..8] {
                assert_eq!(
                    *result,
                    json!({"ok":false,"error":"Tool is not available in Plan mode."})
                );
            }
            assert_eq!(results[8]["content"], "original\n");
            assert_eq!(
                std::fs::read(dir.path().join("canary")).unwrap(),
                b"original\n"
            );
            assert!(!dir.path().join("marker").exists());
        }
    }

    #[test]
    fn runtime_input_steering_applies_at_committed_boundary_and_replays_once() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
            let queue = InputQueue::default();
            queue.begin_run().unwrap();
            let (tx, rx) = std::sync::mpsc::channel::<Value>();
            let serving = protocol.to_owned();
            let server_thread = std::thread::spawn(move || {
                // 第一轮：最终回复结束；补充在请求进行中入队，须在完整提交边界后消费。
                for text in ["first reply", "second reply", "resumed reply"] {
                    let mut request = server
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap()
                        .unwrap();
                    let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
                    tx.send(body).unwrap();
                    final_for(&serving, request, text, true);
                }
            });
            let mut enqueued = false;
            let mut accepted = Vec::new();
            let outcome = agent
                .run(
                    "initial prompt",
                    &CancellationToken::new(),
                    Some(&queue),
                    &mut |event| match event {
                        AgentEvent::ModelRequestStarted if !enqueued => {
                            enqueued = true;
                            queue
                                .enqueue(
                                    input::PendingKind::Steering,
                                    "steer once".into(),
                                    None,
                                    false,
                                )
                                .unwrap();
                        }
                        AgentEvent::UserInputAccepted { id, text } => accepted.push((id, text)),
                        _ => {}
                    },
                )
                .unwrap();
            assert_eq!(outcome, RunOutcome::Completed);
            assert_eq!(accepted, vec![(1, "steer once".to_owned())]);
            let first = rx.recv().unwrap();
            let second = rx.recv().unwrap();
            assert_eq!(user_messages(protocol, &first), ["initial prompt"]);
            assert_eq!(
                user_messages(protocol, &second),
                ["initial prompt", "steer once"]
            );
            assert!(
                second.to_string().contains("first reply"),
                "follow-on request must replay the committed native history"
            );
            assert_eq!(queue.snapshot_since(None).unwrap().unwrap().steering(), 0);
            // 任务结束已关闭队列：此后入队的补充不得被 continue_session 领取。
            queue
                .enqueue(
                    input::PendingKind::Steering,
                    "left pending".into(),
                    None,
                    false,
                )
                .unwrap();
            let outcome = agent
                .continue_session(&CancellationToken::new(), Some(&queue), &mut |event| {
                    if let AgentEvent::UserInputAccepted { .. } = event {
                        panic!("closed queue must not be claimed by a new session")
                    }
                })
                .unwrap();
            assert_eq!(outcome, RunOutcome::Completed);
            let third = rx.recv().unwrap();
            assert_eq!(
                user_messages(protocol, &third),
                ["initial prompt", "steer once"]
            );
            assert_eq!(queue.snapshot_since(None).unwrap().unwrap().steering(), 1);
            server_thread.join().unwrap();
            // 已应用的输入各以普通 User 记录持久化一次，事件不入 journal。
            let id = agent.session_id().unwrap().to_owned();
            drop(agent);
            let restored =
                persistence::load(&dir.path().join("sessions"), Some(&id), true).unwrap();
            let users = restored
                .replay
                .iter()
                .filter_map(|record| match record {
                    persistence::ReplayRecord::User(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(users, vec!["initial prompt", "steer once"]);
        }
    }

    #[test]
    fn runtime_input_acceptance_arrivals_wait_for_the_next_committed_boundary() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
            let queue = InputQueue::default();
            queue.begin_run().unwrap();
            let serving = protocol.to_owned();
            let handle = std::thread::spawn(move || {
                let mut bodies = Vec::new();
                for text in ["first", "second", "third"] {
                    let mut request = server
                        .recv_timeout(std::time::Duration::from_secs(5))
                        .unwrap()
                        .unwrap();
                    bodies.push(serde_json::from_reader::<_, Value>(request.as_reader()).unwrap());
                    final_for(&serving, request, text, true);
                }
                bodies
            });
            let mut initial_started = false;
            agent
                .run(
                    "initial",
                    &CancellationToken::new(),
                    Some(&queue),
                    &mut |event| match event {
                        AgentEvent::AssistantStarted if !initial_started => {
                            initial_started = true;
                            queue
                                .enqueue(input::PendingKind::Steering, "S1".into(), None, false)
                                .unwrap();
                        }
                        AgentEvent::UserInputAccepted { text, .. } if text == "S1" => {
                            queue
                                .enqueue(input::PendingKind::Steering, "S2".into(), None, false)
                                .unwrap();
                        }
                        _ => {}
                    },
                )
                .unwrap();
            let bodies = handle.join().unwrap();
            assert_eq!(user_messages(protocol, &bodies[0]), ["initial"]);
            assert_eq!(user_messages(protocol, &bodies[1]), ["initial", "S1"]);
            assert_eq!(user_messages(protocol, &bodies[2]), ["initial", "S1", "S2"]);
        }
    }

    #[test]
    fn runtime_input_claimed_batch_cancel_restores_only_unaccepted_items() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            let queue = InputQueue::default();
            queue.begin_run().unwrap();
            for text in ["accepted", "unaccepted one", "unaccepted two"] {
                queue
                    .enqueue(input::PendingKind::Steering, text.into(), None, false)
                    .unwrap();
            }
            agent.append_user("initial");
            let mut initial = Some("initial");
            let cancellation = CancellationToken::new();
            let mut accepted = Vec::new();
            let error = agent
                .apply_pending(
                    queue.claim_steering(false).unwrap(),
                    Some(&queue),
                    &mut initial,
                    &cancellation,
                    &mut |event| {
                        if let AgentEvent::UserInputAccepted { text, .. } = event {
                            accepted.push(text);
                            cancellation.cancel();
                        }
                    },
                )
                .unwrap_err();
            assert!(error.is::<Cancelled>());
            assert!(initial.is_none());
            assert_eq!(accepted, ["accepted"]);
            assert_eq!(
                queue.claim_next().unwrap().unwrap().source,
                "unaccepted one"
            );
            assert_eq!(
                queue.claim_next().unwrap().unwrap().source,
                "unaccepted two"
            );
            assert!(queue.claim_next().unwrap().is_none());
            let id = agent.session_id().unwrap().to_owned();
            drop(agent);
            let restored =
                persistence::load(&dir.path().join("sessions"), Some(&id), true).unwrap();
            let users: Vec<_> = restored
                .replay
                .iter()
                .filter_map(|record| match record {
                    persistence::ReplayRecord::User(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(users, ["initial", "accepted"]);
        }
    }

    #[test]
    fn runtime_input_persistence_failure_rolls_back_append_and_restores_batch() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            // A real filesystem failure at journal creation, without changing process limits.
            std::fs::write(dir.path().join("sessions"), "not a directory").unwrap();
            let before = serde_json::to_value(&agent.session).unwrap();
            let queue = InputQueue::default();
            queue.begin_run().unwrap();
            for text in ["one", "two"] {
                queue
                    .enqueue(input::PendingKind::Steering, text.into(), None, false)
                    .unwrap();
            }
            let mut accepted = Vec::new();
            let error = agent
                .apply_pending(
                    queue.claim_steering(false).unwrap(),
                    Some(&queue),
                    &mut None,
                    &CancellationToken::new(),
                    &mut |event| {
                        if let AgentEvent::UserInputAccepted { text, .. } = event {
                            accepted.push(text);
                        }
                    },
                )
                .unwrap_err();
            assert!(error.to_string().contains("persisting session"));
            assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
            assert!(accepted.is_empty());
            assert!(agent.journal.check().is_err());
            assert_eq!(queue.claim_next().unwrap().unwrap().source, "one");
            assert_eq!(queue.claim_next().unwrap().unwrap().source, "two");
            assert!(queue.claim_next().unwrap().is_none());
        }
    }

    #[test]
    fn runtime_input_queued_during_summary_waits_for_next_normal_request() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let mut agent =
                runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
            // 首个完整交换是可压缩内容；最后的完整交换与待回复后缀必须保留。
            let big = "filler ".repeat(20000);
            seed_exchange(&mut agent, &big, "ack");
            seed_exchange(&mut agent, "warmup", "ready");
            // 压缩后用量 = 会话减去首个交换加摘要，必须低于阈值且留出余量。
            let total_bytes = serde_json::to_value(&agent.session)
                .unwrap()
                .to_string()
                .len() as u64;
            let big_bytes = serde_json::to_string(&big).unwrap().len() as u64;
            assert!(
                total_bytes / 4 < big_bytes,
                "test needs a dominant compactable exchange"
            );
            let prompt_bytes = serde_json::to_string("final question").unwrap().len() as u64;
            let pre_tokens = (total_bytes + prompt_bytes).div_ceil(3);
            // 摘要请求本身加输出上限必须放进窗口，且阈值仍要触发压缩。
            let window = pre_tokens + 5120;
            agent.current_model.context_window = Some(window);
            let queue = InputQueue::default();
            queue.begin_run().unwrap();
            let (tx, rx) = std::sync::mpsc::channel::<Value>();
            let serving = protocol.to_owned();
            let server_thread = std::thread::spawn(move || {
                // 请求 1：临时摘要请求，不得包含摘要期间入队的补充。
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
                assert!(
                    !body.to_string().contains("after summary"),
                    "summary request consumed queued input"
                );
                tx.send(body).unwrap();
                final_for(&serving, request, "compact summary", true);
                // 请求 2：摘要后的普通请求，补充在该边界应用。
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
                tx.send(body).unwrap();
                final_for(&serving, request, "final answer", true);
            });
            let mut accepted = Vec::new();
            let outcome = agent
                .run(
                    "final question",
                    &CancellationToken::new(),
                    Some(&queue),
                    &mut |event| match event {
                        AgentEvent::CompactionStarted { automatic: true } => {
                            queue
                                .enqueue(
                                    input::PendingKind::Steering,
                                    "after summary".into(),
                                    None,
                                    false,
                                )
                                .unwrap();
                        }
                        AgentEvent::UserInputAccepted { id, text } => accepted.push((id, text)),
                        _ => {}
                    },
                )
                .unwrap();
            assert_eq!(outcome, RunOutcome::Completed);
            server_thread.join().unwrap();
            drop(rx.recv().unwrap());
            let normal = rx.recv().unwrap();
            assert_eq!(
                user_messages(protocol, &normal).last().map(String::as_str),
                Some("after summary")
            );
            assert_eq!(accepted, vec![(1, "after summary".to_owned())]);
            let snapshot = queue.snapshot_since(None).unwrap().unwrap();
            assert_eq!((snapshot.steering(), snapshot.follow_up()), (0, 0));
        }
    }

    fn seed_exchange(agent: &mut Agent, user: &str, assistant: &str) {
        agent.append_user(user);
        match &mut agent.session {
            Session::Completion(request) => {
                request.messages.push(ChatCompletionRequestMessage::Assistant(
                    async_openai::types::chat::ChatCompletionRequestAssistantMessage {
                        content: Some(
                            async_openai::types::chat::ChatCompletionRequestAssistantMessageContent::Text(
                                assistant.into(),
                            ),
                        ),
                        ..Default::default()
                    },
                ));
            }
            Session::Response(request) => {
                response_input(request).push(InputItem::EasyMessage(EasyInputMessage {
                    role: Role::Assistant,
                    content: assistant.into(),
                    ..Default::default()
                }));
            }
        }
    }
}
