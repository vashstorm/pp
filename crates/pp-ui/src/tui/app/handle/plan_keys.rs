//! Plan panel keys: the panel owns input while present and reports whether the key was
//! consumed.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::{App, PendingKind, State, editor, plan};

impl App {
    pub(super) fn handle_plan_keys(&mut self, key: KeyEvent) -> bool {
        let Some(panel) = &mut self.plan_panel else {
            return false;
        };
        self.escape_tap.reset();
        match panel.key(key) {
            plan::Action::None => {}
            plan::Action::Close => {
                // Esc only closes the panel: mode, body and the pending pause are kept.
                self.plan_panel = None;
                self.menu_dismissed = true;
                // A list read-only preview may have overwritten the progress cache; restore the
                // currently bound plan.
                if let Some(plan) = &self.workflow.plan {
                    self.plan_progress = pp_core::plan::task_progress(&plan.content);
                }
            }
            plan::Action::Approve(context) => self.approve_plan(context),
            plan::Action::Review => self.review_plan(),
            plan::Action::SaveAndLeave => self.save_plan_and_leave(),
            plan::Action::EditInput(key) => {
                if key.code == KeyCode::Char('j') && key.modifiers == KeyModifiers::CONTROL {
                    self.input.insert_newline();
                } else {
                    self.edit_input(key);
                }
                self.refresh_candidates(true);
            }
            plan::Action::DiscussInput => {
                let text = self.input.lines().join("\n");
                if !text.trim().is_empty() && matches!(self.state, State::Ready) {
                    let source = text.clone();
                    let text = panel.compose_discussion(text);
                    if self.resources.job.is_some() {
                        return true;
                    }
                    if self.prepare_resource_input(&source, &text, PendingKind::Steering) {
                        self.resources.plan_discussion = true;
                        return true;
                    }
                    if self.send_plan_discussion(text.clone()) {
                        self.input = editor();
                        self.append("You", &text);
                        self.remember_text(text);
                    }
                }
            }
        }
        true
    }
}
