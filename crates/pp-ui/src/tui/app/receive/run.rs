//! Run terminal events: shell, mock, task completion, compaction and reset.

use crate::tui::{
    App, CompactionOutcome, Level, RunOutcome, State, WorkerEvent, desktop_notifications::Kind,
};

impl App {
    /// Run event family; `MockPrepared`/`MockCompleted` exist only in dev/test builds.
    pub(super) fn on_run_event(&mut self, event: WorkerEvent) -> bool {
        let mut changed = false;
        match event {
            WorkerEvent::ShellCompleted(result) => {
                self.active_cancellation = None;
                self.secondary_status = None;
                self.state = State::Ready;
                if let Err(error) = result {
                    if error
                        .chain()
                        .any(|cause| cause.is::<pp_core::cancellation::CleanupError>())
                    {
                        self.commands = None;
                        self.events = None;
                        self.state = State::Unavailable;
                        self.unavailable =
                            Some("Execution unavailable: shell cleanup failed; restart pp.".into());
                    }
                    self.notify(Level::Error, format!("{error:#}"));
                }
                changed = true;
            }

            #[cfg(debug_assertions)]
            WorkerEvent::MockPrepared(snapshot) => {
                self.install_mock_snapshot(snapshot);
                changed = true;
            }
            #[cfg(debug_assertions)]
            WorkerEvent::MockCompleted {
                result,
                elapsed,
                finished_at,
            } => {
                self.finish_mock_test(result, elapsed, finished_at);
                changed = true;
            }
            #[cfg(debug_assertions)]
            WorkerEvent::WebSearchCompleted(result) => {
                self.finish_test_web_search(result);
                changed = true;
            }

            WorkerEvent::Completed {
                result,
                elapsed,
                finished_at,
            } => {
                if self.token_speed.is_active() {
                    self.token_speed.clear();
                }
                self.ask_reply = None;
                self.ask_panel = None;
                self.desktop_notifications.dialog_closed();
                self.active_cancellation = None;
                self.retry_finished();
                self.subagent_autowake = false;
                let succeeded = matches!(&result, Ok(RunOutcome::Completed));
                if !succeeded {
                    self.queue_paused = self.has_pending();
                    self.reopen_plan_after_discussion = false;
                }
                let awaiting_review = matches!(&result, Ok(RunOutcome::AwaitingPlanReview));
                let label = match result {
                    Ok(RunOutcome::Completed) => {
                        self.desktop_notifications.task_finished(Kind::Completed);
                        "Completed"
                    }
                    Ok(RunOutcome::AwaitingPlanReview) => {
                        self.desktop_notifications.task_finished(Kind::PlanReady);
                        "Plan ready"
                    }
                    Ok(RunOutcome::Cancelled) => {
                        self.conversation.cancel();
                        "Cancelled"
                    }
                    Err(error) => {
                        self.conversation.fail();
                        self.notify(Level::Error, format!("{error:#}"));
                        self.desktop_notifications.task_finished(Kind::Failed);
                        "Failed"
                    }
                };
                self.conversation.finish_run(label, elapsed, finished_at);
                self.state = State::Ready;
                if awaiting_review {
                    // The worker submits and review opens automatically; it is idle by then, so
                    // the disk read is safe.
                    self.request_plan_read();
                }
                changed = true;
            }

            WorkerEvent::Compacted(result) => {
                // Compaction is not a regular task: Transcript is not finished and no task
                // Completed is shown.
                self.active_cancellation = None;
                self.secondary_status = None;
                self.state = State::Ready;
                match result {
                    Ok(CompactionOutcome::Compacted { .. }) => {}
                    Ok(CompactionOutcome::Unchanged) => {
                        self.notify(Level::Normal, "Nothing to compact.".into());
                    }
                    Ok(CompactionOutcome::Cancelled) => {
                        self.notify(Level::Normal, "Context compaction cancelled.".into());
                    }
                    Err(error) => self.notify(Level::Error, format!("{error:#}")),
                }
                changed = true;
            }

            WorkerEvent::Reset => {
                self.task_status = self.task_status_view.snapshot();
                self.todo_panel = Default::default();
                self.close_ask_on_exit();
                self.retry_finished();
                self.history_browser = false;
                self.conversation = self.conversation.fresh();
                self.clear_input();

                self.show_welcome = true;
                self.state = State::Ready;
                self.session_id = None;
                // /new drops the plan binding eligibility; the old plan file is kept and the
                // panel no longer points at it.
                self.plan_panel = None;
                // A new snapshot follows immediately; drop the old session usage first so a
                // stale ratio is never shown briefly.
                self.usage = None;
                self.token_speed.clear();
                changed = true;
            }
            _ => {}
        }
        changed
    }
}
