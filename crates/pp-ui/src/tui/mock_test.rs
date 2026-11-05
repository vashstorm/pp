//! 开发构建专用：`/test-*` 使用独立 worker、事件通道与取消令牌。
//!
//! 该 worker 不替换、也不依赖正常 Agent worker；Prepared/Progress/Completed
//! 通过单独的有界通道回流，退出时先取消再丢弃接收端，最后 join 发送线程。

use std::{
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use pp_config::config::ModelChoice;
use pp_core::{
    cancellation::CancellationToken,
    context::ContextUsage,
    interaction::{DialogQueue, SharedAskHandler},
    model::ThinkLevel,
};
use pp_runtime::{
    mock_ai::{MockEndpoint, MockTest, MockTestEvent, MockTestReport, MockTestSnapshot, run_test},
    subagent::Binding,
    tools::PipeStore,
};

use super::{App, Level, State, WorkerEvent, dialog_guard, request_ask, todo};

/// 开始 mock 运行前的正常展示状态；完成后原样恢复。
pub(in crate::tui) struct MockUiSnapshot {
    state: State,
    current_model: Option<ModelChoice>,
    choices: Vec<ModelChoice>,
    think_level: ThinkLevel,
    default_think_level: ThinkLevel,
    usage: Option<ContextUsage>,
    todo_panel: todo::Panel,
    subagent_control: Arc<Mutex<Option<Binding>>>,
    pipe_store: Option<PipeStore>,
}

/// 场景 worker 的唯一 owner：取消令牌、事件接收端与发送线程。
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
                    request_ask(&ask_events, request, cancellation)
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
        // 先取消并关闭接收端：发送线程即使卡在满通道上也能解除阻塞并退出；
        // 之后再 join，避免退出 TUI 时与有界通道互相等待。
        self.cancellation.cancel();
        drop(self.events.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl App {
    /// 启动一场独立 mock 测试；成功时返回 true，失败只发本地错误。
    pub(in crate::tui) fn start_mock_test(&mut self, test: MockTest) -> bool {
        let endpoint = match pp_runtime::mock_ai::current_endpoint() {
            Ok(endpoint) => endpoint,
            Err(error) => {
                self.notify(Level::Error, format!("{error:#}"));
                return false;
            }
        };
        // 正常展示状态整体移出；Prepared 到达前界面处于空的中性状态。
        let snapshot = MockUiSnapshot {
            state: self.state,
            current_model: self.current_model.take(),
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
        };
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

    /// Prepared 只替换展示快照；不碰 session、workflow、model_control 或正常通道。
    pub(in crate::tui) fn install_mock_snapshot(&mut self, snapshot: MockTestSnapshot) {
        self.current_model = Some(snapshot.current_model);
        self.choices = snapshot.choices;
        self.think_level = snapshot.think_level;
        self.default_think_level = snapshot.default_think_level;
        self.usage = Some(snapshot.usage);
        // 每场场景独立；上一场的 TODO 不能带进下一场。
        self.todo_panel = Default::default();
        self.subagent_control = snapshot.subagent_control;
        self.install_pipe_store(Some(snapshot.pipe_store));
        self.hub_sync_root();
    }

    /// 完成/取消/失败统一收尾：恢复正常展示、关闭 mock 面板并释放隔离 worker。
    pub(in crate::tui) fn finish_mock_test(
        &mut self,
        result: Result<MockTestReport>,
        elapsed: Duration,
        finished_at: chrono::DateTime<chrono::Local>,
    ) {
        self.flush_stream();
        self.close_ask_on_exit();
        if self.token_speed.is_active() {
            self.token_speed.clear();
        }
        self.active_cancellation = None;
        self.secondary_status = None;
        self.subagent_autowake = false;
        // mock 从不消费输入队列；开始前已确认无 pending。
        self.queue_paused = false;
        // 面板可能观察 mock registry；先关闭再恢复绑定，迟到回复按 epoch 丢弃。
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

    fn restore_mock_snapshot(&mut self, snapshot: MockUiSnapshot) {
        self.current_model = snapshot.current_model;
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
