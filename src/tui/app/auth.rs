use crate::tui::{
    App, Level, State, WorkerCommand,
    auth::{self, Action, Reply, Request},
};

impl App {
    pub(in crate::tui) fn open_auth(&mut self, logout: bool) -> bool {
        if !matches!(self.state, State::Ready | State::Unavailable)
            || self.has_pending()
            || self.auth_pending
            || self.skill_panel.is_some()
            || self.skills_pending
            || self.ask_panel.is_some()
            || self.plan_panel.is_some()
            || self.plan_list_panel.is_some()
            || self.model_panel.is_some()
            || self.changes_panel.is_some()
            || self.session_panel.is_some()
            || self.tree_panel.is_some()
        {
            self.notify(
                Level::Error,
                "Finish the current operation before opening authentication.".into(),
            );
            return false;
        }
        self.menu = None;
        self.file_query = None;
        self.menu_dismissed = true;
        self.notifications.clear();
        self.auth_panel = Some(auth::Panel::new(logout));
        self.request_auth(Request::List);
        true
    }

    fn request_auth(&mut self, request: Request) {
        if let Request::Open(url) = request {
            if let Err(error) = self.auth_worker.open(url)
                && let Some(panel) = &mut self.auth_panel
            {
                panel.message = format!("{error:#}");
            }
            return;
        }
        let token = self.auth_worker.token();
        self.auth_pending = true;
        self.auth_cancellation = Some(token.clone());
        if let Some(panel) = &mut self.auth_panel {
            panel.busy = true;
        }
        if self.auth_worker.requests.send((request, token)).is_err() {
            self.auth_pending = false;
            self.auth_cancellation = None;
            if let Some(panel) = &mut self.auth_panel {
                panel.busy = false;
                panel.message = "Authentication worker unavailable; restart pp.".into();
            }
        }
    }

    pub(in crate::tui) fn apply_auth_action(&mut self, action: Action) {
        match action {
            Action::None => {}
            Action::Close => {
                self.auth_panel = None;
                self.menu_dismissed = true;
            }
            Action::Cancel => {
                if let Some(token) = &self.auth_cancellation {
                    token.cancel();
                }
            }
            Action::Login(provider, key) => self.request_auth(Request::Login { provider, key }),
            Action::Discover(provider) => self.request_auth(Request::Discover(provider)),
            Action::Logout(provider) => self.request_auth(Request::Logout(provider)),
            Action::Open(url) => self.request_auth(Request::Open(url)),
        }
    }

    pub(in crate::tui) fn receive_auth(&mut self) -> bool {
        let mut changed = false;
        while let Ok(reply) = self.auth_worker.replies.try_recv() {
            changed = true;
            if let Reply::Progress(progress) = reply {
                if let Some(panel) = &mut self.auth_panel {
                    panel.progress(progress);
                }
                continue;
            }
            if let Reply::Opened(result) = reply {
                if let Err(error) = result
                    && let Some(panel) = &mut self.auth_panel
                {
                    panel.message = format!("{error:#}");
                }
                continue;
            }
            self.auth_pending = false;
            self.auth_cancellation = None;
            let closing = self.auth_panel.as_ref().is_some_and(|panel| panel.closing);
            match reply {
                Reply::Listed(result) => match result {
                    Ok(logged) => {
                        if let Some(panel) = &mut self.auth_panel {
                            panel.listed(logged);
                        }
                    }
                    Err(error) => {
                        if let Some(panel) = &mut self.auth_panel {
                            panel.busy = false;
                            panel.message = format!("{error:#}");
                        }
                    }
                },
                Reply::Updated(result) => {
                    match result {
                        Ok(update) => {
                            if let Some(panel) = &mut self.auth_panel {
                                panel.updated(update);
                            }
                        }
                        Err(error) => {
                            if let Some(panel) = &mut self.auth_panel {
                                panel.busy = false;
                                panel.message = format!("{error:#}");
                            }
                        }
                    }
                    self.reload_model_catalog();
                }
                Reply::Bootstrapped(result) => {
                    match result.and_then(|agent| self.install_agent(*agent)) {
                        Ok(()) => {
                            if let Some(panel) = &mut self.auth_panel {
                                panel.busy = false;
                            }
                        }
                        Err(error) => {
                            self.unavailable = Some(format!("{error:#}"));
                            if let Some(panel) = &mut self.auth_panel {
                                panel.busy = false;
                                panel
                                    .message
                                    .push_str(&format!("\nModel tasks unavailable: {error:#}"));
                            } else {
                                self.notify(Level::Error, format!("{error:#}"));
                            }
                        }
                    }
                }
                Reply::Progress(_) | Reply::Opened(_) => unreachable!(),
            }
            if closing {
                self.auth_panel = None;
            }
        }
        changed
    }

    pub(in crate::tui) fn reload_model_catalog(&mut self) {
        if self.model_refresh_pending {
            return;
        }
        if self.commands.is_some() {
            if let Some(commands) = &self.commands {
                self.model_refresh_pending = commands.send(WorkerCommand::ReloadModels).is_ok();
            }
        } else if !self.auth_pending
            && let Some(factory) = self.bootstrap.clone()
        {
            self.request_auth(Request::Bootstrap(factory));
        }
    }
}
