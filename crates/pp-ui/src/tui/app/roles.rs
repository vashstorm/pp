use crate::tui::{App, Level, State, ThinkLevel, WorkerCommand, roles};
use pp_core::model::ModelSource;

impl App {
    pub(in crate::tui) fn open_roles(&mut self) {
        if !self.model_selection_ready() {
            return;
        }
        self.roles_panel = Some(roles::Panel::new(
            self.choices.clone(),
            self.favorite_models.clone(),
            self.model_roles.clone(),
            self.current_model
                .as_ref()
                .map(|model| model.reference.clone())
                .unwrap_or_default(),
            self.think_level,
            self.override_selection.clone(),
        ));
        self.menu = None;
        self.menu_dismissed = true;

        if self.bootstrap.is_some() && matches!(self.state, State::Ready) {
            self.reload_model_catalog();
        }
    }

    pub(in crate::tui) fn apply_roles_action(&mut self, action: roles::Action) {
        match action {
            roles::Action::None => {}
            roles::Action::Close => {
                self.roles_panel = None;
                self.menu_dismissed = true;
            }
            roles::Action::Favorite(reference) => {
                if !matches!(self.state, State::Ready) {
                    self.notify(
                        Level::Error,
                        "Favorite models can be changed when the agent is idle".into(),
                    );
                    return;
                }
                if self.send(
                    WorkerCommand::ToggleFavoriteModel(reference),
                    State::SavingModelSettings,
                ) {
                    if let Some(panel) = &mut self.roles_panel {
                        panel.busy = true;
                        panel.runtime_status(String::new());
                    }
                } else {
                    self.notify(Level::Error, "Model preferences service unavailable".into());
                }
            }
            roles::Action::Save(role, assignment) => {
                if let Some(control) = &self.model_control {
                    if control.submit(pp_runtime::model_control::Change::Role(role, assignment)) {
                        if let Some(panel) = &mut self.roles_panel {
                            panel.busy = true;
                        }
                    } else {
                        self.notify(Level::Error, "Model control unavailable".into());
                    }
                    return;
                }
                if self.send(
                    WorkerCommand::SaveModelRole(role, assignment),
                    State::SavingModelSettings,
                ) {
                    if let Some(panel) = &mut self.roles_panel {
                        panel.busy = true;
                    }
                } else {
                    self.notify(Level::Error, "Role management service unavailable".into());
                }
            }
            roles::Action::ClearOverride => {
                // 只回退到配置默认；`a` 仅存在于进程内，不需要 worker 持久化。
                if let Some(control) = &self.model_control {
                    if !control.submit(pp_runtime::model_control::Change::ClearOverride) {
                        self.notify(Level::Error, "Model control unavailable".into());
                    }
                } else {
                    self.notify(
                        Level::Error,
                        "Model control unavailable; cannot clear the manual override".into(),
                    );
                }
            }
        }
    }
}

impl App {
    pub(in crate::tui) fn model_selection_ready(&mut self) -> bool {
        matches!(
            self.state,
            State::Running | State::Cancelling | State::ReviewingPlan
        ) || self.shortcut_ready()
    }
    pub(in crate::tui) fn request_model_switch(&mut self, reference: String) -> bool {
        if let Some(control) = &self.model_control {
            // 手动选择记录为覆盖；目标档位沿用在 pp-config 里的统一策略
            // （模型显式默认 → 保留当前档 → 该模型有效默认）。
            let selection = match control.resolve_reference(&reference, Some(self.think_level)) {
                Ok(selection) => selection,
                Err(error) => {
                    self.notify(Level::Error, format!("{error:#}"));
                    return false;
                }
            };
            let accepted = control.submit(pp_runtime::model_control::Change::Override(selection));
            if !accepted {
                self.notify(Level::Error, "Model control unavailable".into());
            }
            accepted
        } else {
            self.send(WorkerCommand::SwitchModel(reference), State::SwitchingModel)
        }
    }

    /// 保存全局默认模型（写配置默认 `b`）：始终走 worker，保留既有的保存状态、失败提示与
    /// `ModelsReloaded` 快照链路；`ModelState` 内部用写锁串行化，两条写路径不会互相覆盖。
    pub(in crate::tui) fn request_save_default(&mut self, reference: String) -> bool {
        self.send(
            WorkerCommand::SaveDefaultModel(reference),
            State::SavingModelSettings,
        )
    }
    /// 切档不是 worker 命令：运行中与空闲都可提交，只要求没有本地面板/技能等待。
    pub(in crate::tui) fn think_switch_ready(&mut self) -> bool {
        matches!(self.state, State::Running | State::Cancelling) || self.shortcut_ready()
    }

    /// 校验档位时使用的模型：优先已提交但尚未应用的目标，回落当前模型。
    fn effective_thinking(&self) -> Option<pp_config::models::ModelThinking> {
        self.pending_model
            .as_ref()
            .and_then(|(_, reference)| {
                self.choices
                    .iter()
                    .find(|choice| choice.reference == *reference)
            })
            .or(self.current_model.as_ref())
            .map(|choice| choice.thinking)
    }

    /// 提交主会话手动档位：只登记期望值，不发送 worker 命令、不改写 `State`。
    pub(in crate::tui) fn submit_think_level(&mut self, level: ThinkLevel) -> bool {
        if matches!(self.state, State::Unavailable) {
            if let Some(error) = self.unavailable.clone() {
                self.notify(Level::Error, error);
            }
            return false;
        }
        let Some(thinking) = self.effective_thinking() else {
            return false;
        };
        if !thinking.supports(level) {
            let options = thinking
                .levels()
                .map(ThinkLevel::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            self.notify(
                Level::Error,
                format!("thinking level is not supported by the current model; options: {options}"),
            );
            return false;
        }
        if let Some(control) = &self.model_control
            && control.submit(pp_runtime::model_control::Change::Think(level))
        {
            self.pending_think = Some((0, level));
            return true;
        }
        self.notify(Level::Error, "Model control unavailable".into());
        false
    }

    pub(in crate::tui) fn poll_model_control(&mut self) -> bool {
        use pp_runtime::model_control::Report;
        let reports = self
            .model_control
            .as_ref()
            .map(|c| c.registry.reports())
            .unwrap_or_default();
        let changed = !reports.is_empty();
        for report in reports {
            match report {
                Report::Cleared { role, revision: _ } => {
                    if role == pp_config::config::ModelRole::Default {
                        // 清除覆盖：待应用目标、来源与 `/Roles` 只读行一起回到默认值。
                        self.pending_model = None;
                        self.current_source = None;
                        self.override_selection = None;
                        if let Some(panel) = &mut self.roles_panel {
                            panel.update_override(None);
                        }
                    }
                    self.secondary_status = None;
                }
                Report::Queued {
                    role,
                    revision,
                    selection,
                    source,
                } => {
                    if role == pp_config::config::ModelRole::Default {
                        self.pending_model = Some((revision, selection.reference.clone()));
                        // 覆盖已写入 `ModelState`；只登记快照，来源等实际应用后才切换。
                        self.override_selection =
                            (source == ModelSource::Override).then(|| selection.clone());
                        if let Some(panel) = &mut self.roles_panel {
                            panel.update_override(self.override_selection.clone());
                        }
                    }
                    if let Some(panel) = &mut self.roles_panel {
                        panel.runtime_status(format!(
                            "{} → {} · pending safe boundary",
                            role.name(),
                            selection.reference
                        ));
                    }
                    self.secondary_status = Some(format!(
                        "{} → {} · pending safe boundary",
                        role.name(),
                        selection.reference
                    ));
                    if let Some(commands) = &self.commands {
                        let _ = commands.send(WorkerCommand::ApplyModelTargets);
                    }
                }
                Report::Saved {
                    roles,
                    default,
                    effort,
                } => {
                    let roles_changed = roles != self.model_roles;
                    self.model_roles = roles;
                    self.default_model_reference = Some(default.clone());
                    self.default_think_level = effort;
                    // 保存默认模型时 `ModelState` 会在 `a == b` 时丢弃覆盖；
                    // UI 同步这一步，避免 `/Roles` 显示已不存在的 Override。
                    let cleared_override = self.override_selection.as_ref().is_some_and(|s| {
                        s.reference == default
                            && self
                                .choices
                                .iter()
                                .find(|choice| choice.reference == default)
                                .is_some_and(|choice| {
                                    choice.thinking.effective_default(effort) == s.effort
                                })
                    });
                    if cleared_override {
                        self.override_selection = None;
                    }
                    if let Some(panel) = &mut self.roles_panel {
                        panel.saved();
                        panel.update_override(self.override_selection.clone());
                    }
                    if roles_changed {
                        self.notify(
                            Level::Normal,
                            "Role configuration saved; applying at safe boundaries".into(),
                        );
                    } else {
                        self.notify(
                            Level::Normal,
                            format!("Default model saved: {default} (takes effect next launch)"),
                        );
                    }
                }
                Report::Applied {
                    agent,
                    revision,
                    model,
                    source,
                } => {
                    crate::tui::app::receive::ui_dbg(&format!(
                        "Applied rev={revision} model={} source={:?} agent={:?} current_rev={}",
                        model.reference, source, agent, self.model_revision
                    ));
                    if let Some(panel) = &mut self.roles_panel {
                        panel.runtime_status(format!(
                            "{}current: {}",
                            agent
                                .as_ref()
                                .map(|id| format!("{id} · "))
                                .unwrap_or_default(),
                            model.reference
                        ));
                    }
                    if let Some(agent) = agent {
                        self.notify(
                            Level::Normal,
                            format!("{agent} switched to {}", model.reference),
                        );
                    } else if revision >= self.model_revision {
                        self.model_revision = revision;
                        self.current_model = Some(model);
                        self.current_source = Some(source);
                        if source == ModelSource::Default {
                            self.override_selection = None;
                            if let Some(panel) = &mut self.roles_panel {
                                panel.update_override(None);
                            }
                        }
                        self.usage = None;
                        if self
                            .pending_model
                            .as_ref()
                            .is_some_and(|(pending, _)| *pending <= revision)
                        {
                            self.pending_model = None;
                            self.secondary_status = None;
                        }
                    }
                }
                Report::Failed {
                    agent,
                    revision,
                    message,
                } => {
                    if agent.is_none()
                        && self
                            .pending_model
                            .as_ref()
                            .is_some_and(|(pending, _)| *pending == revision)
                    {
                        self.pending_model = None;
                        self.secondary_status = None;
                    }
                    if let Some(panel) = &mut self.roles_panel {
                        panel.busy = false;
                        panel.runtime_status(format!("Model change failed: {message}"));
                    }
                    self.notify(
                        Level::Error,
                        format!(
                            "{}model change failed: {message}",
                            agent.map(|id| format!("{id}: ")).unwrap_or_default()
                        ),
                    );
                }
                Report::ThinkQueued { revision, effort } => {
                    // 连按时只保留最新期望值；级别不等说明是已被取代的旧目标。
                    if self
                        .pending_think
                        .as_ref()
                        .is_some_and(|(_, level)| *level == effort)
                    {
                        self.pending_think = Some((revision, effort));
                    }
                    self.secondary_status = Some(format!(
                        "thinking → {} · pending next request",
                        effort.label()
                    ));
                    // 空闲时 worker 的 50ms 轮询随即应用；忙碌时排在任务之后执行。
                    if let Some(commands) = &self.commands {
                        let _ = commands.send(WorkerCommand::ApplyModelTargets);
                    }
                }
                Report::ThinkApplied { revision, effort } => {
                    if revision >= self.model_revision {
                        self.model_revision = revision;
                        self.think_level = effort;
                        if let Some(selection) = &mut self.override_selection {
                            selection.effort = effort;
                        }
                        if let Some(panel) = &mut self.roles_panel {
                            panel.update_override(self.override_selection.clone());
                        }
                        // 命中率不可跨边界比较，旧用量快照同样失效。
                        self.usage = None;
                        self.token_speed.clear();
                    }
                    if self
                        .pending_think
                        .as_ref()
                        .is_some_and(|(pending, level)| revision >= *pending && *level == effort)
                    {
                        self.pending_think = None;
                        self.secondary_status = None;
                    }
                }
                Report::ThinkFailed {
                    revision,
                    effort,
                    message,
                } => {
                    // 迟到的失败不能清掉更新的期望档位。
                    if self
                        .pending_think
                        .as_ref()
                        .is_some_and(|(pending, level)| revision >= *pending && *level == effort)
                    {
                        self.pending_think = None;
                        self.secondary_status = None;
                    }
                    self.notify(
                        Level::Error,
                        format!("Thinking effort change failed: {message}"),
                    );
                }
            }
        }
        changed
    }
}
