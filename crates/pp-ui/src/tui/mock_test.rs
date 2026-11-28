//! Dev builds only: `/test-*` uses its own worker, event channel and cancellation token.
//!
//! That worker neither replaces nor depends on the normal Agent worker; Prepared/Progress/Completed
//! flow back over a separate bounded channel, and on exit we cancel, drop the receiver and finally join the sender thread.

use std::{
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use pp_bash::PipeStore;
use pp_config::config::ModelChoice;
use pp_core::{
    cancellation::CancellationToken,
    context::ContextUsage,
    interaction::{DialogQueue, SharedAskHandler},
    model::ThinkLevel,
};
use pp_mock_ai::{
    MockEndpoint, MockTest, MockTestEvent, MockTestReport, MockTestSnapshot, run_test,
};
use pp_runtime::subagent::Binding;

use super::{App, Level, State, WorkerEvent, dialog_guard, request_ask, todo};

/// Normal display state captured before a mock run starts; restored unchanged when it finishes.
pub(in crate::tui) struct MockUiSnapshot {
    state: State,
    current_model: Option<ModelChoice>,
    current_source: Option<pp_core::model::ModelSource>,
    override_selection: Option<pp_config::model_state::Selection>,
    choices: Vec<ModelChoice>,
    think_level: ThinkLevel,
    default_think_level: ThinkLevel,
    usage: Option<ContextUsage>,
    todo_panel: todo::Panel,
    subagent_control: Arc<Mutex<Option<Binding>>>,
    pipe_store: Option<PipeStore>,
}

impl MockUiSnapshot {
    pub(in crate::tui) fn subagent_binding(&self) -> Option<Binding> {
        self.subagent_control
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Sole owner of the scenario worker: cancellation token, event receiver and sender thread.
pub(in crate::tui) struct Worker {
    cancellation: CancellationToken,
    started: Instant,
    events: Option<Receiver<WorkerEvent>>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub(in crate::tui) fn start(test: MockTest, endpoint: MockEndpoint) -> Result<Self> {
        let cancellation = CancellationToken::new();
        let (sender, receiver) = mpsc::sync_channel(256);
        let token = cancellation.clone();
        let started = Instant::now();
        let dialog = Arc::new(DialogQueue::default());
        let sender_events = sender.clone();
        let handle = thread::Builder::new()
            .name("pp-mock-test".into())
            .spawn(move || {
                let ask_events = sender_events.clone();
                let ask: SharedAskHandler = Arc::new(move |request, cancellation| {
                    let _guard = dialog_guard(&dialog, cancellation)?;
                    request_ask(
                        &ask_events,
                        super::worker_bridge::DialogOrigin::Mock,
                        request,
                        cancellation,
                    )
                });
                let result = run_test(test, endpoint, Some(ask), &token, &mut |event| {
                    let event = match event {
                        MockTestEvent::Prepared(snapshot) => WorkerEvent::MockPrepared(snapshot),
                        MockTestEvent::Progress(event) => WorkerEvent::Progress(event),
                    };
                    let _ = sender_events.send(event);
                });
                let _ = sender.send(WorkerEvent::MockCompleted {
                    result,
                    elapsed: started.elapsed(),
                    finished_at: chrono::Local::now(),
                });
            })
            .context("starting mock test worker")?;
        Ok(Self {
            cancellation,
            started,
            events: Some(receiver),
            thread: Some(handle),
        })
    }

    pub(in crate::tui) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(in crate::tui) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    pub(in crate::tui) fn try_recv(&self) -> std::result::Result<WorkerEvent, TryRecvError> {
        match &self.events {
            Some(events) => events.try_recv(),
            None => Err(TryRecvError::Disconnected),
        }
    }

    pub(in crate::tui) fn cancel(&self) {
        self.cancellation.cancel();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Cancel and close the receiver first so the sender unblocks and exits even when it is stuck on a full channel;
        // then join, so leaving the TUI never deadlocks against the bounded channel.
        self.cancellation.cancel();
        drop(self.events.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl App {
    /// Start one isolated mock test; returns true on success, and failures only raise a local error.
    pub(in crate::tui) fn start_mock_test(&mut self, test: MockTest) -> bool {
        let endpoint = match pp_mock_ai::current_endpoint() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.notify(Level::Error, format!("{error:#}"));
                return false;
            }
        };
        // The normal display state is moved out entirely; until Prepared arrives the UI is in an empty neutral state.
        let snapshot = self.take_mock_snapshot();
        self.hub_bound = None;
        self.close_hub();
        self.close_pipes();
        let worker = match Worker::start(test, endpoint) {
            Ok(worker) => worker,
            Err(error) => {
                self.restore_mock_snapshot(snapshot);
                self.notify(Level::Error, format!("{error:#}"));
                return false;
            }
        };
        self.mock_saved = Some(snapshot);
        self.mock_worker = Some(worker);
        // The previous run's (or a real task's) retry notice must not leak into an isolated scenario.
        self.retry_finished();
        if self.token_speed.is_active() {
            self.token_speed.clear();
        }
        self.active_cancellation = self.mock_worker.as_ref().map(Worker::cancellation);
        self.state = State::RunningTools;
        self.escape_tap.reset();
        self.secondary_status = Some(format!("Running mock test ({}).", test.name()));
        self.remember_input();
        self.clear_input();
        self.show_welcome = false;
        self.history_browser = false;
        self.conversation.follow();
        self.conversation.begin_run();
        self.append("You", test.name());
        true
    }

    /// Prepared only swaps the display snapshot; it touches no session, workflow, model_control or normal channel.
    pub(in crate::tui) fn install_mock_snapshot(&mut self, snapshot: MockTestSnapshot) {
        self.current_model = Some(snapshot.current_model);
        self.choices = snapshot.choices;
        self.think_level = snapshot.think_level;
        self.default_think_level = snapshot.default_think_level;
        // A mock scenario has no manual override, so the source falls back to the configured default.
        self.current_source = Some(pp_core::model::ModelSource::Default);
        self.override_selection = None;
        self.usage = Some(snapshot.usage);
        // Each scenario is isolated; the previous scenario's TODO must not carry over.
        self.todo_panel = Default::default();
        self.subagent_control = snapshot.subagent_control;
        self.install_pipe_store(Some(snapshot.pipe_store));
        self.hub_sync_root();
    }

    /// Shared finish path for completion/cancel/failure: restore the normal display, close the mock panel and release the isolated worker.
    pub(in crate::tui) fn finish_mock_test(
        &mut self,
        result: Result<MockTestReport>,
        elapsed: Duration,
        finished_at: chrono::DateTime<chrono::Local>,
    ) {
        self.flush_stream();
        self.close_ask_on_exit();
        self.retry_finished();
        if self.token_speed.is_active() {
            self.token_speed.clear();
        }
        self.active_cancellation = None;
        self.secondary_status = None;
        self.subagent_autowake = false;
        // mock never drains the input queue; it was confirmed pending-free before starting.
        self.queue_paused = false;
        // A panel may be observing the mock registry; close it before restoring the binding so late replies are dropped by epoch.
        self.close_hub();
        self.hub_bound = None;
        self.close_pipes();
        drop(self.mock_worker.take());
        match self.mock_saved.take() {
            Some(snapshot) => self.restore_mock_snapshot(snapshot),
            None => {
                self.state = State::Ready;
                self.hub_sync_root();
            }
        }
        match result {
            Ok(report) => self.conversation.finish_run(
                &format!("Mock test complete ({} tool calls)", report.calls),
                elapsed,
                finished_at,
            ),
            Err(error)
                if !error
                    .chain()
                    .any(|cause| cause.is::<pp_core::cancellation::CleanupError>())
                    && error
                        .chain()
                        .any(|cause| cause.is::<pp_core::cancellation::Cancelled>()) =>
            {
                self.conversation.cancel();
                self.conversation
                    .finish_run("Cancelled", elapsed, finished_at);
            }
            Err(error) => {
                self.conversation.fail();
                self.conversation.finish_run("Failed", elapsed, finished_at);
                self.notify(Level::Error, format!("{error:#}"));
            }
        }
    }

    pub(in crate::tui) fn take_mock_snapshot(&mut self) -> MockUiSnapshot {
        MockUiSnapshot {
            state: self.state,
            current_model: self.current_model.take(),
            current_source: self.current_source.take(),
            override_selection: self.override_selection.take(),
            choices: std::mem::take(&mut self.choices),
            think_level: self.think_level,
            default_think_level: self.default_think_level,
            usage: self.usage.take(),
            todo_panel: std::mem::take(&mut self.todo_panel),
            subagent_control: std::mem::replace(
                &mut self.subagent_control,
                Arc::new(Mutex::new(None)),
            ),
            pipe_store: self.pipe_store.take(),
        }
    }

    fn restore_mock_snapshot(&mut self, snapshot: MockUiSnapshot) {
        self.current_model = snapshot.current_model;
        self.current_source = snapshot.current_source;
        self.override_selection = snapshot.override_selection;
        self.choices = snapshot.choices;
        self.think_level = snapshot.think_level;
        self.default_think_level = snapshot.default_think_level;
        self.usage = snapshot.usage;
        self.todo_panel = snapshot.todo_panel;
        self.subagent_control = snapshot.subagent_control;
        self.install_pipe_store(snapshot.pipe_store);
        self.state = snapshot.state;
        self.hub_sync_root();
    }
}
