//! 运行终态事件：shell、mock、任务完成、压缩与重置。

use crate::tui::{App, CompactionOutcome, Level, RunOutcome, State, WorkerEvent};

impl App {
    /// 运行事件族；`MockPrepared`／`MockCompleted` 只在 dev/test 构建存在。
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
                    Ok(RunOutcome::Completed) => "Completed",
                    Ok(RunOutcome::AwaitingPlanReview) => "Plan ready",
                    Ok(RunOutcome::Cancelled) => {
                        self.conversation.cancel();
                        "Cancelled"
                    }
                    Err(error) => {
                        self.conversation.fail();
                        self.notify(Level::Error, format!("{error:#}"));
                        "Failed"
                    }
                };
                self.conversation.finish_run(label, elapsed, finished_at);
                self.state = State::Ready;
                if awaiting_review {
                    // 模型提交后自动打开审阅；worker 此时已空闲，安全读取磁盘。
                    self.request_plan_read();
                }
                changed = true;
            }

            WorkerEvent::Compacted(result) => {
                // 压缩不是普通任务：不收尾 Transcript、不显示任务 Completed。
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
                // /new 清掉计划绑定资格；旧计划文件保留，面板不再指向它。
                self.plan_panel = None;
                // 新快照紧随其后；先丢弃旧会话用量，避免短暂显示旧占比。
                self.usage = None;
                self.token_speed.clear();
                changed = true;
            }
            _ => {}
        }
        changed
    }
}
