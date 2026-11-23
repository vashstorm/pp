//! Hub 观察线程：把 registry 读取与事件分页从 UI 主循环和 supervisor 中隔离。
//!
//! 线程只持有 [`Binding`] 的快照与游标，不做任何任务控制以外的写入。请求与
//! 回复都带根 epoch；详情页另带 view epoch，旧根/旧详情的迟到回复由 UI 丢弃。

use pp_runtime::subagent::Binding;
use pp_runtime::subagent::EventCursor;
use pp_runtime::subagent::EventPage;
use pp_runtime::subagent::HubSnapshot;
use pp_runtime::subagent::OverviewPage;
use pp_runtime::subagent::Run;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError},
    thread,
    time::{Duration, Instant},
};

/// Hub 可见时的修订轮询间隔。
const POLL: Duration = Duration::from_millis(250);
/// 回复拥塞时短暂让出 CPU，期间仍可立即接收控制请求。
const REPLY_RETRY: Duration = Duration::from_millis(5);
/// 快照合并窗口：流式事件只按此频率重建类型化快照。
const COALESCE: Duration = Duration::from_millis(200);
/// 总览批量扫描的每轮时间预算，之后回到请求队列检查控制或退让通道。
const OVERVIEW_BATCH_BUDGET: Duration = Duration::from_millis(25);
/// 回复通道容量；满时保留未发送页与游标，绝不丢有序事件。
const REPLY_CAPACITY: usize = 4;

/// 固定的控制动作；worker 内部再调用既有 `Binding::control`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tui) enum Action {
    Send,
    Cancel,
    Kill,
    Revive,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Cancel => "cancel",
            Self::Kill => "kill",
            Self::Revive => "revive",
        }
    }
}

pub(in crate::tui) enum Request {
    Bind {
        epoch: u64,
        binding: Binding,
        visible: bool,
    },
    SetVisible {
        epoch: u64,
        visible: bool,
    },
    Unbind {
        epoch: u64,
    },
    OpenDetail {
        epoch: u64,
        view: u64,
        agent: String,
    },
    CloseDetail {
        epoch: u64,
        view: u64,
    },
    Control {
        epoch: u64,
        request: u64,
        action: Action,
        agent: String,
        text: Option<String>,
    },
    Refresh {
        epoch: u64,
    },
    Shutdown,
}

pub(in crate::tui) enum Reply {
    Snapshot {
        epoch: u64,
        snapshot: Box<HubSnapshot>,
    },
    /// 无详情索引的 Activity 尾部（单页测试与兼容入口）。
    #[cfg_attr(not(test), allow(dead_code))]
    Activity {
        epoch: u64,
        agent: String,
        page: EventPage,
    },
    /// 总览批量聚合后的全量／增量页面。
    ActivityBatch {
        epoch: u64,
        pages: Vec<(String, OverviewPage)>,
    },
    /// 详情从头导入的完整历史。
    Detail {
        epoch: u64,
        view: u64,
        agent: String,
        history: DetailHistory,
    },
    /// 该 agent 的旧投影与锚点已失效，需要只重建这一路。
    Reset { epoch: u64, agent: String },
    Control {
        epoch: u64,
        request: u64,
        agent: String,
        result: Result<(), String>,
    },
    Error {
        epoch: u64,
        view: Option<u64>,
        message: String,
    },
}

/// 详情正文与元数据共用一次读页边界；快照补齐只能使用读页前已完成的 Run。
pub(in crate::tui) struct DetailHistory {
    pub page: EventPage,
    pub runs: Vec<Run>,
    pub settled: BTreeSet<String>,
}

pub(in crate::tui) struct Worker {
    pub(in crate::tui) requests: Sender<Request>,
    pub(in crate::tui) replies: Receiver<Reply>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    /// 首次打开 Hub 或角色定义页时启动；不阻塞 UI。
    pub(in crate::tui) fn start() -> Self {
        let (requests, receiver) = mpsc::channel();
        let (sender, replies) = mpsc::sync_channel(REPLY_CAPACITY);
        let thread = thread::Builder::new()
            .name("pp-agent-hub".into())
            .spawn(move || run(receiver, sender))
            .ok();
        Self {
            requests,
            replies,
            thread,
        }
    }

    pub(in crate::tui) fn send(&self, request: Request) {
        // 线程已退出时静默丢弃：UI 会在下一次回复超时/断链时显示 notice。
        let _ = self.requests.send(request);
    }

    pub(in crate::tui) fn shutdown(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Detail {
    view: u64,
    agent: String,
    cursor: Option<EventCursor>,
    more: bool,
}

struct State {
    epoch: u64,
    binding: Binding,
    visible: bool,
    revision: u64,
    force: bool,
    pending_snapshot: bool,
    last_snapshot: Instant,
    order: Vec<String>,
    /// 每个 agent 的 Activity 游标。
    cursors: BTreeMap<String, Option<EventCursor>>,
    /// 该 agent 还有未读完整页。
    more: BTreeMap<String, bool>,
    /// 已完成首次历史扫描到 EOF 的 agent；首次扫完前暂存分段而不把半成品推给 UI。
    primed: BTreeSet<String>,
    staged: BTreeMap<String, OverviewPage>,
    turn: usize,
    detail: Option<Detail>,
    pending: VecDeque<Reply>,
    snapshot: Option<Reply>,
}

impl State {
    fn new(epoch: u64, binding: Binding, visible: bool) -> Self {
        Self {
            epoch,
            binding,
            visible,
            revision: 0,
            force: true,
            pending_snapshot: true,
            last_snapshot: Instant::now() - Duration::from_secs(1),
            order: Vec::new(),
            cursors: BTreeMap::new(),
            more: BTreeMap::new(),
            primed: BTreeSet::new(),
            staged: BTreeMap::new(),
            turn: 0,
            detail: None,
            pending: VecDeque::new(),
            snapshot: None,
        }
    }

    fn request_wait(&self) -> Option<Duration> {
        if self.has_unsent_replies() {
            Some(REPLY_RETRY)
        } else if self.force
            || self.more.values().any(|more| *more)
            || self.detail.as_ref().is_some_and(|detail| detail.more)
        {
            Some(Duration::ZERO)
        } else if self.pending_snapshot {
            Some(COALESCE.saturating_sub(self.last_snapshot.elapsed()))
        } else if self.visible {
            Some(POLL)
        } else {
            None
        }
    }

    fn has_unsent_replies(&self) -> bool {
        !self.pending.is_empty() || self.snapshot.is_some()
    }

    fn push(&mut self, reply: Reply) {
        self.pending.push_back(reply);
    }

    fn flush(&mut self, replies: &SyncSender<Reply>) {
        if let Some(snapshot) = self.snapshot.take() {
            match replies.try_send(snapshot) {
                Ok(()) => {}
                Err(TrySendError::Full(reply)) => {
                    self.snapshot = Some(reply);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        while let Some(reply) = self.pending.pop_front() {
            match replies.try_send(reply) {
                Ok(()) => {}
                Err(TrySendError::Full(reply)) => {
                    self.pending.push_front(reply);
                    return;
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }

    fn refresh(&mut self) {
        let current = self.binding.hub_revision();
        if current != self.revision {
            self.revision = current;
            self.pending_snapshot = true;
            for more in self.more.values_mut() {
                *more = true;
            }
            if let Some(detail) = &mut self.detail {
                detail.more = true;
            }
        }
        if !self.pending_snapshot {
            return;
        }
        if !self.force && self.last_snapshot.elapsed() < COALESCE {
            return;
        }
        self.force = false;
        self.pending_snapshot = false;
        self.last_snapshot = Instant::now();
        let snapshot = self.binding.hub_snapshot();
        for agent in &snapshot.agents {
            if !self.order.contains(&agent.record.id) {
                self.order.push(agent.record.id.clone());
                self.cursors.entry(agent.record.id.clone()).or_insert(None);
                self.more.insert(agent.record.id.clone(), true);
            }
        }
        self.snapshot = Some(Reply::Snapshot {
            epoch: self.epoch,
            snapshot: Box::new(snapshot),
        });
    }

    fn step(&mut self) {
        self.refresh();
        // 当前详情优先（每轮至多读 1 页以保证控制响应）。
        let detail = self
            .detail
            .as_ref()
            .filter(|detail| detail.more)
            .map(|detail| (detail.agent.clone(), detail.cursor.clone(), detail.view));
        if let Some((agent, cursor, view)) = detail {
            match self.read_detail(&agent, cursor.as_ref()) {
                Ok(history) => {
                    let page = &history.page;
                    let more = page.has_more;
                    let reset = page.reset;
                    if let Some(detail) = &mut self.detail {
                        detail.cursor = page.cursor.clone();
                        detail.more = more;
                    }
                    if reset {
                        self.push(Reply::Reset {
                            epoch: self.epoch,
                            agent: agent.clone(),
                        });
                    }
                    self.push(Reply::Detail {
                        epoch: self.epoch,
                        view,
                        agent,
                        history,
                    });
                }
                Err(error) => {
                    if let Some(detail) = &mut self.detail {
                        detail.more = false;
                    }
                    self.push(Reply::Error {
                        epoch: self.epoch,
                        view: Some(view),
                        message: format!("{error:#}"),
                    });
                }
            }
        }
        let started = Instant::now();
        let mut ready_batch: Vec<(String, OverviewPage)> = Vec::new();
        while !self.order.is_empty() {
            let len = self.order.len();
            let mut picked = None;
            for offset in 0..len {
                let index = (self.turn + offset) % len;
                let agent = self.order[index].clone();
                if self.more.get(&agent).copied().unwrap_or(false) {
                    picked = Some((index, agent));
                    break;
                }
            }
            let Some((index, agent)) = picked else {
                break;
            };
            let cursor = self.cursors.get(&agent).cloned().flatten();
            match self.binding.read_overview_events(&agent, cursor.as_ref()) {
                Ok(page) => {
                    let reset = page.reset;
                    let has_more = page.has_more;
                    self.cursors.insert(agent.clone(), page.cursor.clone());
                    self.more.insert(agent.clone(), has_more);
                    if !has_more {
                        self.turn = (index + 1) % len;
                    }
                    if reset {
                        if !ready_batch.is_empty() {
                            self.push(Reply::ActivityBatch {
                                epoch: self.epoch,
                                pages: std::mem::take(&mut ready_batch),
                            });
                        }
                        self.staged.remove(&agent);
                        self.primed.remove(&agent);
                        self.push(Reply::Reset {
                            epoch: self.epoch,
                            agent: agent.clone(),
                        });
                    }
                    if !self.primed.contains(&agent) {
                        if has_more {
                            merge_overview(self.staged.entry(agent).or_default(), page);
                        } else {
                            self.primed.insert(agent.clone());
                            let final_page = match self.staged.remove(&agent) {
                                Some(mut staged) => {
                                    merge_overview(&mut staged, page);
                                    staged
                                }
                                None => page,
                            };
                            ready_batch.push((agent, final_page));
                        }
                    } else if reset
                        || !page.entries.is_empty()
                        || page.last_activity_ms.is_some()
                        || !page.warnings.is_empty()
                    {
                        ready_batch.push((agent, page));
                    }
                }
                Err(error) => {
                    self.more.insert(agent, false);
                    self.turn = (index + 1) % len;
                    self.push(Reply::Error {
                        epoch: self.epoch,
                        view: None,
                        message: format!("{error:#}"),
                    });
                }
            }
            if started.elapsed() >= OVERVIEW_BATCH_BUDGET {
                break;
            }
        }
        if !ready_batch.is_empty() {
            self.push(Reply::ActivityBatch {
                epoch: self.epoch,
                pages: ready_batch,
            });
        }
    }

    fn read(&self, agent: &str, cursor: Option<&EventCursor>) -> anyhow::Result<EventPage> {
        self.binding.read_events(agent, cursor)
    }

    fn read_detail(
        &self,
        agent: &str,
        cursor: Option<&EventCursor>,
    ) -> anyhow::Result<DetailHistory> {
        let mut runs = self.binding.hub_snapshot().runs;
        runs.retain(|run| run.agent_id == agent);
        let settled = runs
            .iter()
            .filter(|run| !matches!(run.status.as_str(), "queued" | "running"))
            .map(|run| run.id.clone())
            .collect();
        let page = self.read(agent, cursor)?;
        // 读页期间可能刚登记新 Run；只补它的元数据，不扩展此前冻结的终态集合。
        if page
            .entries
            .iter()
            .filter_map(|entry| entry.record.run_id.as_ref())
            .any(|id| !runs.iter().any(|run| run.id == *id))
        {
            for run in self.binding.hub_snapshot().runs {
                if run.agent_id == agent && !runs.iter().any(|known| known.id == run.id) {
                    runs.push(run);
                }
            }
        }
        Ok(DetailHistory {
            page,
            runs,
            settled,
        })
    }
}

fn merge_overview(target: &mut OverviewPage, page: OverviewPage) {
    target.entries.extend(page.entries);
    if let Some(ts) = page.last_activity_ms {
        target.last_activity_ms = Some(target.last_activity_ms.map_or(ts, |old| old.max(ts)));
    }
    target.cursor = page.cursor;
    target.reset |= page.reset;
    target.has_more = page.has_more;
    target.warnings.extend(page.warnings);
}

fn run(requests: Receiver<Request>, replies: SyncSender<Reply>) {
    let mut state: Option<State> = None;
    loop {
        let wait = state.as_ref().and_then(State::request_wait);
        let request = match wait {
            Some(wait) if wait.is_zero() => match requests.try_recv() {
                Ok(request) => Some(request),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => break,
            },
            Some(wait) => match requests.recv_timeout(wait) {
                Ok(request) => Some(request),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
            None => match requests.recv() {
                Ok(request) => Some(request),
                Err(_) => break,
            },
        };
        match request {
            Some(Request::Bind {
                epoch,
                binding,
                visible,
            }) => {
                state = Some(State::new(epoch, binding, visible));
            }
            Some(Request::SetVisible { epoch, visible }) => {
                if let Some(state) = &mut state
                    && state.epoch == epoch
                {
                    state.visible = visible;
                }
            }
            Some(Request::Unbind { epoch }) => {
                if state.as_ref().is_some_and(|state| state.epoch == epoch) {
                    state = None;
                }
            }
            Some(Request::OpenDetail { epoch, view, agent }) => {
                if let Some(state) = &mut state
                    && state.epoch == epoch
                {
                    state.detail = Some(Detail {
                        view,
                        agent,
                        cursor: None,
                        more: true,
                    });
                }
            }
            Some(Request::CloseDetail { epoch, view }) => {
                if let Some(state) = &mut state
                    && state.epoch == epoch
                    && state
                        .detail
                        .as_ref()
                        .is_some_and(|detail| detail.view == view)
                {
                    state.detail = None;
                }
            }
            Some(Request::Control {
                epoch,
                request,
                action,
                agent,
                text,
            }) => {
                let reply = state
                    .as_ref()
                    .filter(|state| state.epoch == epoch)
                    .map(|state| {
                        let result = state
                            .binding
                            .control(&agent, action.as_str(), text.as_deref())
                            .map(|_| ())
                            .map_err(|error| format!("{error:#}"));
                        Reply::Control {
                            epoch,
                            request,
                            agent,
                            result,
                        }
                    });
                if let Some(reply) = reply
                    && let Some(state) = &mut state
                {
                    // 控制回复优先于分页，避免状态提示排队在历史之后。
                    state.pending.push_front(reply);
                    state.force = true;
                    state.pending_snapshot = true;
                }
            }
            Some(Request::Refresh { epoch }) => {
                if let Some(state) = &mut state
                    && state.epoch == epoch
                {
                    state.force = true;
                    state.pending_snapshot = true;
                    for more in state.more.values_mut() {
                        *more = true;
                    }
                    if let Some(detail) = &mut state.detail {
                        detail.more = true;
                    }
                }
            }
            Some(Request::Shutdown) => break,
            None => {}
        }
        if let Some(state) = &mut state {
            state.flush(&replies);
            // 通道满时保留本轮已读的页，等它们入队后再推进任一游标。
            if !state.has_unsent_replies() {
                state.step();
                state.flush(&replies);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn worker_batches_overview_until_eof_and_resumes_incremental_updates_on_visible() {
        let dir = tempfile::tempdir().unwrap();
        let registry = serde_json::json!({
            "version": 1,
            "agents": {
                "a1": {
                    "id": "a1",
                    "name": "worker-1",
                    "parent": "Main",
                    "generation": 1,
                    "status": "parked",
                    "spec": {
                        "definition": "fast",
                        "prompt": "",
                        "context": "",
                        "model": "fixture-model",
                        "thinking": "none",
                        "tools": ["read"],
                        "spawns": [],
                        "output_schema": null,
                        "cwd": ".",
                        "depth": 1,
                        "readonly": false,
                        "isolated": false
                    },
                    "session_file": null,
                    "updated_ms": 100
                }
            },
            "runs": {},
            "next_id": 1
        });
        std::fs::write(dir.path().join("registry.json"), registry.to_string()).unwrap();

        // 超过 OVERVIEW_PAGE_ENTRIES(2048)，强制分页读取，但对未 primed 历史只合并成一次交付。
        {
            let mut log = std::fs::File::create(dir.path().join("a1.events.jsonl")).unwrap();
            for index in 0..2200u64 {
                let line = serde_json::json!({
                    "agent_id": "a1",
                    "run_id": "r1",
                    "generation": 1,
                    "timestamp_ms": 1_000 + index,
                    "event": {
                        "tool_started": {
                            "id": format!("t{index}"),
                            "name": "read",
                            "arguments": "{}"
                        }
                    }
                });
                writeln!(log, "{line}").unwrap();
            }
        }

        let binding = Binding::fixture_root(dir.path()).unwrap();
        let worker = Worker::start();
        worker.send(Request::Bind {
            epoch: 1,
            binding,
            visible: false,
        });

        let mut snapshots = 0usize;
        let mut batches = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && batches.is_empty() {
            match worker.replies.recv_timeout(Duration::from_millis(20)) {
                Ok(Reply::Snapshot { .. }) => snapshots += 1,
                Ok(Reply::ActivityBatch { pages, .. }) => batches.push(pages),
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("worker stopped"),
            }
        }
        assert!(snapshots >= 1);
        assert_eq!(
            batches.len(),
            1,
            "首次历史必须合并为单次 ActivityBatch 交付"
        );
        assert_eq!(batches[0].len(), 1);
        assert!(!batches[0][0].1.has_more);
        assert_eq!(batches[0][0].1.entries.len(), 2200);

        // 追加一条后续记录（模拟 /resume 后继续对话），并在可见时触发增量读取。
        {
            let mut log = std::fs::OpenOptions::new()
                .append(true)
                .open(dir.path().join("a1.events.jsonl"))
                .unwrap();
            let line = serde_json::json!({
                "agent_id": "a1",
                "run_id": "r2",
                "generation": 1,
                "timestamp_ms": 5_000,
                "event": {
                    "tool_started": {
                        "id": "t_new",
                        "name": "bash",
                        "arguments": "{}"
                    }
                }
            });
            writeln!(log, "{line}").unwrap();
        }
        worker.send(Request::SetVisible {
            epoch: 1,
            visible: true,
        });
        worker.send(Request::Refresh { epoch: 1 });

        let mut incremental = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && incremental.is_empty() {
            match worker.replies.recv_timeout(Duration::from_millis(20)) {
                Ok(Reply::ActivityBatch { pages, .. }) => incremental.push(pages),
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("worker stopped"),
            }
        }
        assert_eq!(incremental.len(), 1);
        assert_eq!(incremental[0][0].1.entries.len(), 1);
        assert_eq!(incremental[0][0].1.last_activity_ms, Some(5_000));
    }
}
