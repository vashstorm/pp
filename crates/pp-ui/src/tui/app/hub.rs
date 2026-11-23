//! App 与 Agent Hub 观察线程之间的桥：根 epoch、请求 id、child 草稿与控制 ACK。

use crate::tui::{App, Event, KeyEventKind, KeyModifiers, WorkerCommand, subagents};
use crossterm::event::{Event as CrosstermEvent, KeyCode};
use pp_runtime::subagent::Binding;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

/// 没有任何根绑定时面板显示的固定提示：不是运行错误，而是当前会话不提供 task 能力。
const UNAVAILABLE: &str = "Subagents are unavailable in this session; no binding was granted.";
/// 历史投影按回复让出主循环，使积压期间仍可绘制和处理按键。
const REPLY_BUDGET: Duration = Duration::from_millis(4);

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

    /// 打开 Hub；未获得 Binding 时先请 worker 重建，再决定空态还是 Roster。
    pub(in crate::tui) fn open_hub(&mut self) {
        self.menu = None;
        self.menu_dismissed = true;
        let projection = std::mem::take(&mut self.hub_projection);
        let mut panel = subagents::Panel::with_projection(projection);
        if let Some(error) = self.hub_preload_error.take() {
            panel.set_error(error);
        }
        self.subagent_panel = Some(panel);
        self.subagent_status = None;
        match self.hub_binding() {
            Some(binding) => {
                let same = self.hub_attached
                    && self
                        .hub_bound
                        .as_ref()
                        .is_some_and(|bound| bound.same_root(&binding));
                if same {
                    self.ensure_hub_worker();
                    let epoch = self.hub_root_epoch;
                    self.hub_send(subagents::Request::SetVisible {
                        epoch,
                        visible: true,
                    });
                } else {
                    self.hub_bound = Some(binding);
                    self.hub_attach(true);
                }
            }
            // `/resume`、`/tree`、`/new` 与计划交接会停掉旧根；
            // 此时绑定为空并不代表会话没有历史子 Agent，交由 worker 用当前会话目录重建。
            None => {
                self.ensure_hub_worker();
                self.request_subagents();
            }
        }
    }

    /// 请 worker 重建根绑定：成功直接写回共享 slot，失败只影响面板提示。
    fn request_subagents(&mut self) {
        let sent = self
            .commands
            .as_ref()
            .is_some_and(|sender| sender.send(WorkerCommand::EnsureSubagents).is_ok());
        if sent {
            return;
        }
        self.fail_worker();
        self.set_hub_error(UNAVAILABLE);
    }

    /// worker 重建结果：成功按新根重新 attach，失败保留会话并只在面板报告。
    pub(in crate::tui) fn subagents_ensured(&mut self, result: anyhow::Result<()>) {
        if self.subagent_panel.is_none() {
            return;
        }
        match result {
            Ok(()) => match self.hub_binding() {
                Some(binding) => {
                    let same = self.hub_attached
                        && self
                            .hub_bound
                            .as_ref()
                            .is_some_and(|bound| bound.same_root(&binding));
                    self.hub_bound = Some(binding);
                    self.subagent_status = None;
                    if same {
                        let epoch = self.hub_root_epoch;
                        self.hub_send(subagents::Request::SetVisible {
                            epoch,
                            visible: true,
                        });
                    } else {
                        self.hub_attach(true);
                    }
                }
                None => self.set_hub_error(UNAVAILABLE),
            },
            Err(error) => self.set_hub_error(format!("Subagents are unavailable: {error:#}")),
        }
    }

    fn set_hub_error(&mut self, message: impl Into<String>) {
        if let Some(panel) = &mut self.subagent_panel {
            panel.set_error(message.into());
        }
    }

    /// 绑定并开始读取当前根；换根或首次附着时清空旧投影。
    fn hub_attach(&mut self, visible: bool) {
        let Some(binding) = self.hub_binding() else {
            return;
        };
        self.ensure_hub_worker();
        self.hub_projection = subagents::Projection::default();
        self.hub_preload_error = None;
        if let Some(panel) = &mut self.subagent_panel {
            panel.reset_projection();
        }
        self.hub_root_epoch += 1;
        self.hub_view_epoch += 1;
        self.hub_attached = true;
        let epoch = self.hub_root_epoch;
        self.hub_send(subagents::Request::Bind {
            epoch,
            binding,
            visible,
        });
    }

    /// 关闭 Hub：保留投影与游标以便重开复用，通知观察线程停止后台轮询。
    pub(in crate::tui) fn close_hub(&mut self) {
        if let Some(panel) = self.subagent_panel.take() {
            let view = panel.detail_view();
            if let Some(view) = view {
                self.hub_send(subagents::Request::CloseDetail {
                    epoch: self.hub_root_epoch,
                    view,
                });
            }
            self.hub_projection = panel.into_projection();
        }
        if self.hub_attached {
            let epoch = self.hub_root_epoch;
            self.hub_send(subagents::Request::SetVisible {
                epoch,
                visible: false,
            });
        }
        self.menu_dismissed = true;
    }

    /// 根变化时清空旧草稿、投影缓存与详情状态；已有历史子 Agent 时静默预热。
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
        self.subagent_status = None;
        self.child_drafts.clear();
        self.hub_sends.clear();
        self.hub_projection = subagents::Projection::default();
        self.hub_preload_error = None;
        if let Some(panel) = &mut self.subagent_panel {
            panel.reset_projection();
        }
        self.hub_attached = false;
        self.hub_view_epoch += 1;
        let visible = self.subagent_panel.is_some();
        if let Some(binding) = &self.hub_bound {
            if visible || binding.has_agents() {
                self.hub_attach(visible);
            } else {
                let epoch = self.hub_root_epoch;
                self.hub_send(subagents::Request::Unbind { epoch });
            }
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
                subagents::Command::CloseDetail { view } => {
                    let epoch = self.hub_root_epoch;
                    self.hub_send(subagents::Request::CloseDetail { epoch, view });
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
            }
        }
    }

    /// 收取观察线程回复；旧根/旧详情/关闭后的页面按 epoch 丢弃。
    pub(in crate::tui) fn receive_hub(&mut self) -> bool {
        let mut changed = false;
        let started = Instant::now();
        for _ in 0..64 {
            let Some(worker) = &self.hub_worker else {
                break;
            };
            let reply = match worker.replies.try_recv() {
                Ok(reply) => reply,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.hub_worker = None;
                    self.hub_attached = false;
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
            if started.elapsed() >= REPLY_BUDGET {
                break;
            }
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
            | subagents::Reply::ActivityBatch { epoch, .. }
            | subagents::Reply::Detail { epoch, .. }
            | subagents::Reply::Reset { epoch, .. }
            | subagents::Reply::Error { epoch, .. }
            | subagents::Reply::Control { epoch, .. } => *epoch,
        };
        if epoch != self.hub_root_epoch {
            return false;
        }
        let view = match &reply {
            subagents::Reply::Detail { view, .. } => Some(*view),
            subagents::Reply::Error { view, .. } => *view,
            _ => None,
        };
        if let Some(view) = view
            && self
                .subagent_panel
                .as_ref()
                .and_then(|panel| panel.detail_view())
                != Some(view)
        {
            return false;
        }
        if let Some(panel) = &mut self.subagent_panel {
            panel.apply(reply);
            return true;
        }
        // Hub 未打开时在后台静默预热投影缓存；不触发主界面多余重绘。
        let clock = subagents::now();
        match reply {
            subagents::Reply::Snapshot { snapshot, .. } => {
                self.hub_projection.apply_snapshot(*snapshot, clock);
            }
            subagents::Reply::Activity { agent, page, .. } => {
                if !page.warnings.is_empty() {
                    self.hub_preload_error = Some(page.warnings.join("; "));
                }
                self.hub_projection.ingest(&agent, &page, clock);
            }
            subagents::Reply::ActivityBatch { pages, .. } => {
                let mut warnings = Vec::new();
                for (_, page) in &pages {
                    warnings.extend(page.warnings.iter().cloned());
                }
                if !warnings.is_empty() {
                    self.hub_preload_error = Some(warnings.join("; "));
                }
                self.hub_projection.ingest_overview_batch(&pages, clock);
            }
            subagents::Reply::Reset { agent, .. } => {
                self.hub_projection.reset(&agent);
            }
            subagents::Reply::Error {
                view: None,
                message,
                ..
            } => {
                self.hub_preload_error = Some(message);
            }
            subagents::Reply::Detail { .. }
            | subagents::Reply::Error { view: Some(_), .. }
            | subagents::Reply::Control { .. } => {}
        }
        false
    }

    /// Alt+A 只在主视图与 Hub 接收，不被其他模态面板抢占。
    pub(in crate::tui) fn hub_toggle_allowed(&self) -> bool {
        self.subagent_panel.is_none()
            && self.ask_panel.is_none()
            && self.auth_panel.is_none()
            && self.active_settings_tab().is_none()
            && self.model_panel.is_none()
            && self.plan_panel.is_none()
            && self.plan_list_panel.is_none()
            && self.plan_leave_confirmation.is_none()
            && self.session_panel.is_none()
            && self.pipes_panel.is_none()
            && self.tree_panel.is_none()
            && self.mcp_panel.is_none()
            && !self.history_browser
    }

    /// 每轮 tick：时钟、每秒刷新与 worker 回复。
    pub(in crate::tui) fn hub_tick(&mut self) -> bool {
        let mut changed = self.receive_hub();
        let previous_count = self.subagent_status.map_or(0, |(_, count)| count);
        changed |= self.hub_sync_root();
        self.subagent_status = self.hub_bound.as_ref().map(|binding| {
            let revision = binding.hub_revision();
            match self.subagent_status {
                Some(snapshot) if snapshot.0 == revision => snapshot,
                _ => (revision, binding.running_subagent_count()),
            }
        });
        changed |= self.subagent_status.map_or(0, |(_, count)| count) != previous_count;
        let now = Instant::now();
        if let Some(panel) = &mut self.subagent_panel {
            changed |= panel.tick(subagents::now(), now, self.hub_bound.as_ref());
        }
        changed
    }
}

/// 主循环事件里只有按下的 Alt+A 才切换 Hub。
pub(in crate::tui) fn is_hub_toggle(event: &CrosstermEvent) -> bool {
    matches!(event, CrosstermEvent::Key(key)
        if key.code == KeyCode::Char('a')
            && key.modifiers == KeyModifiers::ALT
            && key.kind == KeyEventKind::Press)
}
