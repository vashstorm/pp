//! worker `Ask`／`DialogFinished` 事件：打开提问面板并登记回复通道。

use crate::tui::{App, State, WorkerEvent, ask};

impl App {
    /// 提问事件族；面板打开时无条件视为已变化。
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
                        || request.call_id == "approval-dialog")
                    && self.ask_panel.is_none()
                    && (self.conversation.has_running_ask(&request.call_id)
                        || request.call_id.starts_with("subagent-")
                        || request.call_id == "approval-dialog")
                {
                    self.menu = None;
                    self.menu_dismissed = true;
                    self.queue_menu = false;
                    // 进度视图不能挡住 ask 的键盘路由；先让位给 ask 面板。
                    self.plan_panel = None;
                    self.ask_id = Some(request.call_id.clone());
                    self.ask_cancellation = Some(cancellation);
                    self.ask_panel = Some(ask::Panel::new(request));
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
