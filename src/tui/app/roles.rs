use crate::tui::{App, Level, State, WorkerCommand, roles};

impl App {
    pub(in crate::tui) fn open_roles(&mut self) {
        if !self.shortcut_ready() {
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
        self.clear_input();
        self.notifications.clear();
        if self.bootstrap.is_some() {
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
