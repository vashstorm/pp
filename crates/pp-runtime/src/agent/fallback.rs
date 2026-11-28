//! In-task fallback model and model reversion at task boundaries; models switch only on complete message boundaries.
use super::*;

impl Agent {
    pub(super) fn checkpoint_route(&mut self) -> Result<()> {
        let metadata = persistence::Metadata {
            role_route: self.role_route.as_ref(),
            cwd: self.tools.cwd(),
            model_reference: &self.current_model.reference,
            think_level: self.think_level,
            edit_mode: self.tools.edit_mode(),
            tool_limits: self.tools.limits,
            base_system: &self.base_system,
            prompt_source: self.prompt_source.as_ref(),
        };
        self.journal
            .checkpoint(&metadata, (&self.session).into(), &self.workflow)
    }

    /// Task start: return to `resolve()` (`a` → `b`) only if the previous task stopped on the fallback.
    pub(super) fn restore_resolved(&mut self, emit: &mut dyn FnMut(AgentEvent)) -> Result<()> {
        if !self.model_source.is_fallback() {
            return Ok(());
        }
        let selection = self.models.resolve()?;
        let source = self.models.source();
        self.switch_role_model(&selection.reference, selection.effort, source, emit)
    }

    /// Active role: runtime binding (TUI main session, subagent) → role route matching the current model
    /// (PLAN mode, role-routed subagents) → DEFAULT.
    /// `-p`/line mode has no runtime binding and must fall back to `role_route`, otherwise a role fallback model would degrade to DEFAULT.
    fn active_role(&self) -> ModelRole {
        self.runtime_models
            .as_ref()
            .and_then(|binding| binding.role)
            .or_else(|| {
                self.role_route
                    .as_ref()
                    .filter(|route| route.primary.model == self.current_model.reference)
                    .map(|route| route.role)
            })
            .unwrap_or(ModelRole::Default)
    }

    /// The enabled fallback model of the active role, returned together with that role's main model; the
    /// latter lets the summarization path decide to "switch to the fallback only on the role's main model". Owned values are
    /// returned so callers do not borrow `self` for long.
    pub(super) fn role_fallback(&self) -> Option<(String, RoleModel)> {
        let assignment = self
            .catalog
            .model_roles()
            .get(&self.active_role())
            .filter(|assignment| assignment.enabled)?;
        Some((assignment.model.clone(), assignment.fallback.clone()?))
    }

    /// In-task fallback model after a request failure: switches once, and [`Self::restore_resolved`] reverts at task end.
    pub(super) fn activate_fallback(&mut self, emit: &mut dyn FnMut(AgentEvent)) -> Result<bool> {
        if self.model_source.is_fallback() {
            return Ok(false);
        }
        // A subagent uses its own role's fallback model; the main Agent uses the active role's (usually DEFAULT) fallback model.
        let Some((_, fallback)) = self.role_fallback() else {
            return Ok(false);
        };
        if fallback.model == self.current_model.reference {
            return Ok(false);
        }
        let config = self.catalog.resolve(&fallback.model)?;
        ensure!(
            config.thinking.supports(fallback.effort),
            "fallback thinking is not supported by the fallback model"
        );
        self.switch_role_model(
            &fallback.model,
            fallback.effort,
            ModelSource::Fallback,
            emit,
        )?;
        Ok(true)
    }

    fn switch_role_model(
        &mut self,
        reference: &str,
        effort: ThinkLevel,
        source: ModelSource,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        self.switch_model_with_effort(reference, Some(effort))?;
        self.model_source = source;
        self.role_route = None;
        if let Some(binding) = &mut self.subagent {
            binding.model = self.current_model.reference.clone();
            binding.thinking = self.think_level;
            if binding.depth > 0 {
                let binding = binding.clone();
                self.save_child(
                    &binding
                        .directory()
                        .join(format!("{}.session.json", binding.owner)),
                )?;
                binding.update_model(reference, effort, self.catalog.cost(reference))?;
            }
        }
        let event = AgentEvent::ModelRouteChanged {
            model: reference.to_owned(),
            effort,
            source,
            temporary: false,
        };
        self.journal.event(&event);
        self.journal.check()?;
        emit(event);
        self.publish_context(emit)
    }
}

/// Scratch review/summarization replaces only its own client and request, never the main session.
pub(super) fn switch_scratch_model(
    client: &mut OpenAiClient,
    session: &mut Session,
    catalog: &ModelCatalog,
    target: &pp_config::config::RoleModel,
    edit_mode: EditMode,
) -> Result<Option<u64>> {
    let (replacement, window) = create_scratch_model(client, session, catalog, target, edit_mode)?;
    *client = replacement;
    Ok(window)
}

/// Build a new client pointing at the target model and convert the scratch session history as needed; the source model need not still be in the catalog.
pub(super) fn create_scratch_model(
    source: &OpenAiClient,
    session: &mut Session,
    catalog: &ModelCatalog,
    target: &pp_config::config::RoleModel,
    edit_mode: EditMode,
) -> Result<(OpenAiClient, Option<u64>)> {
    let config = catalog.resolve(&target.model)?;
    ensure!(
        config.thinking.supports(target.effort),
        "fallback thinking is not supported"
    );
    let portable = match session {
        Session::Completion(_) => config.protocol != Protocol::Completion,
        Session::Response(_) => {
            config.protocol != Protocol::Response
                || !source.same_source(&config)
                || source.model_reference().split_once('/').map(|r| r.0)
                    != target.model.split_once('/').map(|r| r.0)
        }
    };
    let converted = if portable {
        Some(history::convert(
            session,
            config.model.clone(),
            config.protocol,
            edit_mode,
        )?)
    } else {
        None
    };
    let mut replacement = OpenAiClient::new(&config)?;
    source.copy_stats_context(&mut replacement);
    if let Some(converted) = converted {
        *session = converted;
    }
    match session {
        Session::Completion(request) => request.model = config.model,
        Session::Response(request) => request.model = Some(config.model),
    }
    session.set_think_level(target.effort);
    Ok((replacement, config.context_window))
}
