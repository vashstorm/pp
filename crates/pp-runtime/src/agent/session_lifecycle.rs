//! Session restore, tree navigation and lifecycle switches to a new session root.
use super::{
    Agent, Session, TreeNavigation, debug, history,
    persistence::{self, Metadata},
    plan_sync,
    turn::{reported_project_load, response_input},
};
use anyhow::{Context, Result};
use async_openai::types::chat::ChatCompletionRequestMessage;
use pp_config::config::ModelChoice;
use pp_config::model_state::ModelState;
use pp_config::models::Protocol;
use pp_core::model::ModelSource;
use pp_llm::OpenAiClient;
use pp_tools::Tools;
use std::path::PathBuf;

impl Agent {
    pub fn restore(
        models: std::sync::Arc<ModelState>,
        tools: Tools,
        restored: persistence::RestoredSession,
    ) -> Result<Self> {
        Self::restore_with_options(models, tools, restored, true)
    }

    pub fn restore_with_options(
        models: std::sync::Arc<ModelState>,
        mut tools: Tools,
        restored: persistence::RestoredSession,
        hooks_enabled: bool,
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
        // Role routes of an old checkpoint no longer participate in the restore; a stale route must be cleaned up once.
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
        // The override source uses the override level; otherwise keep the level saved in the session while still supported.
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
        let ephemeral = journal.is_ephemeral();
        // Hook config is not restored from the checkpoint: user-level and project-level hooks files are always re-read; `--no-hooks` skips them.
        let hooks = if hooks_enabled {
            Self::compile_hooks(&catalog, tools.cwd())?
        } else {
            std::sync::Arc::new(crate::hooks::Runner::empty())
        };
        tools.limits = checkpoint.tool_limits;
        tools.configure_eval(catalog.eval_settings());
        tools.configure_web_search(catalog.web_search_settings().clone());
        tools.configure_mcp(catalog.mcp_settings());
        // A restore does not promise the temporary pipe bodies of the old process: switch to an empty registry explicitly.
        tools.reset_pipe_store();
        if let Some(id) = journal.id() {
            let _ = tools.artifacts.bind(journal.directory(), id);
        }
        // Permission config follows the current settings.yml and is not restored from the old checkpoint;
        // session-level grants were never serialized in the first place.
        tools.permission.set_config(config.settings.tools.clone());
        // Reuse the process-wide singleton: the background connect at startup may still be running, so only the target access is registered here and the first
        // task entry waits and refreshes the tool table uniformly, keeping session switches non-blocking for the UI.
        if tools.allows_any_mcp() {
            tools.mcp.start_connect(tools.mcp_access());
        }
        if tools.allows_lsp_lifecycle() {
            Self::install_lsp(&mut tools, &catalog);
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
        checkpoint.session.apply_cache_policy(
            config.prompt_cache_key,
            config.prompt_cache_retention,
            || super::generate_prompt_cache_key(journal.id()),
        );
        let session_date = super::current_session_date();
        let restored_reported = reported_project_load(&replay);
        let delivered_project_docs = restored_reported
            .iter()
            .map(|(path, content)| (PathBuf::from(path), content.clone()))
            .collect();
        let thinking_history = super::context_diagnosis::extract_thinking_from_replay(&replay);
        let mut agent = Self {
            role_route: None,
            client,
            runtime_models: None,
            subagent_control: Default::default(),
            subagent: None,
            subagents_enabled: true,
            task_status: Default::default(),
            context_snapshot: None,
            tools,
            session: checkpoint.session,
            auto_compact: config.settings.auto_compact,
            auto_compact_threshold_percent: config.settings.auto_compact_threshold_percent,
            auto_prune: config.settings.auto_prune,
            auto_prune_threshold_percent: config.settings.auto_prune_threshold_percent,
            last_request: None,
            last_request_raw_tokens: None,
            session_output_tokens: 0,
            thinking_history,
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
            prompt_cache_key_enabled: config.prompt_cache_key,
            prompt_cache_retention: config.prompt_cache_retention,
            base_system: checkpoint.base_system,
            prompt_source: checkpoint.prompt_source,
            pending_instructions: None,
            pending_project_load: Vec::new(),
            reported_project_load: restored_reported,
            session_date,
            static_project_prompt: None,
            delivered_project_docs,
            request_skills: Vec::new(),
            journal,
            replay,
            debug: debug::DebugState::new(config.settings.debug && !ephemeral, true),
            workflow: checkpoint.workflow,
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
        if let Some(source) = &agent.prompt_source {
            let context = crate::system_prompt::RenderContext::new(
                agent.tools.cwd(),
                &agent.current_model.model,
                agent.tools.edit_mode(),
                agent.workflow.mode_name(),
                agent.session.tool_names(),
            )
            .with_sandbox(&agent.tools.sandbox_status());
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
        agent.restore_session_usage();
        agent.init_auto_review();
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
        session.apply_cache_policy(
            self.prompt_cache_key_enabled,
            self.prompt_cache_retention,
            || super::generate_prompt_cache_key(self.journal.id()),
        );
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
        self.tools.permission.clear_session();
        self.session = session;
        self.reset_lsp_diagnostics();
        self.tools.clear_caches();
        if let Err(error) = self.tools.stop_eval() {
            tracing::warn!(error = %error, "stopping Python eval kernel on tree navigation failed");
        }
        // Navigating to another session root: the old root's temporary pipe bodies cannot be reused across roots.
        self.tools.reset_pipe_store();
        self.clear_session_usage();
        self.thinking_history = super::context_diagnosis::extract_thinking_from_replay(&replay);
        if let Some(view) = &self.task_status.view {
            view.clear_title();
        }
        self.pending_instructions = None;
        self.pending_project_load.clear();
        self.reported_project_load = reported_project_load(&replay);
        self.static_project_prompt = None;
        self.delivered_project_docs = self
            .reported_project_load
            .iter()
            .map(|(path, content)| (PathBuf::from(path), content.clone()))
            .collect();
        self.tools
            .reset_project_instructions(self.reported_project_load.keys().map(PathBuf::from));
        if let Some(state) = &self.auto_review {
            state.clear_user_intent();
        }
        self.sync_auto_review();
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
        self.tools.clear_caches();
        if let Err(error) = self.tools.stop_eval() {
            tracing::warn!(error = %error, "stopping Python eval kernel on reset failed");
        }
        self.workflow = pp_core::plan::WorkflowState {
            mode: self.workflow.mode,
            ..Default::default()
        };
        self.tools.permission.clear_session();
        if let Some(state) = &self.auto_review {
            state.clear_user_intent();
        }
        self.journal.reset();
        self.sync_auto_review();
        self.tools.artifacts = Default::default();
        // /new is a new session root: old temporary pipe bodies and URIs all become invalid.
        self.tools.reset_pipe_store();
        if let Some(debug) = &mut self.debug {
            debug.set_prompt_recorded(false);
        }
        self.replay.clear();
        self.thinking_history.clear();
        self.clear_session_usage();
        self.clear_session_cost();
        self.pending_project_load.clear();
        self.reported_project_load.clear();
        self.static_project_prompt = None;
        self.delivered_project_docs.clear();
        self.session_date = super::current_session_date();
        self.tools.reset_project_instructions([]);
        self.session.clear_prompt_cache_key();
        self.session.apply_cache_policy(
            self.prompt_cache_key_enabled,
            self.prompt_cache_retention,
            || super::generate_prompt_cache_key(None),
        );
        match &mut self.session {
            Session::Completion(request) => request.messages.truncate(1),
            Session::Response(request) => response_input(request).clear(),
        }
    }

    pub fn delete_saved_session(&self, id: &str) -> Result<()> {
        let dir = self.journal.directory();
        let history_path = dir
            .parent()
            .context("missing agent data directory")?
            .join("history.db");
        persistence::delete(
            dir,
            self.journal.lock_dir(),
            self.journal.stats_handle(),
            &history_path,
            id,
        )
    }
}
