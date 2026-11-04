//! App 与管道阶段观察线程之间的桥：根 epoch、view epoch 与本地面板动作。

use crate::tui::{App, pipes};
use std::sync::mpsc::TryRecvError;

impl App {
    fn ensure_pipes_worker(&mut self) {
        if self.pipes_worker.is_none() {
            self.pipes_worker = Some(pipes::Worker::start());
        }
    }

    fn pipes_send(&self, request: pipes::Request) {
        if let Some(worker) = &self.pipes_worker {
            worker.send(request);
        }
    }

    /// F8 只在没有模态面板占用输入时进入；否则先关闭当前模态。
    pub(in crate::tui) fn pipes_entry_allowed(&self) -> bool {
        self.ask_panel.is_none()
            && self.subagent_panel.is_none()
            && self.auth_panel.is_none()
            && self.roles_panel.is_none()
            && self.model_panel.is_none()
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

    /// 打开阶段浏览；运行中、取消中与空闲都可本地打开，不进入 steering。
    pub(in crate::tui) fn open_pipes(&mut self, run: Option<&str>) {
        self.close_pipes();
        self.menu = None;
        self.menu_dismissed = true;
        self.pipes_view_epoch = self.pipes_view_epoch.wrapping_add(1);
        self.pipes_panel = Some(pipes::Panel::new(run.map(str::to_owned)));
        let Some(store) = self.pipe_store.clone() else {
            if let Some(panel) = &mut self.pipes_panel {
                panel.set_error("Pipe streams are unavailable in this session.".into());
            }
            return;
        };
        self.ensure_pipes_worker();
        self.pipes_send(pipes::Request::Bind {
            epoch: self.pipe_store_epoch,
            view: self.pipes_view_epoch,
            store,
            run: run.map(str::to_owned),
        });
    }

    /// 关闭面板：停止观察线程并释放正文副本；不取消任何 shell 任务。
    pub(in crate::tui) fn close_pipes(&mut self) {
        self.pipes_panel = None;
        self.pipes_view_epoch = self.pipes_view_epoch.wrapping_add(1);
        if let Some(mut worker) = self.pipes_worker.take() {
            worker.shutdown();
        }
    }

    pub(in crate::tui) fn apply_pipes_action(&mut self, action: pipes::Action) {
        match action {
            pipes::Action::None => {}
            pipes::Action::Close => self.close_pipes(),
            pipes::Action::Refresh { full } => {
                let epoch = self.pipe_store_epoch;
                let view = self.pipes_view_epoch;
                self.pipes_send(pipes::Request::Refresh { epoch, view, full });
            }
            pipes::Action::Follow { run, stream } => {
                let _ = (run, stream);
                let epoch = self.pipe_store_epoch;
                let view = self.pipes_view_epoch;
                self.pipes_send(pipes::Request::Refresh {
                    epoch,
                    view,
                    full: true,
                });
            }
            pipes::Action::SelectRun { run } => {
                self.pipes_view_epoch = self.pipes_view_epoch.wrapping_add(1);
                let epoch = self.pipe_store_epoch;
                let view = self.pipes_view_epoch;
                self.pipes_send(pipes::Request::SelectRun { epoch, view, run });
            }
            pipes::Action::SelectStream { run, stream } => {
                self.pipes_view_epoch = self.pipes_view_epoch.wrapping_add(1);
                let epoch = self.pipe_store_epoch;
                let view = self.pipes_view_epoch;
                self.pipes_send(pipes::Request::SelectStream {
                    epoch,
                    view,
                    run,
                    stream,
                });
            }
        }
    }

    /// 收取观察线程回复；旧根/旧 view/关闭后的回复按 epoch 丢弃。
    pub(in crate::tui) fn pipes_tick(&mut self) -> bool {
        let mut changed = false;
        for _ in 0..64 {
            let Some(worker) = &self.pipes_worker else {
                break;
            };
            let reply = match worker.replies.try_recv() {
                Ok(reply) => reply,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.pipes_worker = None;
                    if let Some(panel) = &mut self.pipes_panel {
                        panel.set_error("Pipe observer stopped; press F8 to reopen.".into());
                    }
                    changed = true;
                    break;
                }
            };
            changed |= self.apply_pipes_reply(reply);
        }
        changed
    }

    pub(in crate::tui) fn apply_pipes_reply(&mut self, reply: pipes::Reply) -> bool {
        let (epoch, view) = match &reply {
            pipes::Reply::Runs { epoch, view, .. }
            | pipes::Reply::Body { epoch, view, .. }
            | pipes::Reply::Error { epoch, view, .. } => (*epoch, *view),
        };
        if epoch != self.pipe_store_epoch || view != self.pipes_view_epoch {
            return false;
        }
        let Some(panel) = &mut self.pipes_panel else {
            return false;
        };
        match reply {
            pipes::Reply::Runs {
                runs,
                selected_run,
                selected_stream,
                ..
            } => panel.set_runs(runs, selected_run, selected_stream),
            pipes::Reply::Body { update, .. } => panel.apply_body(update),
            pipes::Reply::Error { message, .. } => {
                panel.set_error(message);
                true
            }
        }
    }
}
