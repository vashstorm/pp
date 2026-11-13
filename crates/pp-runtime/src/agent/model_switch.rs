//! 运行时模型／档位切换与失败回报；整块搬迁，切换语义不可再拆。
use super::{Agent, Session, history, persistence::Metadata, plan_workflow};
use anyhow::{Result, ensure};
use pp_config::config::ModelChoice;
use pp_config::model_state::Selection;
use pp_config::models::Protocol;
use pp_core::model::ModelSource;
use pp_core::model::ThinkLevel;
use pp_llm::OpenAiClient;

impl Agent {
    pub fn apply_runtime_models(&mut self) -> Result<()> {
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
        let Some((model, think)) = self
            .runtime_models
            .as_mut()
            .map(|binding| binding.take_targets())
        else {
            return Ok(());
        };
        // 两次意图是独立提交，按 revision 顺序应用（旧者先、新者后），高 revision 的 effort 胜出。
        match (model, think) {
            (Some(model), Some(think)) if think.revision < model.revision => {
                self.apply_think_target(&think);
                self.apply_model_target(model)
            }
            (Some(model), think) => {
                self.apply_model_target(model)?;
                if let Some(think) = &think {
                    self.apply_think_target(think);
                }
                Ok(())
            }
            (None, Some(think)) => {
                self.apply_think_target(&think);
                Ok(())
            }
            (None, None) => Ok(()),
        }
    }

    fn apply_think_target(&mut self, target: &crate::model_control::ThinkTarget) {
        // 失败只回报 ThinkFailed 并保留旧档位：不向外返回错误、不重试。
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
        let old_catalog = self.catalog.clone();
        let old_source = self.model_source;
        self.catalog = self.models.catalog_snapshot();
        if let Err(error) = self.switch_model_with_effort_result(&selection, target.source) {
            self.catalog = old_catalog;
            self.model_source = old_source;
            self.report_model_failure(&target, format!("{error:#}"));
            return Ok(());
        }
        self.persist_active_model(&target)?;
        let binding = self
            .runtime_models
            .as_ref()
            .expect("模型绑定在应用期间保留");
        binding.report(crate::model_control::Report::Applied {
            agent: binding.agent.clone(),
            revision: target.revision,
            model: self.current_model.clone(),
            source: target.source,
        });
        Ok(())
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

    /// 把当前模型同步给子 Agent 监督绑定；失败上报并终止本次切换。
    fn persist_active_model(&mut self, target: &crate::model_control::Target) -> Result<()> {
        let Some(supervisor) = &mut self.tools.subagent else {
            return Ok(());
        };
        supervisor.model = self.current_model.reference.clone();
        supervisor.thinking = self.think_level;
        if supervisor.depth == 0 {
            return Ok(());
        }
        let supervisor = supervisor.clone();
        let snapshot = supervisor
            .directory()
            .join(format!("{}.session.json", supervisor.owner));
        if let Err(error) = self.save_child(&snapshot).and_then(|()| {
            supervisor.update_model(
                &self.current_model.reference,
                self.think_level,
                self.catalog.cost(&self.current_model.reference),
            )
        }) {
            self.report_model_failure(target, format!("Could not persist active model: {error:#}"));
            return Err(error.context("persisting subagent model change"));
        }
        Ok(())
    }

    /// 应用一次显式选择：写 `a` 后按源切换；切换失败时把 `a` 与运行时字段一起回滚。
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
        if result.is_err() {
            self.model_source = previous_source;
            self.role_route = previous_route;
            self.catalog = previous_catalog;
            let rollback = match previous_override {
                Some(selection) => self.models.set_override(selection).map(|_| ()),
                None => self.models.clear_override().map(|_| ()),
            };
            rollback?;
        }
        result
    }

    pub fn switch_model(&mut self, reference: &str) -> Result<()> {
        let selection = self
            .models
            .resolve_reference(reference, Some(self.think_level))?;
        self.apply_override_selection(selection)
    }

    /// CLI `--model`：模糊选择后写入手动覆盖 `a`，返回最终选择供 CLI 显示。
    ///
    /// 选择或应用失败时不改变当前模型与档位；覆盖由进程级 `ModelState` 持有，
    /// 重建 Agent、进出 Plan 与恢复会话都不会丢失。
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
        let mut client = client;
        client.set_task_accounting(self.task_status.accounting.clone());
        self.client = client;
        self.current_model = choice;
        self.think_level = target_level;
        // 切模型只丢弃最近请求指标，会话累计输出跨模型保留。
        self.last_request = None;
        self.pending_instructions = None;
        Ok(true)
    }
}
