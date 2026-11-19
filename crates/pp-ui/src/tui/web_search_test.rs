//! 开发构建专用：`/test-web-search` 不经过大模型，直接以当前配置或指定引擎执行真实 `web_search`。

use std::{
    path::Path,
    sync::mpsc::{self, Receiver, TryRecvError},
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result};
use pp_core::cancellation::CancellationToken;

use crate::commands::WebSearchTestCommand;

use super::{App, Level, State, WorkerCommand, WorkerEvent};

pub(in crate::tui) fn run_queries(
    queries: &[String],
    call_id: &str,
    cancellation: &CancellationToken,
    mut run_one: impl FnMut(&str, &str) -> Result<()>,
) -> Result<()> {
    for (index, query) in queries.iter().enumerate() {
        if cancellation.is_cancelled() {
            break;
        }
        let id = if queries.len() == 1 {
            call_id.to_owned()
        } else {
            format!("{call_id}-{}", index + 1)
        };
        run_one(query, &id)?;
    }
    Ok(())
}

/// 无可用 `Agent`（`State::Unavailable`）时使用的单次后台搜索 worker。
pub(in crate::tui) struct Worker {
    cancellation: CancellationToken,
    events: Option<Receiver<WorkerEvent>>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub(in crate::tui) fn start(
        cwd: &Path,
        settings_path: Option<&Path>,
        command: WebSearchTestCommand,
        call_id: String,
    ) -> Result<Self> {
        let cancellation = CancellationToken::new();
        let (sender, receiver) = mpsc::sync_channel(16);
        let token = cancellation.clone();
        let cwd = cwd.to_path_buf();
        let settings_path = settings_path.map(Path::to_path_buf);
        let sender_events = sender.clone();
        let handle = thread::Builder::new()
            .name("pp-web-search-test".into())
            .spawn(move || {
                let result = run_queries(&command.queries, &call_id, &token, |query, id| {
                    pp_runtime::agent::run_web_search_standalone(
                        &cwd,
                        settings_path.as_deref(),
                        query,
                        command.provider,
                        id,
                        &token,
                        &mut |event| {
                            let _ = sender_events.send(WorkerEvent::Progress(event));
                        },
                    )
                });
                let _ = sender.send(WorkerEvent::WebSearchCompleted(result));
            })
            .context("starting web search diagnostic worker")?;
        Ok(Self {
            cancellation,
            events: Some(receiver),
            thread: Some(handle),
        })
    }

    pub(in crate::tui) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(in crate::tui) fn try_recv(&self) -> std::result::Result<WorkerEvent, TryRecvError> {
        match &self.events {
            Some(events) => events.try_recv(),
            None => Err(TryRecvError::Disconnected),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancellation.cancel();
        drop(self.events.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl App {
    pub(in crate::tui) fn submit_test_web_search(&mut self, command: WebSearchTestCommand) -> bool {
        if !matches!(self.state, State::Ready | State::Unavailable)
            || self.active_cancellation.is_some()
        {
            self.notify(
                Level::Error,
                "Finish or cancel the current task before using /test-web-search.".into(),
            );
            return false;
        }
        if self.has_pending() || self.resources.ready.is_some() {
            self.notify(
                Level::Error,
                "Resolve pending input before using /test-web-search.".into(),
            );
            return false;
        }
        let call_id = format!("test-web-search-{}", self.next_shell_id);
        let cancellation = CancellationToken::new();
        if self.commands.is_some() {
            if self.send_submission(
                WorkerCommand::TestWebSearch {
                    queries: command.queries,
                    provider: command.provider,
                    call_id,
                    cancellation: cancellation.clone(),
                },
                State::RunningWebSearch,
            ) {
                self.next_shell_id += 1;
                self.active_cancellation = Some(cancellation);
                self.clear_input();
                self.show_welcome = false;
                self.history_browser = false;
                self.conversation.follow();
                self.secondary_status = Some("Running web_search...".into());
            }
            return false;
        }
        match Worker::start(
            &self.history_cwd,
            self.settings_path.as_deref(),
            command,
            call_id,
        ) {
            Ok(worker) => {
                self.next_shell_id += 1;
                self.active_cancellation = Some(worker.cancellation());
                self.web_search_worker = Some(worker);
                self.state = State::RunningWebSearch;
                self.escape_tap.reset();
                self.remember_input();
                self.clear_input();
                self.show_welcome = false;
                self.history_browser = false;
                self.conversation.follow();
                self.secondary_status = Some("Running web_search...".into());
            }
            Err(error) => self.notify(Level::Error, format!("{error:#}")),
        }
        false
    }

    pub(in crate::tui) fn finish_test_web_search(&mut self, result: Result<()>) {
        self.active_cancellation = None;
        self.secondary_status = None;
        drop(self.web_search_worker.take());
        self.state = if self.commands.is_some() {
            State::Ready
        } else {
            State::Unavailable
        };
        if let Err(error) = result {
            if self.commands.is_some()
                && error
                    .chain()
                    .any(|cause| cause.is::<pp_core::cancellation::CleanupError>())
            {
                self.commands = None;
                self.events = None;
                self.state = State::Unavailable;
                self.unavailable =
                    Some("Execution unavailable: web_search cleanup failed; restart pp.".into());
            }
            self.notify(Level::Error, format!("{error:#}"));
        }
    }
}
