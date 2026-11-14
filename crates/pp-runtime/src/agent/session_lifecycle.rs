//! 会话恢复、树导航与新建会话根的生命周期切换。
use super::{
    Agent, Session, TreeNavigation, debug, history,
    persistence::{self, Metadata},
    plan_sync,
    turn::{reported_project_load, response_input},
};
use crate::tools::Tools;
use anyhow::{Context, Result};
use async_openai::types::chat::ChatCompletionRequestMessage;
use pp_config::config::ModelChoice;
use pp_config::model_state::ModelState;
use pp_config::models::Protocol;
use pp_core::cancellation::CancellationToken;
use pp_core::model::ModelSource;
use pp_llm::OpenAiClient;
use std::path::PathBuf;

impl Agent {
    pub fn restore(
        models: std::sync::Arc<ModelState>,
        mut tools: Tools,
        restored: persistence::RestoredSession,
    ) -> Result<Self> {
        let persistence::RestoredSession {
            mut checkpoint,
            mut journal,
            mut replay,
        } = restored;
        tools.reset_project_instructions(reported_project_load(&replay).keys().map(PathBuf::from));
        let catalog = models.catalog_snapshot();
        let (config, resolved) = Self::resolve_selection(&models)?;
        let model_source = models.source();
        // 旧 checkpoint 的角色路由不再参与恢复；已失效的路由必须落一次清理。
        let had_route = checkpoint.role_route.take().is_some();
        let compatible = match &checkpoint.session {
            Session::Completion(_) => config.protocol == Protocol::Completion,
            Session::Response(request) => {
                config.protocol == Protocol::Response
                    && checkpoint.model_reference == resolved.reference
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
        // 覆盖态用覆盖的档位；否则保留会话已保存且仍受支持的档位。
        let think_level = if model_source == ModelSource::Override {
            resolved.effort
        } else if config.thinking.supports(checkpoint.think_level) {
            checkpoint.think_level
        } else {
            resolved.effort
        };
        let model_changed = checkpoint.model_reference != resolved.reference
            || checkpoint.think_level != think_level
            || !compatible;
        checkpoint.model_reference.clone_from(&resolved.reference);
        checkpoint.think_level = think_level;
        checkpoint.session.set_model(&config.model);
        checkpoint.session.set_think_level(think_level);
        if let Some(snapshot) =
            plan_sync::sync_restored_todos(&tools, &mut journal, &mut checkpoint.workflow, &replay)?
        {
            replay.push(persistence::ReplayRecord::Event(snapshot));
        }
        let client = OpenAiClient::new(&config)?;
        // hook 配置不从 checkpoint 恢复：始终重新读取用户级与项目级 hooks 文件。
        let hooks = Self::compile_hooks(&catalog, tools.cwd())?;
        tools.limits = checkpoint.tool_limits;
        // 恢复不承诺旧进程的临时管道正文：显式换一个空 registry。
        tools.reset_pipe_store();
        if let Some(id) = journal.id() {
            let _ = tools.artifacts.bind(journal.directory(), id);
        }
        // 审批配置沿用当前 settings.yml，不恢复 checkpoint 中的旧值；
        // 会话级授权本来就不序列化。
        tools.approval.set_config(config.settings.tools.clone());
        // 复用进程级单例：跨 `/resume` 不重连也不重读 `mcp.json`，只按目标
        // 模式补连尚未连接的服务。
        tools.mcp.connect_enabled(
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
            role_route: None,
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
            models,
            model_source,
            current_model: ModelChoice {
                reference: checkpoint.model_reference,
                model: config.model,
                protocol: config.protocol,
                thinking: config.thinking,
                context_window: config.context_window,
            },
            catalog,
            think_level,
            base_system: checkpoint.base_system,
            prompt_source: checkpoint.prompt_source,
            pending_instructions: None,
            pending_project_load: Vec::new(),
            reported_project_load: reported_project_load(&replay),
            request_skills: Vec::new(),
            journal,
            replay,
            debug: debug::DebugState::new(config.settings.debug, true),
            workflow: checkpoint.workflow,
            plan_cleanup_error: None,
            plan_lease: None,
            lsp_injected: Default::default(),
            hooks,
            hook_agent_type: None,
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
        if had_route || model_changed {
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
                .checkpoint(&metadata, (&agent.session).into(), &agent.workflow)?;
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
        self.pending_project_load.clear();
        self.reported_project_load = reported_project_load(&replay);
        self.tools
            .reset_project_instructions(self.reported_project_load.keys().map(PathBuf::from));
        Ok(TreeNavigation { replay, draft })
    }
    pub fn take_session_notice(&mut self) -> Option<String> {
        self.journal.take_notice()
    }
    pub fn take_replay(&mut self) -> Vec<persistence::ReplayRecord> {
        std::mem::take(&mut self.replay)
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
        self.pending_project_load.clear();
        self.reported_project_load.clear();
        self.tools.reset_project_instructions([]);
        match &mut self.session {
            Session::Completion(request) => request.messages.truncate(1),
            Session::Response(request) => response_input(request).clear(),
        }
    }
}
