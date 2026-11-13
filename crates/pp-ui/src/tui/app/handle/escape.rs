//! Esc 键语义：关闭历史浏览／shell 模式、取消运行或准备会话树入口。

use crossterm::event::{KeyEvent, KeyEventKind};

use crate::tui::{App, Instant, State};

impl App {
    pub(super) fn handle_escape_key(&mut self, key: KeyEvent) {
        if self.history_browser {
            self.history_browser = false;
            self.conversation.follow();
            self.escape_tap.reset();
            return;
        }
        if self.shell_mode
            && !matches!(
                self.state,
                State::RunningShell
                    | State::Running
                    | State::RunningTools
                    | State::Compacting
                    | State::ReviewingPlan
            )
        {
            self.shell_mode = false;
            self.menu_dismissed = true;
            self.escape_tap.reset();
            return;
        }
        if key.kind == KeyEventKind::Press && matches!(self.state, State::RunningShell) {
            if let Some(cancellation) = &self.active_cancellation {
                cancellation.cancel();
                self.secondary_status = Some("Cancelling shell...".into());
            }
            self.escape_tap.reset();
            return;
        }
        if key.kind == KeyEventKind::Press
            && matches!(
                self.state,
                State::Running
                    | State::RunningTools
                    | State::ManagingMcp
                    | State::Compacting
                    | State::ReviewingPlan
            )
            && let Some(cancellation) = &self.active_cancellation
        {
            cancellation.cancel();
            self.queue_paused = true;
            match self.state {
                State::ManagingMcp => {
                    self.secondary_status = Some("Cancelling MCP management…".into());
                }
                State::Compacting => {
                    self.secondary_status = Some("Cancelling context...".into());
                }
                State::RunningTools => {
                    self.secondary_status = Some("Cancelling mock test...".into());
                }
                State::ReviewingPlan => {
                    // 取消 reviewer；保持 ReviewingPlan，等待 worker
                    // 的 PlanReviewed 受控收尾后回五项菜单。
                    if let Some(review) = &mut self.review_transcript {
                        review.cancelling();
                    }
                    self.secondary_status = Some("Cancelling review...".into());
                }
                _ => self.conversation.cancelling(),
            }
            if !matches!(self.state, State::ReviewingPlan) {
                self.state = State::Cancelling;
            }
            self.escape_tap.reset();
        } else if key.kind == KeyEventKind::Press
            && matches!(self.state, State::Ready)
            && !self.skills_pending
            && self.input.lines().iter().all(String::is_empty)
        {
            if self.escape_tap.press(Instant::now()) {
                self.open_tree();
            }
        } else {
            self.escape_tap.reset();
        }
    }
}
