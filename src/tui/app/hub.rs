//! App 与 Agent Hub 观察线程之间的桥：根 epoch、请求 id、child 草稿与控制 ACK。

use crate::subagent::Binding;
use crate::tui::{App, Event, KeyEventKind, KeyModifiers, subagents};
use crossterm::event::{Event as CrosstermEvent, KeyCode};
use std::sync::mpsc::TryRecvError;

/// 一次已发出的控制请求；成功后才清掉同一份草稿。
pub(in crate::tui) struct HubSend {
    agent: String,
    text: String,
    revision: u64,
    submit: bool,
}

impl App {
    fn hub_binding(&self) -> Option<Binding> {
        self.subagent_control
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn ensure_hub_worker(&mut self) {
        if self.hub_worker.is_none() {
            self.hub_worker = Some(subagents::Worker::start());
        }
    }

    fn hub_send(&self, request: subagents::Request) {
        if let Some(worker) = &self.hub_worker {
            worker.send(request);
        }
    }

    /// 打开 Hub；未获得 Binding 时显示不可用空态而不是 panic。
    pub(in crate::tui) fn open_hub(&mut self, roles: bool) {
        self.subagent_panel = Some(subagents::Panel::new(roles));
        self.ensure_hub_worker();
        let commands = self
            .subagent_panel
            .as_mut()
            .map(|panel| panel.take_commands())
            .unwrap_or_default();
        let binding = self.hub_binding();
        if binding.is_none() {
            if let Some(panel) = &mut self.subagent_panel {
                panel.set_error(
                    "Subagents are unavailable in this session; no binding was granted.".into(),
                );
            }
            return;
        }
        self.hub_bound = binding;
        self.hub_attach();
        self.apply_hub_commands(commands);
    }

    /// 用当前根重新扫描；同一根保留 child 草稿。
    fn hub_attach(&mut self) {
        let Some(binding) = self.hub_binding() else {
            return;
        };
        self.ensure_hub_worker();
        self.hub_root_epoch += 1;
        self.hub_view_epoch += 1;
        let epoch = self.hub_root_epoch;
        self.hub_send(subagents::Request::Bind { epoch, binding });
    }

    /// 关闭 Hub：只停止观察，不取消任何 child，也不丢弃在途控制 ACK。
    pub(in crate::tui) fn close_hub(&mut self) {
        if let Some(panel) = self.subagent_panel.take() {
            let view = panel.detail_view();
            if let Some(view) = view {
                self.hub_send(subagents::Request::CloseDetail {
                    epoch: self.hub_root_epoch,
                    view,
                });
            }
        }
        let epoch = self.hub_root_epoch;
        self.hub_send(subagents::Request::Unbind { epoch });
        self.menu_dismissed = true;
    }

    /// 根变化时清空旧草稿与详情缓存；pending→真实目录不算换根。
    pub(in crate::tui) fn hub_sync_root(&mut self) -> bool {
        let current = self.hub_binding();
        let same = match (&current, &self.hub_bound) {
            (Some(left), Some(right)) => left.same_root(right),
            (None, None) => true,
            _ => false,
        };
        if same {
            return false;
        }
        self.hub_bound = current;
        self.child_drafts.clear();
        self.hub_sends.clear();
        self.hub_view_epoch += 1;
        if self.subagent_panel.is_some() {
            self.hub_attach();
        } else {
            let epoch = self.hub_root_epoch;
            self.hub_send(subagents::Request::Unbind { epoch });
        }
        true
    }

    /// 把面板事件转成观察线程请求。
    pub(in crate::tui) fn hub_handle(&mut self, event: Event) -> (bool, Vec<subagents::Command>) {
        let Some(panel) = &mut self.subagent_panel else {
            return (false, Vec::new());
        };
        let close = panel.handle(event, &mut self.child_drafts);
        let commands = panel.take_commands();
        (close, commands)
    }

    pub(in crate::tui) fn apply_hub_commands(&mut self, commands: Vec<subagents::Command>) {
        for command in commands {
            match command {
                subagents::Command::Open { agent, anchor } => {
                    self.hub_view_epoch += 1;
                    let view = self.hub_view_epoch;
                    if let Some(panel) = &mut self.subagent_panel {
                        panel.opened(&agent, view, anchor, self.conversation.fresh());
                    }
                    let epoch = self.hub_root_epoch;
                    self.hub_send(subagents::Request::OpenDetail { epoch, view, agent });
                }
                subagents::Command::FocusEditor => {
                    if let Some(panel) = &mut self.subagent_panel {
                        panel.focus_editor();
                    }
                }
                subagents::Command::CloseDetail => {
                    let view = self
                        .subagent_panel
                        .as_ref()
                        .and_then(|panel| panel.detail_view());
                    if let Some(view) = view {
                        let epoch = self.hub_root_epoch;
                        self.hub_send(subagents::Request::CloseDetail { epoch, view });
                    }
                }
                subagents::Command::Submit {
                    agent,
                    text,
                    revision,
                } => {
                    self.hub_request += 1;
                    let request = self.hub_request;
                    self.hub_sends.insert(
                        request,
                        HubSend {
                            agent: agent.clone(),
                            text: text.clone(),
                            revision,
                            submit: true,
                        },
                    );
                    let epoch = self.hub_root_epoch;
                    self.hub_send(subagents::Request::Control {
                        epoch,
                        request,
                        action: subagents::Control::Send,
                        agent,
                        text: Some(text),
                    });
                }
                subagents::Command::Control {
                    agent,
                    action,
                    text,
                } => {
                    self.hub_request += 1;
                    let request = self.hub_request;
                    self.hub_sends.insert(
                        request,
                        HubSend {
                            agent: agent.clone(),
                            text: String::new(),
                            revision: 0,
                            submit: false,
                        },
                    );
                    let epoch = self.hub_root_epoch;
                    self.hub_send(subagents::Request::Control {
                        epoch,
                        request,
                        action,
                        agent,
                        text,
                    });
                }
                subagents::Command::Refresh => {
                    let epoch = self.hub_root_epoch;
                    self.hub_send(subagents::Request::Refresh { epoch });
                }
                subagents::Command::Roles => {
                    let epoch = self.hub_root_epoch;
                    self.hub_send(subagents::Request::Roles { epoch });
                }
            }
        }
    }

    /// 收取观察线程回复；旧根/旧详情/关闭后的页面按 epoch 丢弃。
    pub(in crate::tui) fn receive_hub(&mut self) -> bool {
        let mut changed = false;
        for _ in 0..64 {
            let Some(worker) = &self.hub_worker else {
                break;
            };
            let reply = match worker.replies.try_recv() {
                Ok(reply) => reply,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.hub_worker = None;
                    if let Some(panel) = &mut self.subagent_panel {
                        panel.set_error(
                            "Agent hub observer stopped; press F5 after reopening.".into(),
                        );
                    }
                    changed = true;
                    break;
                }
            };
            changed |= self.apply_hub_reply(reply);
        }
        changed
    }

    fn apply_hub_reply(&mut self, reply: subagents::Reply) -> bool {
        // 控制 ACK 只按根身份（request id 表在换根时清空）处理，不按详情 view 丢弃。
        if let subagents::Reply::Control {
            request,
            agent,
            result,
            ..
        } = reply
        {
            let Some(send) = self.hub_sends.remove(&request) else {
                return false;
            };
            if result.is_ok() && send.submit {
                // 成功才清掉同一份未修改的草稿，并唤醒父方投递。
                self.child_drafts
                    .clear_if_unchanged(&send.agent, send.revision, &send.text);
                self.subagent_autowake = true;
            }
            if let Some(panel) = &mut self.subagent_panel {
                panel.send_settled(&agent, result);
            }
            return true;
        }
        let epoch = match &reply {
            subagents::Reply::Snapshot { epoch, .. }
            | subagents::Reply::Activity { epoch, .. }
            | subagents::Reply::Detail { epoch, .. }
            | subagents::Reply::Reset { epoch, .. }
            | subagents::Reply::Roles { epoch, .. }
            | subagents::Reply::Error { epoch, .. }
            | subagents::Reply::Control { epoch, .. } => *epoch,
        };
        if epoch != self.hub_root_epoch {
            return false;
        }
        if let subagents::Reply::Detail { view, .. } = &reply
            && self
                .subagent_panel
                .as_ref()
                .and_then(|panel| panel.detail_view())
                != Some(*view)
        {
            return false;
        }
        if let subagents::Reply::Error { message, .. } = reply {
            if let Some(panel) = &mut self.subagent_panel {
                panel.set_error(message);
                return true;
            }
            return false;
        }
        let Some(panel) = &mut self.subagent_panel else {
            return false;
        };
        panel.apply(reply);
        true
    }

    /// Alt+A 只在主视图与 Hub 接收，不被其他模态面板抢占。
    pub(in crate::tui) fn hub_toggle_allowed(&self) -> bool {
        self.subagent_panel.is_none()
            && self.ask_panel.is_none()
            && self.auth_panel.is_none()
            && self.roles_panel.is_none()
            && self.model_panel.is_none()
            && self.menu.is_none()
            && self.plan_panel.is_none()
            && self.plan_list_panel.is_none()
            && self.plan_leave_confirmation.is_none()
            && self.session_panel.is_none()
            && self.skill_panel.is_none()
            && self.changes_panel.is_none()
            && self.tree_panel.is_none()
            && self.mcp_panel.is_none()
            && !self.history_browser
    }

    /// 每轮 tick：时钟、每秒刷新与 worker 回复。
    pub(in crate::tui) fn hub_tick(&mut self) -> bool {
        let mut changed = self.receive_hub();
        changed |= self.hub_sync_root();
        let now = std::time::Instant::now();
        if let Some(panel) = &mut self.subagent_panel {
            panel.tick(subagents::now(), now);
        }
        changed
    }
}

/// 主循环事件里只有按下的 Alt+A 才切换 Hub。
pub(in crate::tui) fn is_hub_toggle(event: &CrosstermEvent) -> bool {
    matches!(event, CrosstermEvent::Key(key)
        if key.code == KeyCode::Char('a')
            && key.modifiers.contains(KeyModifiers::ALT)
            && key.kind == KeyEventKind::Press)
}
