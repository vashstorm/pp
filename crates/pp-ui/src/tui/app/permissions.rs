use crate::tui::{App, KeyCode, KeyModifiers, Level, PendingKind, State, WorkerCommand, editor};
use crossterm::event::KeyEvent;

pub(super) fn is_permission_shortcut(key: KeyEvent) -> bool {
    let alt_shift = KeyModifiers::ALT | KeyModifiers::SHIFT;
    matches!(key.code, KeyCode::Char('a' | 'A')) && key.modifiers == alt_shift
        || key.code == KeyCode::Char('A') && key.modifiers == KeyModifiers::ALT
        || key.code == KeyCode::F(6) && key.modifiers.is_empty()
}

impl App {
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

    pub(super) fn submit_permission_command(
        &mut self,
        command: WorkerCommand,
        report_busy: bool,
    ) -> bool {
        if !matches!(self.state, State::Ready) || self.has_pending() {
            if report_busy {
                let message = if matches!(self.state, State::Unavailable) {
                    self.unavailable
                        .clone()
                        .unwrap_or_else(|| "Permissions unavailable.".into())
                } else {
                    "Finish or cancel the current task before changing permissions.".into()
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

pub(super) fn request_approval(
    events: &std::sync::mpsc::SyncSender<crate::tui::WorkerEvent>,
    request: pp_permission::ApprovalRequest,
    token: &pp_core::cancellation::CancellationToken,
) -> anyhow::Result<pp_permission::ApprovalDecision> {
    use pp_core::interaction::{AskArgs, AskOption, AskQuestion, AskRequest};
    use pp_permission::ApprovalDecision;

    const ONCE: &str = "Approve once";
    const SESSION: &str = "Allow for this session";
    const PERSIST: &str = "Always allow in this project";
    let option = |label: &str, description: &str| AskOption {
        label: label.into(),
        description: Some(description.into()),
        preview: None,
    };
    let mut options = vec![option(ONCE, "Run the current tool call only")];
    if request.session_grant {
        options.push(option(
            SESSION,
            "Granted to the root session and its subagents",
        ));
    }
    if request.persistent_grant {
        options.push(option(
            PERSIST,
            "Save a rule for this exact command and permission scope",
        ));
    }
    options.push(option("Deny", "Return tool approval denied"));
    let args = AskArgs {
        questions: vec![AskQuestion {
            id: "approval".into(),
            question: format!("{}: {}", request.tool, request.reason),
            options,
            header: None,
            multi: false,
            recommended: None,
        }],
    };
    let call_id = request.agent.as_ref().map_or_else(
        || "approval-dialog".into(),
        |id| format!("subagent-{id}-approval"),
    );
    let answers = crate::tui::request_ask(
        events,
        AskRequest {
            call_id,
            args: std::sync::Arc::new(args),
        },
        token,
    )?;
    let selected = answers
        .first()
        .and_then(|answer| answer.selected_options.first());
    Ok(match selected.map(String::as_str) {
        Some(ONCE) => ApprovalDecision::Approve,
        Some(SESSION) if request.session_grant => ApprovalDecision::ApproveSession,
        Some(PERSIST) if request.persistent_grant => ApprovalDecision::ApprovePersistently,
        _ => ApprovalDecision::Deny,
    })
}
