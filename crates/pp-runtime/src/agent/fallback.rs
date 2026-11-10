//! 任务内备用模型与任务边界的模型回落；只在完整消息边界切换模型。
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

    /// 任务开始：只有上一个任务停在 fallback 时才回到 `resolve()`（`a` → `b`）。
    pub(super) fn restore_resolved(&mut self, emit: &mut dyn FnMut(AgentEvent)) -> Result<()> {
        if !self.model_source.is_fallback() {
            return Ok(());
        }
        let selection = self.models.resolve()?;
        let source = self.models.source();
        self.switch_role_model(&selection.reference, selection.effort, source, emit)
    }

    /// 请求失败后的任务内备用模型：只切一次，任务结束时由 [`Self::restore_resolved`] 回落。
    pub(super) fn activate_fallback(&mut self, emit: &mut dyn FnMut(AgentEvent)) -> Result<bool> {
        if self.model_source.is_fallback() {
            return Ok(false);
        }
        // 子 Agent 用自己的角色备用模型；主 Agent 用 DEFAULT 角色的备用模型。
        let role = self
            .runtime_models
            .as_ref()
            .and_then(|binding| binding.role)
            .unwrap_or(ModelRole::Default);
        let Some(fallback) = self
            .catalog
            .model_roles()
            .get(&role)
            .filter(|assignment| assignment.enabled)
            .and_then(|assignment| assignment.fallback.clone())
        else {
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

/// 临时审阅／摘要只替换自己的客户端和请求，不修改主会话。
pub(super) fn switch_scratch_model(
    client: &mut OpenAiClient,
    session: &mut Session,
    catalog: &ModelCatalog,
    target: &pp_config::config::RoleModel,
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
