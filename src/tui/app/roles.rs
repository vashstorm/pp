use crate::tui::{App, Level, State, WorkerCommand, roles};

impl App {
    pub(in crate::tui) fn open_roles(&mut self) {
        if !self.model_selection_ready() {
            return;
        }
        self.roles_panel = Some(roles::Panel::new(
            self.choices.clone(),
            self.model_roles.clone(),
            self.current_model
                .as_ref()
                .map(|model| model.reference.clone())
                .unwrap_or_default(),
            self.think_level,
        ));
        self.menu = None;
        self.menu_dismissed = true;

        self.notifications.clear();
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
            roles::Action::Save(role, assignment) => {
                if let Some(control) = &self.model_control {
                    if control.submit(crate::model_control::Change::Role(role, assignment)) {
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
            let accepted = control.submit(crate::model_control::Change::Select(reference));
            if !accepted {
                self.notify(Level::Error, "Model control unavailable".into());
            }
            accepted
        } else {
            self.send(WorkerCommand::SwitchModel(reference), State::SwitchingModel)
        }
    }
    pub(in crate::tui) fn poll_model_control(&mut self) -> bool {
        use crate::model_control::Report;
        let reports = self
            .model_control
            .as_ref()
            .map(|c| c.registry.reports())
            .unwrap_or_default();
        let changed = !reports.is_empty();
        for report in reports {
            match report {
                Report::Cleared { role, revision } => {
                    if role == crate::config::ModelRole::Default
                        && self
                            .pending_model
                            .as_ref()
                            .is_some_and(|(pending, _)| *pending <= revision)
                    {
                        self.pending_model = None;
                    }
                    self.secondary_status = None;
                }
                Report::Queued {
                    role,
                    revision,
                    reference,
                } => {
                    if role == crate::config::ModelRole::Default {
                        self.pending_model = Some((revision, reference.clone()));
                    }
                    if let Some(panel) = &mut self.roles_panel {
                        panel.runtime_status(format!(
                            "{} → {} · pending safe boundary",
                            role.name(),
                            reference
                        ));
                    }
                    self.secondary_status = Some(format!(
                        "{} → {} · pending safe boundary",
                        role.name(),
                        reference
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
                    self.model_roles = roles;
                    self.default_model_reference = Some(default);
                    self.default_think_level = effort;
                    if let Some(panel) = &mut self.roles_panel {
                        panel.saved();
                    }
                    self.notify(
                        Level::Normal,
                        "Role configuration saved; applying at safe boundaries".into(),
                    );
                }
                Report::Applied {
                    agent,
                    revision,
                    model,
                    effort,
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
                    } else {
                        self.current_model = Some(model);
                        self.think_level = effort;
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
            }
        }
        changed
    }
}
