use crate::tui::{App, Level, State, ThinkLevel, WorkerCommand, roles};
use pp_core::model::ModelSource;

impl App {
    pub(in crate::tui) fn open_roles(&mut self) {
        let allowed = if self.active_settings_tab().is_some() {
            !self.skills_pending
                && self.plan_list_panel.is_none()
                && matches!(
                    self.state,
                    State::Ready
                        | State::Running
                        | State::Cancelling
                        | State::ReviewingPlan
                        | State::Unavailable
                )
        } else {
            self.model_selection_ready()
        };
        if !allowed {
            return;
        }
        self.clear_other_settings_tabs(Some(super::super::settings::Tab::Roles));
        let mut panel = roles::Panel::new(
            self.choices.clone(),
            self.favorite_models.clone(),
            self.model_roles.clone(),
            self.current_model
                .as_ref()
                .map(|model| model.reference.clone())
                .unwrap_or_default(),
            self.think_level,
            self.override_selection.clone(),
        );
        if matches!(self.state, State::Unavailable)
            && let Some(error) = &self.unavailable
        {
            panel.runtime_status(error.clone());
        }
        self.roles_panel = Some(panel);
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
                // Falls back to the configured default only; `a` exists in-process and needs
                // no worker persistence.
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
            // A manual choice is recorded as an override; the target effort follows the
            // shared policy in pp-config (explicit model default -> keep current level ->
            // that model's effective default).
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

    /// Saves the global default model (writes config default `b`): always goes through the
    /// worker, keeping the existing save state, failure notice and `ModelsReloaded` snapshot
    /// chain; `ModelState` serializes with a write lock, so the two write paths never
    /// overwrite each other.
    pub(in crate::tui) fn request_save_default(&mut self, reference: String) -> bool {
        self.send(
            WorkerCommand::SaveDefaultModel(reference),
            State::SavingModelSettings,
        )
    }
    /// Effort switching is not a worker command: it can be submitted while running or idle
    /// and only requires that no local panel or skill is waiting.
    pub(in crate::tui) fn think_switch_ready(&mut self) -> bool {
        matches!(self.state, State::Running | State::Cancelling) || self.shortcut_ready()
    }

    /// The model used to validate an effort level: prefer the submitted but not yet applied
    /// target, falling back to the current model.
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

    /// Submits a manual effort level for the main session: registers the desired value only,
    /// sends no worker command and does not rewrite `State`.
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
                        // Override cleared: the pending target, source and the `/roles`
                        // read-only row all return to their defaults.
                        self.pending_model = None;
                        self.current_source = None;
                        self.override_selection = None;
                        if let Some(panel) = &mut self.roles_panel {
                            panel.update_override(None);
                        }
                    }
                    self.secondary_status = None;
                    // After clearing, `a` is empty and the session must return to `resolve()`;
                    // the registry already registered a main target with `selection: None`, so
                    // this only has to wake the worker to apply it.
                    if let Some(commands) = &self.commands {
                        let _ = commands.send(WorkerCommand::ApplyModelTargets);
                    }
                }
                Report::Queued {
                    role,
                    revision,
                    selection,
                    source,
                } => {
                    if let Some(commands) = &self.commands {
                        let _ = commands.send(WorkerCommand::ApplyModelTargets);
                    }
                    // Whether PLAN takes effect depends on the worker's current mode and the
                    // manual override.
                    if role == pp_config::config::ModelRole::Plan {
                        continue;
                    }
                    if role == pp_config::config::ModelRole::Default {
                        self.pending_model = Some((revision, selection.reference.clone()));
                        // The override is already written into `ModelState`; only the snapshot is
                        // registered, and the source switches once it is actually applied.
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
                    // Saving the default model makes `ModelState` drop the override when
                    // `a == b`; the UI mirrors that step so `/roles` never shows an Override
                    // that no longer exists.
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
                    effort,
                    source,
                } => {
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
                        self.think_level = effort;
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
                    // Repeated presses keep only the newest desired value; a different level
                    // means this is a superseded older target.
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
                    // When idle the worker's 50ms poll applies it right away; when busy it runs
                    // after the current task.
                    if let Some(commands) = &self.commands {
                        let _ = commands.send(WorkerCommand::ApplyModelTargets);
                    }
                }
                Report::ThinkApplied { revision, effort } => {
                    if revision >= self.model_revision {
                        self.model_revision = revision;
                        self.think_level = effort;
                        if let Some(control) = &self.model_control {
                            self.override_selection = control.override_selection();
                            if self.override_selection.is_none()
                                && self.current_source == Some(ModelSource::Override)
                            {
                                self.current_source = Some(ModelSource::Default);
                            }
                        } else if let Some(selection) = &mut self.override_selection {
                            selection.effort = effort;
                        }
                        if let Some(panel) = &mut self.roles_panel {
                            panel.update_override(self.override_selection.clone());
                        }
                        // Hit rates are not comparable across boundaries, so old usage snapshots
                        // are stale too.
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
                    // A late failure must not clear a newer desired effort level.
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
