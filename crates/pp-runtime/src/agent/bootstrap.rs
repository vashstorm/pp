//! Agent 构造入口、共享模型状态与提示词快照刷新。
use super::{Agent, Session, debug, persistence::SessionJournal};
use crate::tools::Tools;
use anyhow::{Context, Result, ensure};
use async_openai::types::chat::ChatCompletionRequestMessage;
use pp_config::config::ModelCatalog;
use pp_config::config::ModelChoice;
use pp_config::model_state::{ModelState, Selection};
#[cfg(debug_assertions)]
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_llm::OpenAiClient;
use std::path::PathBuf;

impl Agent {
    /// 按当前 settings 安装一次共享 Registry；配置无效只让 LSP 不可用，
    /// 不阻止 Agent 启动。
    pub(in crate::agent) fn install_lsp(tools: &mut Tools, catalog: &ModelCatalog) {
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

    /// 测试构造入口：统计库随会话目录落在临时工作区，绝不写开发者真实 `~/.pp/stats`。
    #[cfg(test)]
    pub(crate) fn new(
        catalog: ModelCatalog,
        mut tools: Tools,
        sessions_dir: PathBuf,
    ) -> Result<Self> {
        let lock_dir = sessions_dir.join(".locks");
        let stats_db = sessions_dir.join(".stats/stats.db");
        // 单测各自持有配置化管理器：Plan 交接测试的 `/mcp reload` 读的是本
        // catalog 指向的临时 `mcp.json`，不能落到真实用户配置。
        let cwd = tools.cwd().to_owned();
        tools.mcp = std::sync::Arc::new(crate::mcp::McpManager::new(catalog.mcp_path(), cwd));
        Self::new_with_prompt(
            ModelState::shared(catalog),
            tools,
            sessions_dir,
            lock_dir,
            crate::system_prompt::Snapshot::bundled(),
            stats_db,
        )
    }

    /// 进程内唯一模型状态的只读句柄；UI 用它启动 `model_control::Service`。
    pub fn models(&self) -> std::sync::Arc<ModelState> {
        std::sync::Arc::clone(&self.models)
    }

    /// 决议当前应使用的模型：`a`（手动覆盖）优先，否则配置默认；不自动套用任何角色。
    pub(in crate::agent) fn resolve_selection(
        models: &ModelState,
    ) -> Result<(pp_config::config::Config, Selection)> {
        let catalog = models.catalog_snapshot();
        let selection = models.resolve()?;
        let config = catalog.resolve(&selection.reference)?;
        ensure!(
            config.thinking.supports(selection.effort),
            "thinking level is not supported by the configured model"
        );
        Ok((config, selection))
    }

    pub(crate) fn new_with_prompt(
        models: std::sync::Arc<ModelState>,
        mut tools: Tools,
        sessions_dir: PathBuf,
        lock_dir: PathBuf,
        prompt_source: crate::system_prompt::Snapshot,
        stats_db: PathBuf,
    ) -> Result<Self> {
        tools.reset_project_instructions([]);
        let catalog = models.catalog_snapshot();
        let (config, selection) = Self::resolve_selection(&models)?;
        let think_level = selection.effort;
        let model_source = models.source();
        tools.limits = config.settings.tool_limits();
        tools.approval.set_config(config.settings.tools.clone());
        let client = OpenAiClient::new(&config)?;
        // 进程级单例：首次连接失败即启动失败；管理器保留未加载状态，
        // 下次构造（如登录后重建 Agent）自动重试，这里不手动回收。
        tools
            .mcp
            .connect_enabled(&CancellationToken::new(), crate::mcp::Access::Full)?;
        Self::install_lsp(&mut tools, &catalog);
        let current_model = ModelChoice {
            reference: selection.reference.clone(),
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
            role_route: None,
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
            current_model,
            think_level,
            models,
            model_source,
            catalog,
            base_system: system,
            prompt_source: Some(prompt_source),
            pending_instructions: None,
            pending_project_load: Vec::new(),
            reported_project_load: Default::default(),
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
        mut tools: Tools,
        sessions_dir: PathBuf,
        lock_dir: PathBuf,
        scripts: pp_llm::MockScripts,
    ) -> Result<Self> {
        // mock 场景固定空管理器：不读取真实用户 MCP 定义。
        tools.mcp = std::sync::Arc::new(crate::mcp::McpManager::empty());
        let mut agent = Self::new_with_prompt(
            ModelState::shared(catalog),
            tools,
            sessions_dir.clone(),
            lock_dir,
            crate::system_prompt::Snapshot::bundled(),
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
            self.tools.mcp.shutdown(),
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

    pub(in crate::agent) fn refresh_instructions(&mut self) -> Result<()> {
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
        self.reported_project_load.retain(|path, _| {
            project
                .iter()
                .any(|document| document.path.to_string_lossy() == path.as_str())
        });
        self.pending_project_load = project
            .into_iter()
            .filter(|document| {
                self.reported_project_load
                    .get(&document.path.display().to_string())
                    != Some(&document.content)
            })
            .collect();
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
}
