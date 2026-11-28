//! Runtime model/effort switching and failure reporting; moved as a whole, the switching semantics can no longer be split.
use super::{Agent, Session, history, persistence::Metadata, plan_workflow};
use anyhow::{Result, ensure};
use pp_config::config::{ModelChoice, RoleRoute};
use pp_config::model_state::Selection;
use pp_config::models::Protocol;
use pp_core::model::ModelSource;
use pp_core::model::ThinkLevel;
use pp_core::plan::WorkMode;
use pp_llm::OpenAiClient;

impl Agent {
    pub(super) fn runtime_model_pending(&self) -> bool {
        self.runtime_models
            .as_ref()
            .is_some_and(|binding| binding.pending(self.follows_plan_role()))
    }

    fn follows_plan_role(&self) -> bool {
        self.workflow.mode == WorkMode::Plan && self.models.override_selection().is_none()
    }

    pub fn apply_runtime_models(&mut self) -> Result<()> {
        let _ = self.apply_runtime_models_changed()?;
        Ok(())
    }

    pub(in crate::agent) fn apply_runtime_models_changed(&mut self) -> Result<bool> {
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
        let follow_plan = self.follows_plan_role();
        let Some((model, think)) = self
            .runtime_models
            .as_mut()
            .map(|binding| binding.take_targets(follow_plan))
        else {
            return Ok(false);
        };
        // The two intents are independent commits applied in revision order (older first, newer last); the higher revision's effort wins.
        match (model, think) {
            (Some(model), Some(think)) if think.revision < model.revision => {
                self.apply_think_target(&think);
                self.apply_model_target(model)?;
                Ok(true)
            }
            (Some(model), think) => {
                self.apply_model_target(model)?;
                if let Some(think) = &think {
                    self.apply_think_target(think);
                }
                Ok(true)
            }
            (None, Some(think)) => {
                self.apply_think_target(&think);
                Ok(true)
            }
            (None, None) => Ok(false),
        }
    }

    fn apply_think_target(&mut self, target: &crate::model_control::ThinkTarget) {
        // On failure only report ThinkFailed and keep the old level: no error propagates out, no retry.
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
        let selection = match &target.selection {
            Some(selection) => selection.clone(),
            None => match self.models.resolve() {
                Ok(selection) => selection,
                Err(error) => {
                    self.report_model_failure(&target, format!("{error:#}"));
                    return Ok(());
                }
            },
        };
        let old_catalog = std::mem::replace(&mut self.catalog, self.models.catalog_snapshot());
        let old_source = self.model_source;
        let result = match &target.plan_route {
            Some(route) => self.switch_plan_model(&selection, Some(route.clone()), target.source),
            None => self.switch_model_with_effort_result(&selection, target.source),
        };
        if let Err(error) = result {
            self.catalog = old_catalog;
            self.model_source = old_source;
            self.report_model_failure(&target, format!("{error:#}"));
            return Ok(());
        }
        self.persist_active_model(&target)?;
        let binding = self
            .runtime_models
            .as_ref()
            .expect("model binding is retained during application");
        binding.report(crate::model_control::Report::Applied {
            agent: binding.agent.clone(),
            revision: target.revision,
            model: self.current_model.clone(),
            effort: self.think_level,
            source: target.source,
        });
        Ok(())
    }

    pub(super) fn switch_plan_model(
        &mut self,
        selection: &Selection,
        route: Option<RoleRoute>,
        source: ModelSource,
    ) -> Result<bool> {
        let previous_route = std::mem::replace(&mut self.role_route, route);
        let result = self
            .switch_model_with_effort(&selection.reference, Some(selection.effort))
            .and_then(|changed| {
                if !changed && previous_route != self.role_route {
                    self.checkpoint_route()?;
                }
                Ok(changed)
            });
        if result.is_ok() {
            self.model_source = source;
        } else {
            self.role_route = previous_route;
        }
        result
    }

    fn switch_model_with_effort_result(
        &mut self,
        selection: &Selection,
        source: ModelSource,
    ) -> Result<bool> {
        let changed =
            self.switch_model_with_effort(&selection.reference, Some(selection.effort))?;
        self.model_source = source;
        Ok(changed)
    }

    fn report_model_failure(&self, target: &crate::model_control::Target, message: String) {
        if let Some(binding) = self.runtime_models.as_ref() {
            binding.report(crate::model_control::Report::Failed {
                agent: binding.agent.clone(),
                revision: target.revision,
                message,
            });
        }
    }

    /// Sync the current model to the subagent supervising binding; a failure is reported and aborts this switch.
    fn persist_active_model(&mut self, target: &crate::model_control::Target) -> Result<()> {
        let Some(supervisor) = &self.subagent else {
            return Ok(());
        };
        let mut updated = supervisor.clone();
        updated.model = self.current_model.reference.clone();
        updated.thinking = self.think_level;
        if updated.depth == 0 {
            self.subagent = Some(updated);
            return Ok(());
        }
        let snapshot = updated
            .directory()
            .join(format!("{}.session.json", updated.owner));
        if let Err(error) = self.save_child(&snapshot).and_then(|()| {
            updated.update_model(
                &self.current_model.reference,
                self.think_level,
                self.catalog.cost(&self.current_model.reference),
            )
        }) {
            self.report_model_failure(target, format!("Could not persist active model: {error:#}"));
            return Err(error.context("persisting subagent model change"));
        }
        self.subagent = Some(updated);
        Ok(())
    }

    /// Apply one explicit selection: write `a` and then switch by source; on a failed switch roll back `a` together with the runtime fields.
    fn apply_override_selection(&mut self, selection: Selection) -> Result<()> {
        let previous_source = self.model_source;
        let previous_route = self.role_route.take();
        let previous_catalog = self.catalog.clone();
        let previous_override = self.models.override_selection();
        self.models.set_override(selection.clone())?;
        let source = self.models.source();
        self.catalog = self.models.catalog_snapshot();
        let result = self
            .switch_model_with_effort(&selection.reference, Some(selection.effort))
            .map(|_| ())
            .map(|()| {
                self.model_source = source;
            });
        if let Err(error) = result {
            self.model_source = previous_source;
            self.role_route = previous_route;
            self.catalog = previous_catalog;
            let rollback = match previous_override {
                Some(selection) => self.models.set_override(selection).map(|_| ()),
                None => self.models.clear_override().map(|_| ()),
            };
            if let Err(rollback_error) = rollback {
                return Err(error.context(format!(
                    "rolling back model override failed: {rollback_error:#}"
                )));
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn switch_model(&mut self, reference: &str) -> Result<()> {
        let selection = self
            .models
            .resolve_reference(reference, Some(self.think_level))?;
        self.apply_override_selection(selection)
    }

    /// CLI `--model`: fuzzy-select and write the manual override `a`, returning the final selection for the CLI to display.
    ///
    /// A failed selection or application leaves the current model and level untouched; the override is held by the
    /// process-wide `ModelState`, so it survives Agent rebuilds, entering/leaving Plan and session restores.
    pub fn select_model(&mut self, query: &str) -> Result<pp_config::config::ModelSelection> {
        let selection = self.catalog.select(query)?;
        let chosen = Selection {
            reference: selection.reference.clone(),
            effort: selection.effort,
        };
        self.apply_override_selection(chosen)?;
        Ok(selection)
    }

    pub(in crate::agent) fn switch_model_with_effort(
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
            && config.prompt_cache_key == self.prompt_cache_key_enabled
            && config.prompt_cache_retention == self.prompt_cache_retention
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
            session.apply_cache_policy(
                config.prompt_cache_key,
                config.prompt_cache_retention,
                || super::generate_prompt_cache_key(self.journal.id()),
            );
            self.journal
                .checkpoint(&metadata, (&session).into(), &self.workflow)?;
            self.session = session;
        } else {
            let old_system = plan_workflow::SavedInstructions::take(&mut self.session);
            let old_cache_key = self.session.prompt_cache_key().map(str::to_owned);
            let old_cache_retention = self.session.prompt_cache_retention();
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
            self.session.apply_cache_policy(
                config.prompt_cache_key,
                config.prompt_cache_retention,
                || super::generate_prompt_cache_key(self.journal.id()),
            );
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
                match old_cache_key {
                    Some(key) => self.session.set_prompt_cache_key(key),
                    None => self.session.clear_prompt_cache_key(),
                }
                self.session
                    .set_prompt_cache_retention_raw(old_cache_retention);
                old_system.restore(&mut self.session);
                return Err(error);
            }
        }
        let mut client = client;
        client.set_task_accounting(self.task_status.accounting.clone());
        self.client = client;
        self.current_model = choice;
        self.think_level = target_level;
        self.prompt_cache_key_enabled = config.prompt_cache_key;
        self.prompt_cache_retention = config.prompt_cache_retention;
        // A model switch drops only the last-request metrics; cumulative session output survives model switches.
        self.last_request = None;
        self.last_request_raw_tokens = None;
        self.pending_instructions = None;
        self.static_project_prompt = None;
        self.delivered_project_docs.clear();
        self.invalidate_instruction_refresh();
        Ok(true)
    }
}
