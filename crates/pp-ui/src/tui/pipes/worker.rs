//! 管道阶段观察线程：每 250ms 检查一次 revision，只复制当前选中流。
//!
//! 线程只读取 `PipeStore` 的有界快照与保留正文，不做 shell/agent 控制。所有回复
//! 带根 epoch、view epoch 与 run/stream id；面板关闭后旧线程直接结束。

use super::{BodyUpdate, RunView};
use pp_runtime::tools::PipeStore;
use pp_runtime::tools::output::Normalizer;
use std::{
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError},
    thread,
    time::Duration,
};

/// 面板打开时的修订轮询间隔。
const POLL: Duration = Duration::from_millis(250);
/// 回复通道容量；阻塞发送保证正文更新不被静默丢弃。
const REPLY_CAPACITY: usize = 16;

pub(in crate::tui) enum Request {
    Bind {
        epoch: u64,
        view: u64,
        store: PipeStore,
        run: Option<String>,
    },
    SelectRun {
        epoch: u64,
        view: u64,
        run: String,
    },
    SelectStream {
        epoch: u64,
        view: u64,
        run: String,
        stream: u64,
    },
    /// `full` 为真时重建当前流的完整保留快照；否则只刷新元数据。
    Refresh {
        epoch: u64,
        view: u64,
        full: bool,
    },
    Shutdown,
}

pub(in crate::tui) enum Reply {
    Runs {
        epoch: u64,
        view: u64,
        runs: Vec<RunView>,
        selected_run: Option<String>,
        selected_stream: Option<u64>,
    },
    Body {
        epoch: u64,
        view: u64,
        update: BodyUpdate,
    },
    Error {
        epoch: u64,
        view: u64,
        message: String,
    },
}

pub(in crate::tui) struct Worker {
    pub(in crate::tui) requests: mpsc::Sender<Request>,
    pub(in crate::tui) replies: Receiver<Reply>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    pub(in crate::tui) fn start() -> Self {
        let (requests, receiver) = mpsc::channel();
        let (sender, replies) = mpsc::sync_channel(REPLY_CAPACITY);
        let thread = thread::Builder::new()
            .name("pp-pipe-streams".into())
            .spawn(move || run(receiver, sender))
            .ok();
        Self {
            requests,
            replies,
            thread,
        }
    }

    pub(in crate::tui) fn send(&self, request: Request) {
        let _ = self.requests.send(request);
    }

    pub(in crate::tui) fn shutdown(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
        if let Some(thread) = self.thread.take() {
            // 观察线程可能阻塞在容量有限的回复通道上；关闭时由本线程排空，
            // 不能只 join 而让发送端永久等待。
            while !thread.is_finished() {
                while self.replies.try_recv().is_ok() {}
                thread::sleep(Duration::from_millis(1));
            }
            let _ = thread.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct State {
    epoch: u64,
    view: u64,
    store: PipeStore,
    desired_run: Option<String>,
    requested_stream: Option<u64>,
    run: Option<String>,
    stream: Option<u64>,
    cursor: Option<u64>,
    last_revision: Option<u64>,
    signature: Vec<(String, u64)>,
    force_list: bool,
    force_body: bool,
    reported: Option<String>,
    normalizer: Normalizer,
    normalizer_key: Option<(String, u64)>,
}

impl State {
    fn new(epoch: u64, view: u64, store: PipeStore, desired_run: Option<String>) -> Self {
        Self {
            epoch,
            view,
            store,
            desired_run,
            requested_stream: None,
            run: None,
            stream: None,
            cursor: None,
            last_revision: None,
            signature: Vec::new(),
            force_list: true,
            force_body: true,
            reported: None,
            normalizer: Normalizer::default(),
            normalizer_key: None,
        }
    }

    fn busy(&self) -> bool {
        self.force_list || self.force_body
    }

    fn reset_body(&mut self) {
        self.cursor = None;
        self.last_revision = None;
        self.requested_stream = None;
        self.normalizer = Normalizer::default();
        self.normalizer_key = None;
    }

    /// 返回 false 表示回复通道已关闭，观察线程应退出。
    fn step(&mut self, replies: &SyncSender<Reply>) -> bool {
        // 请求的流 id 必须在 run 解析（可能 reset_body）之前取出，避免被清掉。
        let requested_stream = self.requested_stream.take();
        let views = views(&self.store);
        let signature = views
            .iter()
            .map(|run| (run.run_id.clone(), run.revision))
            .collect::<Vec<_>>();
        let mut reported_error = None;
        match &self.desired_run {
            Some(desired) => {
                if views.iter().any(|run| &run.run_id == desired) {
                    if self.run.as_deref() != Some(desired) {
                        self.run = Some(desired.clone());
                        self.stream = None;
                        self.reset_body();
                    }
                    self.reported = None;
                } else {
                    if self.reported.as_deref() != Some(desired) {
                        reported_error = Some(
                            "Temporary pipe output is unavailable (evicted or from another session)."
                                .to_owned(),
                        );
                        self.reported = Some(desired.clone());
                    }
                    self.run = None;
                    self.stream = None;
                }
            }
            None => {
                let exists = self
                    .run
                    .as_ref()
                    .is_some_and(|id| views.iter().any(|run| &run.run_id == id));
                if !exists {
                    self.run = views
                        .iter()
                        .find(|run| !run.streams.is_empty())
                        .or_else(|| views.first())
                        .map(|run| run.run_id.clone());
                    self.stream = None;
                    self.reset_body();
                    self.reported = None;
                }
            }
        }
        let previous = (self.run.clone(), self.stream);
        if let Some(run) = self.run.clone() {
            let streams = views
                .iter()
                .find(|view| view.run_id == run)
                .map(|view| view.streams.as_slice())
                .unwrap_or(&[]);
            let requested_stream =
                requested_stream.filter(|id| streams.iter().any(|stream| stream.id == *id));
            if let Some(id) = requested_stream {
                if self.stream != Some(id) {
                    self.stream = Some(id);
                    self.reset_body();
                }
            } else if !self
                .stream
                .is_some_and(|id| streams.iter().any(|stream| stream.id == id))
            {
                self.stream = streams.first().map(|stream| stream.id);
                self.reset_body();
            }
        } else {
            self.stream = None;
        }
        let selection_changed = (self.run.clone(), self.stream) != previous;
        if self.stream.is_none() {
            // 没有可观察的流时清掉强制正文位，避免观察线程空转。
            self.force_body = false;
        }
        let selected_stream = self
            .run
            .as_ref()
            .and_then(|run| {
                views
                    .iter()
                    .find(|view| &view.run_id == run)
                    .and_then(|view| {
                        view.streams
                            .iter()
                            .find(|stream| Some(stream.id) == self.stream)
                    })
            })
            .cloned();

        if signature != self.signature || self.force_list || selection_changed {
            self.signature = signature;
            self.force_list = false;
            let reply = Reply::Runs {
                epoch: self.epoch,
                view: self.view,
                runs: views,
                selected_run: self.run.clone(),
                selected_stream: self.stream,
            };
            if replies.send(reply).is_err() {
                return false;
            }
        }
        if let Some(message) = reported_error {
            let reply = Reply::Error {
                epoch: self.epoch,
                view: self.view,
                message,
            };
            if replies.send(reply).is_err() {
                return false;
            }
        }

        let (Some(run), Some(stream)) = (self.run.clone(), self.stream) else {
            return true;
        };
        let revision = self.store.stream_revision(&run, stream);
        if !self.force_body && revision == self.last_revision {
            return true;
        }
        let from = if self.force_body { None } else { self.cursor };
        let read = match self.store.read(&run, stream, from) {
            Ok(read) => read,
            Err(error) => {
                self.last_revision = revision;
                let reply = Reply::Error {
                    epoch: self.epoch,
                    view: self.view,
                    message: error.to_string(),
                };
                return replies.send(reply).is_ok();
            }
        };
        let same_stream = self
            .normalizer_key
            .as_ref()
            .is_some_and(|(known_run, known_stream)| known_run == &run && *known_stream == stream);
        let reset = self.force_body || from.is_none() || read.prefix_evicted || !same_stream;
        if reset || !same_stream {
            self.normalizer = Normalizer::default();
            self.normalizer_key = Some((run.clone(), stream));
        }
        let data = feed(&mut self.normalizer, &read.data, !read.open);
        let retained_start = selected_stream
            .as_ref()
            .map_or(read.start, |stream| stream.retained_start);
        let retained_end = read.end;
        let total_bytes = read.total_bytes;
        let open = read.open;
        let start = read.start;
        let end = read.end;
        let prefix_evicted = read.prefix_evicted;
        self.cursor = Some(end);
        self.last_revision = revision;
        self.force_body = false;
        replies
            .send(Reply::Body {
                epoch: self.epoch,
                view: self.view,
                update: BodyUpdate {
                    run,
                    stream,
                    data,
                    start,
                    end,
                    reset,
                    retained_start,
                    retained_end,
                    total_bytes,
                    open,
                    revision: revision.unwrap_or_default(),
                    prefix_evicted,
                },
            })
            .is_ok()
    }
}

/// 归一化一段正文；关闭流时先把本块喂入，再单独收尾，不能丢弃本块字节。
fn feed(normalizer: &mut Normalizer, bytes: &[u8], finish: bool) -> String {
    let mut text = normalizer.feed(bytes, false);
    if finish {
        text.push_str(&normalizer.feed(&[], true));
    }
    text
}

/// 把不可命名的 `RunInfo` 转成 UI 自己的有界视图。
fn views(store: &PipeStore) -> Vec<RunView> {
    store
        .list()
        .iter()
        .map(|run| RunView {
            run_id: run.run_id.clone(),
            owner: run.owner.clone(),
            state: format!("{:?}", run.state).to_ascii_lowercase(),
            exit_code: run.exit_code,
            revision: run.revision,
            pipelines: run
                .pipelines
                .iter()
                .map(|pipeline| super::PipelineView {
                    id: pipeline.id,
                    parent: pipeline.parent,
                    stage_count: pipeline.stage_count,
                    bang: pipeline.bang,
                    pipefail: pipeline.pipefail,
                    status: pipeline.status,
                    stages: pipeline
                        .stages
                        .iter()
                        .map(|stage| super::StageView {
                            command: stage.command.clone(),
                            state: format!("{:?}", stage.state).to_ascii_lowercase(),
                            status: stage.status,
                            signal: stage.signal,
                        })
                        .collect(),
                })
                .collect(),
            streams: run
                .streams
                .iter()
                .map(|stream| super::StreamView {
                    id: stream.id,
                    pipeline: stream.pipeline,
                    producer_stage: stream.producer_stage,
                    consumer_stage: stream.consumer_stage,
                    producer: stream.producer.clone(),
                    consumer: stream.consumer.clone(),
                    open: stream.open,
                    total_bytes: stream.total_bytes,
                    retained_start: stream.retained_start,
                    retained_end: stream.retained_end,
                    revision: stream.revision,
                    close_reason: stream.close_reason.clone(),
                })
                .collect(),
            nonzero_stages: run.nonzero_stages,
            omitted_pipelines: run.omitted_pipelines,
            omitted_stages: run.omitted_stages,
            untapped_edges: run.untapped_edges,
            evicted_streams: run.evicted_streams,
            incomplete: run.incomplete,
            capture_error: run.capture_error.clone(),
        })
        .collect()
}

fn run(requests: Receiver<Request>, replies: SyncSender<Reply>) {
    let mut state: Option<State> = None;
    loop {
        let request = if state.as_ref().is_some_and(State::busy) {
            match requests.try_recv() {
                Ok(request) => Some(request),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => break,
            }
        } else {
            match requests.recv_timeout(POLL) {
                Ok(request) => Some(request),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        };
        match request {
            Some(Request::Bind {
                epoch,
                view,
                store,
                run,
            }) => state = Some(State::new(epoch, view, store, run)),
            Some(Request::SelectRun { epoch, view, run }) => {
                if let Some(state) = &mut state
                    && state.epoch == epoch
                {
                    state.view = view;
                    state.desired_run = Some(run);
                    state.run = None;
                    state.stream = None;
                    state.reset_body();
                    state.requested_stream = None;
                    state.force_list = true;
                    state.force_body = true;
                    state.reported = None;
                }
            }
            Some(Request::SelectStream {
                epoch,
                view,
                run,
                stream,
            }) => {
                if let Some(state) = &mut state
                    && state.epoch == epoch
                {
                    state.view = view;
                    state.desired_run = Some(run);
                    state.run = None;
                    state.stream = None;
                    state.reset_body();
                    state.requested_stream = Some(stream);
                    state.force_list = true;
                    state.force_body = true;
                    state.reported = None;
                }
            }
            Some(Request::Refresh { epoch, view, full }) => {
                if let Some(state) = &mut state
                    && state.epoch == epoch
                {
                    state.view = view;
                    state.force_list = true;
                    state.force_body = full;
                    if full {
                        state.cursor = None;
                        state.normalizer = Normalizer::default();
                        state.normalizer_key = None;
                    }
                }
            }
            Some(Request::Shutdown) => break,
            None => {}
        }
        if let Some(state) = &mut state
            && !state.step(&replies)
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn closed_stream_body_is_not_dropped_when_finishing() {
        let mut normalizer = Normalizer::default();
        assert_eq!(feed(&mut normalizer, b"visible\n", true), "visible\n");
        assert_eq!(feed(&mut normalizer, b"hidden\n", false), "hidden\n");
    }

    #[test]
    fn empty_store_emits_one_bounded_run_list() {
        let (sender, replies) = sync_channel(8);
        let mut state = State::new(0, 0, PipeStore::new(), None);
        assert!(state.step(&sender));
        let Reply::Runs { runs, .. } = replies.try_recv().expect("first run list") else {
            panic!("expected Runs");
        };
        assert!(runs.is_empty());
        assert!(state.step(&sender));
        assert!(
            replies.try_recv().is_err(),
            "unchanged revision must not copy"
        );
    }

    #[test]
    fn default_selection_is_latest_run_with_streams_or_latest_run() {
        let store = PipeStore::new();
        let first = store.start_run("a".repeat(32), "root").expect("run");
        drop(first);
        let (sender, replies) = sync_channel(8);
        let mut state = State::new(0, 0, store, None);
        assert!(state.step(&sender));
        let Reply::Runs {
            runs,
            selected_run,
            selected_stream,
            ..
        } = replies.try_recv().expect("run list")
        else {
            panic!("expected Runs");
        };
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].state, "running");
        let expected = "a".repeat(32);
        assert_eq!(selected_run.as_deref(), Some(expected.as_str()));
        assert_eq!(selected_stream, None);
    }

    #[test]
    fn unknown_explicit_run_reports_unavailable_without_switching() {
        let store = PipeStore::new();
        drop(store.start_run("a".repeat(32), "root").expect("run"));
        let (sender, replies) = sync_channel(8);
        let mut state = State::new(0, 0, store, Some("b".repeat(32)));
        assert!(state.step(&sender));
        let Reply::Runs {
            selected_run,
            selected_stream,
            ..
        } = replies.try_recv().expect("run list")
        else {
            panic!("expected Runs");
        };
        assert_eq!(selected_run, None);
        assert_eq!(selected_stream, None);
        let Reply::Error { message, .. } = replies.try_recv().expect("unavailable error") else {
            panic!("expected Error");
        };
        assert!(message.contains("unavailable"));
        assert!(state.step(&sender));
        assert!(replies.try_recv().is_err(), "error must not repeat");
    }
}
