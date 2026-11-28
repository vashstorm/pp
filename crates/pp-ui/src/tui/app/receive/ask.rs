//! Worker `Ask`/`DialogFinished` events: open the interaction panel and register the reply
//! channel.

use crate::tui::{
    App, State, WorkerEvent, desktop_notifications, dialog, worker_bridge::DialogOrigin,
};

impl App {
    pub(in crate::tui) fn apply_ask_action(&mut self, action: dialog::Action) {
        match action {
            dialog::Action::None => {}
            #[cfg(debug_assertions)]
            dialog::Action::Cancel if self.ask_reply.is_none() => self.ask_panel = None,
            dialog::Action::Cancel => {
                let child = self
                    .ask_id
                    .as_ref()
                    .is_some_and(|id| id.starts_with("subagent-"));
                self.close_ask_on_exit();
                if !child {
                    self.state = State::Cancelling;
                    self.conversation.cancelling();
                }
            }
            dialog::Action::Submit(answers) => {
                let child = self
                    .ask_id
                    .as_ref()
                    .is_some_and(|id| id.starts_with("subagent-"));
                self.ask_panel = None;
                self.desktop_notifications.dialog_closed();
                self.ask_cancellation = None;
                self.ask_id = None;
                if let Some(reply) = self.ask_reply.take()
                    && reply.send(answers).is_err()
                    && !child
                {
                    if let Some(token) = &self.active_cancellation {
                        token.cancel();
                    }
                    self.state = State::Cancelling;
                    self.conversation.cancelling();
                }
            }
        }
    }

    /// Redraws when an interaction request arrives.
    pub(super) fn on_ask_event(&mut self, event: WorkerEvent) -> bool {
        let mut changed = false;
        match event {
            WorkerEvent::DialogFinished { call_id } => {
                if self.ask_id.as_deref() == Some(call_id.as_str()) {
                    self.desktop_notifications.dialog_closed();
                    self.ask_panel = None;
                    self.ask_reply = None;
                    self.ask_id = None;
                    self.ask_cancellation = None;
                    changed = true;
                }
            }

            WorkerEvent::Ask {
                origin,
                request,
                reply,
                cancellation,
            } => {
                if self
                    .ask_cancellation
                    .as_ref()
                    .is_some_and(|token| token.is_cancelled())
                {
                    self.desktop_notifications.dialog_closed();
                    self.ask_panel = None;
                    self.ask_reply = None;
                    self.ask_id = None;
                    self.ask_cancellation = None;
                }
                if !cancellation.is_cancelled()
                    && (matches!(self.state, State::Running | State::RunningTools)
                        && self
                            .active_cancellation
                            .as_ref()
                            .is_some_and(|token| !token.is_cancelled())
                        || request.call_id.starts_with("subagent-")
                        || request.call_id == "permission-dialog")
                    // A real request may replace a local preview that has no reply channel.
                    && self.ask_reply.is_none()
                    && (self.conversation.has_running_ask(&request.call_id)
                        || request.call_id.starts_with("subagent-")
                        || request.call_id == "permission-dialog")
                {
                    self.menu = None;
                    self.menu_dismissed = true;
                    self.queue_menu = false;
                    // The interaction panel must receive keyboard input first.
                    self.plan_panel = None;
                    self.help_panel = None;
                    self.ask_id = Some(request.call_id.clone());
                    let kind = match origin {
                        DialogOrigin::Ask => Some(desktop_notifications::Kind::Ask),
                        DialogOrigin::Permission => Some(desktop_notifications::Kind::Permission),
                        #[cfg(debug_assertions)]
                        DialogOrigin::Mock => None,
                    };
                    self.desktop_notifications
                        .dialog_opened(kind, &cancellation);
                    self.ask_cancellation = Some(cancellation);
                    self.ask_panel = Some(dialog::Panel::new(request));
                    self.ask_reply = Some(reply);
                    self.conversation.waiting_for_input();
                }
                changed = true;
            }
            _ => {}
        }
        changed
    }
}
