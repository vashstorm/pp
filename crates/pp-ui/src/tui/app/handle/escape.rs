//! Esc key semantics: close history browsing/shell mode, cancel the run or prepare the session
//! tree entry.

use crossterm::event::{KeyEvent, KeyEventKind};

use crate::tui::{App, Instant, State};

impl App {
    pub(super) fn handle_escape_key(&mut self, key: KeyEvent) {
        if self.history_browser {
            self.history_browser = false;
            // Restore only the currently displayed transcript: after browsing the review tab,
            // Esc resumes following instead of unconditionally pulling the main conversation to
            // the bottom (whose scroll position stays put).
            self.active_transcript().follow();
            self.escape_tap.reset();
            return;
        }
        if self.shell_mode
            && !matches!(
                self.state,
                State::RunningShell
                    | State::Running
                    | State::RunningTools
                    | State::RunningWebSearch
                    | State::Compacting
                    | State::ReviewingPlan
            )
        {
            self.shell_mode = false;
            self.menu_dismissed = true;
            self.escape_tap.reset();
            return;
        }
        if key.kind == KeyEventKind::Press
            && matches!(self.state, State::RunningShell | State::RunningWebSearch)
        {
            if let Some(cancellation) = &self.active_cancellation {
                cancellation.cancel();
                self.secondary_status = Some(if matches!(self.state, State::RunningShell) {
                    "Cancelling shell...".into()
                } else {
                    "Cancelling web search...".into()
                });
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
                    // Cancel the reviewer but stay in ReviewingPlan until the worker's controlled
                    // PlanReviewed shutdown returns to the five-item menu.
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
            && self.active_settings_tab().is_none()
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
