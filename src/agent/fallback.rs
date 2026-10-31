//! 角色来源及任务内备用选择；只在完整消息边界切换模型。
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

    pub(super) fn activate_fallback(&mut self, emit: &mut dyn FnMut(AgentEvent)) -> Result<bool> {
        let Some(route) = &self.role_route else {
            return Ok(false);
        };
        if self.current_model.reference != route.primary.model {
            return Ok(false);
        }
        let Some(target) = route.fallback.clone() else {
            return Ok(false);
        };
        self.switch_role_model(&target, true, emit)?;
        Ok(true)
    }

    pub(super) fn restore_role_primary(&mut self, emit: &mut dyn FnMut(AgentEvent)) -> Result<()> {
        if let Some(route) = &self.role_route {
            let target = route.primary.clone();
            if self.current_model.reference != target.model {
                self.switch_role_model(&target, false, emit)?;
            }
        }
        Ok(())
    }

    fn switch_role_model(
        &mut self,
        target: &crate::config::RoleModel,
        fallback: bool,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        self.switch_model_with_effort(&target.model, Some(target.effort))?;
        if let Some(binding) = &mut self.tools.subagent {
            binding.model = self.current_model.reference.clone();
            binding.thinking = self.think_level;
            if binding.depth > 0 {
                let binding = binding.clone();
                self.save_child(
                    &binding
                        .directory()
                        .join(format!("{}.session.json", binding.owner)),
                )?;
                binding.update_model(
                    &target.model,
                    target.effort,
                    self.catalog.cost(&target.model),
                )?;
            }
        }
        let event = AgentEvent::ModelRouteChanged {
            model: target.model.clone(),
            effort: target.effort,
            fallback,
            temporary: false,
        };
        self.journal.event(&event);
        self.journal.check()?;
        emit(event);
        self.publish_context(emit)
    }
}

/// 临时审阅／摘要只替换自己的客户端和请求，不修改主会话。
pub(super) fn switch_scratch_model(
    client: &mut OpenAiClient,
    session: &mut Session,
    catalog: &ModelCatalog,
    target: &crate::config::RoleModel,
    edit_mode: EditMode,
) -> Result<Option<u64>> {
    let config = catalog.resolve(&target.model)?;
    ensure!(
        config.thinking.supports(target.effort),
        "fallback thinking is not supported"
    );
    let portable = match session {
        Session::Completion(_) => config.protocol != Protocol::Completion,
        Session::Response(_) => {
            config.protocol != Protocol::Response
                || !client.same_source(&config)
                || client.model_reference().split_once('/').map(|r| r.0)
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
    client.copy_stats_context(&mut replacement);
    if let Some(converted) = converted {
        *session = converted;
    }
    match session {
        Session::Completion(request) => request.model = config.model,
        Session::Response(request) => request.model = Some(config.model),
    }
    session.set_think_level(target.effort);
    *client = replacement;
    Ok(config.context_window)
}
