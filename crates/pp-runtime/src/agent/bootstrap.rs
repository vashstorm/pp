//! Agent 构造入口、共享模型状态与提示词快照刷新。
use super::{Agent, Session, debug, persistence::SessionJournal};
use anyhow::{Context, Result, ensure};
use async_openai::types::chat::ChatCompletionRequestMessage;
use pp_config::config::ModelCatalog;
use pp_config::config::ModelChoice;
use pp_config::model_state::{ModelState, Selection};
#[cfg(debug_assertions)]
use pp_core::cancellation;
use pp_llm::OpenAiClient;
use pp_tools::Tools;
use std::path::PathBuf;

impl Agent {
    /// 用户级 `~/.pp/agent/hooks.yml` 与启动目录 `.pp/hooks.yml` 合并编译。
    ///
    /// 配置在 Agent 构造时冻结：文件缺失视为无 hooks，格式、结构或正则错误
    /// 不静默忽略。脚本本身只在匹配时启动。
    pub(in crate::agent) fn compile_hooks(
        catalog: &ModelCatalog,
        cwd: &std::path::Path,
    ) -> Result<std::sync::Arc<crate::hooks::Runner>> {
        let user = catalog.hooks_path();
        let project = cwd.join(".pp/hooks.yml");
        Ok(std::sync::Arc::new(crate::hooks::Runner::load(
            &user, &project,
        )?))
    }

    /// 按当前 settings 安装一次共享 Registry；配置无效只让 LSP 不可用，
    /// 不阻止 Agent 启动。
    pub(in crate::agent) fn install_lsp(tools: &mut Tools, catalog: &ModelCatalog) {
        if !catalog.lsp_settings().enabled || tools.lsp_registry().is_some() {
            return;
        }
        let registry = pp_lsp::Registry::new(
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
        // 单测各自持有隔离管理器：Plan 交接测试的 `/mcp reload` 读的是本 catalog
        // 指向的临时 `mcp.json`，不能落到真实用户配置或 HOME 下的 Claude/Codex 来源。
        let cwd = tools.cwd().to_owned();
        tools.mcp = std::sync::Arc::new(pp_mcp::McpManager::new_isolated(catalog.mcp_path(), cwd));
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

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn new_with_prompt(
        models: std::sync::Arc<ModelState>,
        tools: Tools,
        sessions_dir: PathBuf,
        lock_dir: PathBuf,
        prompt_source: crate::system_prompt::Snapshot,
        stats: impl Into<pp_stats::Database>,
    ) -> Result<Self> {
        Self::new_with_prompt_with_options(
            models,
            tools,
            sessions_dir,
            lock_dir,
            prompt_source,
            stats,
            true,
            false,
        )
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn new_with_prompt_with_options(
        models: std::sync::Arc<ModelState>,
        mut tools: Tools,
        sessions_dir: PathBuf,
        lock_dir: PathBuf,
        prompt_source: crate::system_prompt::Snapshot,
        stats: impl Into<pp_stats::Database>,
        hooks_enabled: bool,
        ephemeral: bool,
    ) -> Result<Self> {
        tools.reset_project_instructions([]);
        let catalog = models.catalog_snapshot();
        let (config, selection) = Self::resolve_selection(&models)?;
        let think_level = selection.effort;
        let model_source = models.source();
        tools.limits = config.settings.tool_limits();
        tools.configure_eval(catalog.eval_settings());
        tools.configure_web_search(catalog.web_search_settings().clone());
        tools.configure_mcp(catalog.mcp_settings());
        tools.permission.set_config(config.settings.tools.clone());
        let client = OpenAiClient::new(&config)?;
        // 进程级单例：启动期后台建连，不再阻塞首个界面；首个任务入口等它结束。
        // 连接失败不是启动失败：失败只落到 `/mcp list` 与提示行。
        if tools.allows_any_mcp() {
            tools.mcp.start_connect(pp_mcp::Access::Full);
        }
        pp_core::logging::perf::mark("mcp_connect_started");
        if tools.allows_lsp_lifecycle() {
            Self::install_lsp(&mut tools, &catalog);
        }
        pp_core::logging::perf::mark("lsp_installed");
        let current_model = ModelChoice {
            reference: selection.reference.clone(),
            model: config.model.clone(),
            protocol: config.protocol,
            thinking: config.thinking,
            context_window: config.context_window,
        };
        let edit_mode = tools.edit_mode();
        let definitions = tools.all_definitions(pp_core::plan::WorkMode::Execute);
        let session_date = super::current_session_date();
        // 项目文件在任务开始时加载，读取失败不会让整个交互会话永久不可用。
        let context = crate::system_prompt::RenderContext::new(
            tools.cwd(),
            &config.model,
            edit_mode,
            "execute",
            definitions.iter().map(|tool| tool.name.clone()).collect(),
        )
        .with_date(session_date.clone())
        .with_sandbox(&tools.sandbox_status());
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
        // 无效正则或结构错误阻止 Agent 构造；脚本本身只在匹配时启动。
        let hooks = if hooks_enabled {
            Self::compile_hooks(&catalog, tools.cwd())?
        } else {
            std::sync::Arc::new(crate::hooks::Runner::empty())
        };
        let initial_system = system.clone();
        let mut session = Session::new_with_tools(
            config.model,
            config.protocol,
            initial_system,
            Some(definitions),
        );
        session.set_think_level(think_level);
        session.apply_cache_policy(
            config.prompt_cache_key,
            config.prompt_cache_retention,
            || super::generate_prompt_cache_key(None),
        );
        let journal = if ephemeral {
            SessionJournal::ephemeral(sessions_dir, lock_dir, stats)
        } else {
            SessionJournal::with_locks(sessions_dir, lock_dir, stats)
        };
        let mut agent = Self {
            role_route: None,
            client,
            runtime_models: None,
            subagent_control: Default::default(),
            subagent: None,
            subagents_enabled: true,
            task_status: Default::default(),
            context_snapshot: None,
            session,
            tools,
            auto_compact: config.settings.auto_compact,
            auto_compact_threshold_percent: config.settings.auto_compact_threshold_percent,
            auto_prune: config.settings.auto_prune,
            auto_prune_threshold_percent: config.settings.auto_prune_threshold_percent,
            last_request: None,
            last_request_raw_tokens: None,
            session_output_tokens: 0,
            thinking_history: Vec::new(),
            current_model,
            think_level,
            prompt_cache_key_enabled: config.prompt_cache_key,
            prompt_cache_retention: config.prompt_cache_retention,
            models,
            model_source,
            catalog,
            base_system: system,
            prompt_source: Some(prompt_source),
            pending_instructions: None,
            pending_project_load: Vec::new(),
            reported_project_load: Default::default(),
            session_date,
            static_project_prompt: None,
            delivered_project_docs: Default::default(),
            request_skills: Vec::new(),
            journal,
            replay: Vec::new(),
            debug: debug::DebugState::new(config.settings.debug && !ephemeral, false),
            workflow: pp_core::plan::WorkflowState::default(),
            plan_cleanup_error: None,
            plan_lease: None,
            lsp_injected: Default::default(),
            hooks,
            hook_agent_type: None,
            ephemeral,
            max_turns: None,
            output_schema: None,
            auto_review: None,
            ephemeral_subagent_dir: None,
        };
        agent.init_auto_review();
        Ok(agent)
    }

    #[cfg(debug_assertions)]
    pub fn new_mock(
        catalog: ModelCatalog,
        mut tools: Tools,
        sessions_dir: PathBuf,
        lock_dir: PathBuf,
        scripts: pp_llm::MockScripts,
    ) -> Result<Self> {
        // mock 场景固定空管理器：不读取真实用户 MCP 定义。
        tools.mcp = std::sync::Arc::new(pp_mcp::McpManager::empty());
        let mut agent = Self::new_with_prompt(
            ModelState::shared(catalog),
            tools,
            sessions_dir.clone(),
            lock_dir,
            crate::system_prompt::Snapshot::bundled(),
            sessions_dir.join(".stats/stats.db"),
        )?;
        agent.client.set_mock_scripts(scripts)?;
        agent.sync_auto_review();
        Ok(agent)
    }

    #[cfg(debug_assertions)]
    pub fn stop_mock(&mut self) -> Result<()> {
        let mut failure: Option<anyhow::Error> = None;
        for result in [
            self.stop_subagents(),
            self.tools.stop_background_tasks(),
            self.tools.stop_eval(),
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

    pub fn permission_mode(&self) -> pp_config::config::PermissionMode {
        self.tools.permission.snapshot()
    }

    pub fn permission_gate(&self) -> pp_tools::permission::Gate {
        self.tools.permission.clone()
    }

    pub fn sandbox(&self) -> std::sync::Arc<pp_sandbox::Sandbox> {
        std::sync::Arc::clone(self.tools.sandbox())
    }

    pub fn sync_permission_mode(&mut self) -> Result<()> {
        self.refresh_instructions_changed()?;
        Ok(())
    }

    pub fn set_permission_mode(
        &mut self,
        mode: Option<pp_config::config::PermissionMode>,
    ) -> Result<()> {
        self.tools.permission.select(mode);
        self.tools.sandbox().set_mode_override(None);
        self.sync_permission_mode()?;
        Ok(())
    }

    pub fn list_permission_rules(&self) -> Result<String> {
        self.tools.permission.list_rules()
    }

    pub fn revoke_permission_rule(&self, target: &str) -> Result<()> {
        self.tools.permission.revoke_rule(target)
    }

    pub fn sandbox_status(&self) -> pp_sandbox::SandboxStatus {
        self.tools.sandbox_status()
    }

    /// `--sandbox` 或 `/sandbox <mode>` 的会话级内存覆盖，刷新提示词与工具描述。
    pub fn set_sandbox_override(
        &mut self,
        mode: Option<pp_config::config::SandboxMode>,
    ) -> Result<()> {
        let previous = self.tools.sandbox().mode_override();
        self.tools.sandbox().set_mode_override(mode);
        if matches!(
            mode,
            Some(
                pp_config::config::SandboxMode::WorkspaceWrite
                    | pp_config::config::SandboxMode::ReadOnly
            )
        ) && let Err(error) = self.tools.sandbox().policy()
        {
            self.tools.sandbox().set_mode_override(previous);
            return Err(error);
        }
        self.refresh_instructions_changed()?;
        self.session
            .set_tools(self.tools.all_definitions(self.workflow.mode));
        Ok(())
    }

    /// `/setting` 写回 `tools.sandbox.mode` 后同步运行时默认配置。
    pub fn update_sandbox_mode_from_settings(
        &mut self,
        mode: pp_config::config::SandboxMode,
    ) -> Result<()> {
        self.tools.sandbox().update_settings_mode(mode);
        self.refresh_instructions_changed()?;
        self.session
            .set_tools(self.tools.all_definitions(self.workflow.mode));
        Ok(())
    }

    /// `/setting` 写回 `webSearch` 配置后同步工具开关、搜索设置与系统提示词。
    pub fn update_web_search_settings(
        &mut self,
        settings: pp_config::config::WebSearchSettings,
    ) -> Result<()> {
        self.tools.configure_web_search(settings);
        self.session
            .set_tools(self.tools.all_definitions(self.workflow.mode));
        self.refresh_instructions_changed()?;
        Ok(())
    }

    /// 权限 handler 镜像 ask：不改变 schema 列表，无需刷新工具定义。
    pub fn set_permission_handler(&mut self, handler: pp_tools::permission::PermissionHandler) {
        self.tools.permission.set_handler(handler);
    }

    pub(in crate::agent) fn refresh_instructions(&mut self) -> Result<()> {
        let _ = self.refresh_instructions_changed()?;
        Ok(())
    }

    pub(in crate::agent) fn refresh_instructions_changed(&mut self) -> Result<bool> {
        let definitions = self.tools.all_definitions(self.workflow.mode);
        let names = definitions.iter().map(|d| d.name.clone()).collect();
        let documents = self.tools.project_documents()?;
        let scope_summary = self.tools.project_scope_summary();
        let rendered_all = scope_summary + &pp_tools::project_instructions::render(&documents);

        // 已有对话历史且已冻结静态项目前缀时，保持 system/instructions 逐字节不变，
        // 仅把新增或变更的 `AGENTS.md` 以 developer 消息增量追加到尾部，保护 Prompt Cache。
        let use_dynamic_delta = self.session.has_history() && self.static_project_prompt.is_some();
        let system_project = if use_dynamic_delta {
            self.static_project_prompt.clone().unwrap_or_default()
        } else {
            self.static_project_prompt = Some(rendered_all.clone());
            self.delivered_project_docs = documents
                .iter()
                .map(|doc| (doc.path.clone(), doc.content.clone()))
                .collect();
            rendered_all
        };

        let (system, skills) = self.compose_prompt_with_project(
            &crate::subagent::catalog::workflow_instructions(&self.workflow),
            self.workflow.mode_name(),
            &self.current_model.model,
            names,
            &system_project,
        )?;
        let mut session_changed = false;
        if self.session.update_tools(definitions) {
            self.journal.tools_changed();
            session_changed = true;
        }
        self.request_skills = skills;
        self.reported_project_load.retain(|path, _| {
            documents
                .iter()
                .any(|document| document.path.to_string_lossy() == path.as_str())
        });
        // 快照按引用计数共享；只为本轮新增或变化的规则建立待发布副本。
        self.pending_project_load = documents
            .iter()
            .filter(|document| {
                self.reported_project_load
                    .get(&document.path.display().to_string())
                    != Some(&document.content)
            })
            .cloned()
            .collect();

        if use_dynamic_delta {
            let delta_docs: Vec<pp_tools::project_instructions::Document> = documents
                .iter()
                .filter(|doc| self.delivered_project_docs.get(&doc.path) != Some(&doc.content))
                .cloned()
                .collect();
            let removed_paths: Vec<std::path::PathBuf> = self
                .delivered_project_docs
                .keys()
                .filter(|path| !documents.iter().any(|doc| &doc.path == *path))
                .cloned()
                .collect();
            if !delta_docs.is_empty() || !removed_paths.is_empty() {
                let mut text = String::from(super::PROJECT_INSTRUCTIONS_PREFIX);
                if !delta_docs.is_empty() {
                    text.push_str(&pp_tools::project_instructions::render(&delta_docs));
                }
                for path in &removed_paths {
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&format!(
                        "[Project instructions removed: {}]\n",
                        path.display()
                    ));
                }
                super::append_developer_to(&mut self.session, text);
                if self.journal.id().is_some() {
                    self.journal.commit((&self.session).into())?;
                }
                for doc in delta_docs {
                    self.delivered_project_docs.insert(doc.path, doc.content);
                }
                for path in removed_paths {
                    self.delivered_project_docs.remove(&path);
                }
                session_changed = true;
            }
        }

        let unchanged = match &self.session {
            Session::Completion(request) => {
                matches!(request.messages.first(), Some(ChatCompletionRequestMessage::System(message)) if matches!(&message.content, async_openai::types::chat::ChatCompletionRequestSystemMessageContent::Text(text) if text == &system))
            }
            Session::Response(request) => request.instructions.as_deref() == Some(system.as_str()),
        };
        if unchanged {
            return Ok(session_changed);
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
        Ok(true)
    }
}
