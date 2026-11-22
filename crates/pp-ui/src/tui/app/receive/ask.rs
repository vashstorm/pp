//! worker `Ask`／`DialogFinished` 事件：打开交互面板并登记回复通道。

use crate::tui::{App, State, WorkerEvent, dialog};

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

    /// 交互请求到达时重新绘制。
    pub(super) fn on_ask_event(&mut self, event: WorkerEvent) -> bool {
        let mut changed = false;
        match event {
            WorkerEvent::DialogFinished { call_id } => {
                if self.ask_id.as_deref() == Some(call_id.as_str()) {
                    self.ask_panel = None;
                    self.ask_reply = None;
                    self.ask_id = None;
                    self.ask_cancellation = None;
                    changed = true;
                }
            }

            WorkerEvent::Ask {
                request,
                reply,
                cancellation,
            } => {
                if self
                    .ask_cancellation
                    .as_ref()
                    .is_some_and(|token| token.is_cancelled())
                {
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
                    // 真实请求可以替换没有回复通道的本地预览。
                    && self.ask_reply.is_none()
                    && (self.conversation.has_running_ask(&request.call_id)
                        || request.call_id.starts_with("subagent-")
                        || request.call_id == "permission-dialog")
                {
                    self.menu = None;
                    self.menu_dismissed = true;
                    self.queue_menu = false;
                    // 交互面板需要优先接收键盘输入。
                    self.plan_panel = None;
                    self.ask_id = Some(request.call_id.clone());
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
