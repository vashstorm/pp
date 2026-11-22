use crate::commands::PermissionCommand;
use crate::tui::{
    App, Event, KeyCode, KeyEventKind, KeyModifiers, Level, PendingKind, State, WorkerCommand,
    commands, editor, permissions,
};
use crossterm::event::KeyEvent;
use pp_core::interaction::{AskArgs, AskOption, AskQuestion, AskRequest};
use pp_permission::{PermissionDecision, PermissionRequest};

const ONCE: &str = "Allow once";
const SESSION: &str = "Allow for this session";
const PERSIST: &str = "Always allow globally";

pub(super) fn is_permission_shortcut(key: KeyEvent) -> bool {
    let alt_or_meta = KeyModifiers::ALT | KeyModifiers::META;
    let modifiers = key.modifiers - alt_or_meta;
    match key.code {
        KeyCode::F(6) => key.modifiers.is_empty(),
        // 某些终端通过 Meta 报告 Alt，另一些只在字符大小写上体现 Shift。
        KeyCode::Char('a' | 'A') if key.modifiers.intersects(alt_or_meta) => {
            modifiers == KeyModifiers::SHIFT
                || (modifiers.is_empty() && key.code == KeyCode::Char('A'))
        }
        _ => false,
    }
}

pub(super) fn is_permission_command_shortcut(key: KeyEvent) -> bool {
    key.code == KeyCode::F(7) && key.modifiers.is_empty()
}

impl App {
    #[cfg(debug_assertions)]
    pub(super) fn open_permission_preview(&mut self) {
        if !matches!(self.state, State::Ready | State::Unavailable)
            || self.active_cancellation.is_some()
            || self.ask_panel.is_some()
            || self.has_pending()
        {
            self.notify(
                Level::Error,
                "Wait until the TUI is idle before using /test-permission.".into(),
            );
            return;
        }
        let request = permission_prompt(PermissionRequest {
            tool: "bash".into(),
            agent: None,
            reason: "Preview only. Example request for access outside the workspace sandbox."
                .into(),
            session_grant: true,
            persistent_grant: true,
        });
        self.remember_input();
        self.clear_input();
        self.ask_panel = Some(crate::tui::dialog::Panel::new(request));
    }

    pub(in crate::tui) fn bind_bootstrap_permissions(&mut self) {
        if self.permission_mode.is_none()
            && let Some(bootstrap) = &self.bootstrap
        {
            if bootstrap.sandbox.get().is_none()
                && let Ok(settings) =
                    pp_config::config::read_settings(&bootstrap.dir.join("settings.yml"))
            {
                bootstrap.apply_tools_settings(&settings.tools);
            }
            self.permission = bootstrap.permission.as_ref().clone();
            self.sandbox = bootstrap
                .sandbox
                .get()
                .unwrap_or_else(|| std::sync::Arc::new(pp_sandbox::Sandbox::disabled()));
            self.permission_mode = Some(self.permission.snapshot());
        }
    }

    pub(in crate::tui) fn permission_status_text(&self) -> String {
        PermissionCommand::Status
            .apply_with(&self.permission, &self.sandbox)
            .ok()
            .flatten()
            .unwrap_or_else(|| format!("Permission mode: {}", self.permission.snapshot().as_str()))
    }

    pub(super) fn toggle_permission_overlay(&mut self) {
        if self.permission_overlay.is_some() {
            self.permission_overlay = None;
        } else {
            self.permission_overlay =
                Some(permissions::Overlay::new(self.permission_status_text()));
        }
    }

    pub(super) fn handle_permission_overlay_event(
        &mut self,
        event: Event,
        dirty: &mut bool,
    ) -> bool {
        *dirty = true;
        match event {
            Event::Key(key)
                if key.code == KeyCode::Char('c')
                    && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                key.kind == KeyEventKind::Press
            }
            Event::Key(key) => {
                let action = self
                    .permission_overlay
                    .as_mut()
                    .map_or(permissions::OverlayAction::None, |overlay| overlay.key(key));
                match action {
                    permissions::OverlayAction::None => {}
                    permissions::OverlayAction::Close => self.permission_overlay = None,
                    permissions::OverlayAction::Submit(raw) => {
                        match permissions::Overlay::parse_input(&raw) {
                            Ok(command) => {
                                self.apply_permission_command(command);
                                if let Some(overlay) = &mut self.permission_overlay {
                                    overlay.reset_input();
                                }
                            }
                            Err(error) => {
                                let message =
                                    pp_core::logging::diagnostic_text(&format!("{error:#}"));
                                if let Some(overlay) = &mut self.permission_overlay {
                                    overlay.set_error(message);
                                }
                            }
                        }
                    }
                }
                false
            }
            Event::Paste(text) => {
                if let Some(overlay) = &mut self.permission_overlay {
                    overlay.paste(&text);
                }
                false
            }
            _ => false,
        }
    }

    /// 权限命令优先于面板／资源加载守卫执行；非 `/permissions` 命令返回 `false`。
    pub(super) fn try_submit_permission_command(&mut self) -> bool {
        if self.shell_mode {
            return false;
        }
        let prompt = self.input.lines().join("\n");
        let trimmed = prompt.trim_start();
        let is_permissions = trimmed == "/permissions"
            || trimmed
                .strip_prefix("/permissions")
                .is_some_and(|rest| rest.starts_with(char::is_whitespace));
        if !is_permissions {
            return false;
        }
        match commands::parse(&prompt, &self.file_commands) {
            Ok(Some(crate::commands::Command::Permissions(command))) => {
                self.mcp_panel = None;
                self.apply_permission_command(command);
                self.remember_input();
                self.clear_input();
                true
            }
            Err(error) => {
                self.notify(Level::Error, format!("{error:#}"));
                true
            }
            _ => false,
        }
    }

    pub(super) fn complete_permission_mode(
        &mut self,
        mode: pp_config::config::PermissionMode,
        confirm: bool,
    ) -> bool {
        self.input = editor();
        self.input
            .insert_str(format!("/permissions {}", mode.as_str()));
        self.refresh_candidates(true);
        confirm && self.submit(PendingKind::Steering)
    }

    fn dispatch_permission_rule_op(
        &mut self,
        request: permissions::RuleRequest,
        pending_label: &str,
    ) {
        let worker = match self.permission_worker.as_ref() {
            Some(worker) => worker,
            None => match permissions::Worker::new() {
                Ok(worker) => self.permission_worker.insert(worker),
                Err(error) => {
                    let message = pp_core::logging::diagnostic_text(&format!("{error:#}"));
                    if let Some(overlay) = &mut self.permission_overlay {
                        overlay.set_error(message.clone());
                    }
                    self.notify(Level::Error, message);
                    return;
                }
            },
        };
        if let Err(error) = worker.send(request) {
            self.permission_worker = None;
            let message = pp_core::logging::diagnostic_text(&format!("{error:#}"));
            if let Some(overlay) = &mut self.permission_overlay {
                overlay.set_error(message.clone());
            }
            self.notify(Level::Error, message);
            return;
        }
        if let Some(overlay) = &mut self.permission_overlay {
            overlay.set_pending(pending_label);
        }
    }

    pub(in crate::tui) fn receive_permissions(&mut self) -> bool {
        let mut changed = false;
        while let Some(reply) = self
            .permission_worker
            .as_ref()
            .and_then(|worker| worker.replies.try_recv().ok())
        {
            match reply {
                Ok(message) => {
                    self.append("Permissions", &message);
                    if let Some(overlay) = &mut self.permission_overlay {
                        overlay.set_output(message);
                    }
                }
                Err(error) => {
                    let message = pp_core::logging::diagnostic_text(&format!("{error:#}"));
                    if let Some(overlay) = &mut self.permission_overlay {
                        overlay.set_error(message.clone());
                    }
                    self.notify(Level::Error, message);
                }
            }
            changed = true;
        }
        changed
    }

    pub(super) fn apply_permission_command(&mut self, command: PermissionCommand) {
        // 规则查询与撤销涉及 `permission.toml` / `permission.lock` 磁盘锁，
        // 统一交给独立 worker，内存中的模式切换与状态查询继续即时完成。
        match command {
            PermissionCommand::Rules => {
                self.dispatch_permission_rule_op(
                    permissions::RuleRequest::List(self.permission.clone()),
                    "Loading saved permission rules...",
                );
                return;
            }
            PermissionCommand::AddBashRule(rule) => {
                self.dispatch_permission_rule_op(
                    permissions::RuleRequest::SaveBash {
                        gate: self.permission.clone(),
                        rule,
                    },
                    "Saving bash permission rule...",
                );
                return;
            }
            PermissionCommand::Revoke(target) => {
                self.dispatch_permission_rule_op(
                    permissions::RuleRequest::Revoke {
                        gate: self.permission.clone(),
                        target,
                    },
                    "Revoking saved permission rule...",
                );
                return;
            }
            _ => {}
        }
        let changed = matches!(
            command,
            PermissionCommand::Cycle | PermissionCommand::Select(_)
        );
        match command.apply_with(&self.permission, &self.sandbox) {
            Ok(message) => {
                self.permission_mode = Some(self.permission.snapshot());
                if let Some(message) = &message {
                    self.append("Permissions", message);
                }
                if self.permission_overlay.is_some() {
                    let status = message.unwrap_or_else(|| self.permission_status_text());
                    if let Some(overlay) = &mut self.permission_overlay {
                        overlay.set_output(status);
                    }
                }
                if changed {
                    if self.menu.is_some() {
                        self.refresh_candidates(false);
                    }
                    if let Some(commands) = &self.commands
                        && commands.send(WorkerCommand::SyncPermissions).is_err()
                    {
                        self.fail_worker();
                    }
                }
            }
            Err(error) => {
                let message = pp_core::logging::diagnostic_text(&format!("{error:#}"));
                if let Some(overlay) = &mut self.permission_overlay {
                    overlay.set_error(message.clone());
                }
                self.notify(Level::Error, message);
            }
        }
    }

    pub(super) fn submit_sandbox_command(
        &mut self,
        command: WorkerCommand,
        report_busy: bool,
    ) -> bool {
        if !matches!(self.state, State::Ready) || self.has_pending() {
            if report_busy {
                let message = if matches!(self.state, State::Unavailable) {
                    self.unavailable
                        .clone()
                        .unwrap_or_else(|| "Sandbox unavailable.".into())
                } else {
                    "Finish or cancel the current task before changing sandbox mode.".into()
                };
                self.notify(Level::Error, message);
            }
            return false;
        }
        let Some(commands) = &self.commands else {
            return false;
        };
        if commands.send(command).is_err() {
            self.fail_worker();
            return false;
        }
        true
    }
}

fn permission_prompt(request: PermissionRequest) -> AskRequest {
    let option = |label: &str, description: Option<&str>| AskOption {
        label: label.into(),
        description: description.map(str::to_owned),
        preview: None,
    };
    let mut options = vec![option(ONCE, None)];
    if request.session_grant {
        options.push(option(SESSION, Some("Includes subagents")));
    }
    if request.persistent_grant {
        options.push(option(PERSIST, Some("Saved in permission.toml")));
    }
    options.push(option("Deny", None));
    let args = AskArgs {
        questions: vec![AskQuestion {
            id: "permission".into(),
            question: request.reason,
            options,
            header: Some(request.tool),
            multi: false,
            recommended: None,
        }],
    };
    let call_id = request.agent.as_ref().map_or_else(
        || "permission-dialog".into(),
        |id| format!("subagent-{id}-permission"),
    );
    AskRequest {
        call_id,
        args: std::sync::Arc::new(args),
    }
}

pub(in crate::tui) fn request_permission(
    events: &std::sync::mpsc::SyncSender<crate::tui::WorkerEvent>,
    request: PermissionRequest,
    token: &pp_core::cancellation::CancellationToken,
) -> anyhow::Result<PermissionDecision> {
    let PermissionRequest {
        session_grant,
        persistent_grant,
        ..
    } = request;
    let answers = crate::tui::request_ask(events, permission_prompt(request), token)?;
    let selected = answers
        .first()
        .and_then(|answer| answer.selected_options.first());
    Ok(match selected.map(String::as_str) {
        Some(ONCE) => PermissionDecision::Approve,
        Some(SESSION) if session_grant => PermissionDecision::ApproveSession,
        Some(PERSIST) if persistent_grant => PermissionDecision::ApprovePersistently,
        _ => PermissionDecision::Deny,
    })
}
