//! Hub 观察线程：把 registry 读取与事件分页从 UI 主循环和 supervisor 中隔离。
//!
//! 线程只持有 [`Binding`] 的快照与游标，不做任何任务控制以外的写入。请求与
//! 回复都带根 epoch；详情页另带 view epoch，旧根/旧详情的迟到回复由 UI 丢弃。

use crate::subagent::{Binding, Definition, EventCursor, EventPage, HubSnapshot, Run};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError},
    thread,
    time::{Duration, Instant},
};

/// Hub 可见时的修订轮询间隔。
const POLL: Duration = Duration::from_millis(250);
/// 快照合并窗口：流式事件只按此频率重建类型化快照。
const COALESCE: Duration = Duration::from_millis(200);
/// 每轮最多处理的分页数；之后必须回到请求队列检查控制。
const PAGES_PER_ROUND: usize = 2;
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
    Roles {
        epoch: u64,
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
    /// 无详情索引的 Activity 尾部。
    Activity {
        epoch: u64,
        agent: String,
        page: EventPage,
    },
    /// 详情从头导入的完整历史。
    Detail {
        epoch: u64,
        view: u64,
        agent: String,
        history: DetailHistory,
    },
    /// 该 agent 的旧投影与锚点已失效，需要只重建这一路。
    Reset {
        epoch: u64,
        agent: String,
    },
    Roles {
        epoch: u64,
        roles: Result<Vec<Definition>, String>,
    },
    Control {
        epoch: u64,
        request: u64,
        agent: String,
        result: Result<(), String>,
    },
    Error {
        epoch: u64,
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
    revision: u64,
    force: bool,
    pending_snapshot: bool,
    snapshot_taken: bool,
    last_snapshot: Instant,
    order: Vec<String>,
    /// 每个 agent 的 Activity 游标。
    cursors: BTreeMap<String, Option<EventCursor>>,
    /// 该 agent 还有未读完整页。
    more: BTreeMap<String, bool>,
    turn: usize,
    detail: Option<Detail>,
    pending: VecDeque<Reply>,
    snapshot: Option<Reply>,
}

impl State {
    fn new(epoch: u64, binding: Binding) -> Self {
        Self {
            epoch,
            binding,
            revision: 0,
            force: true,
            pending_snapshot: true,
            snapshot_taken: false,
            last_snapshot: Instant::now() - Duration::from_secs(1),
            order: Vec::new(),
            cursors: BTreeMap::new(),
            more: BTreeMap::new(),
            turn: 0,
            detail: None,
            pending: VecDeque::new(),
            snapshot: None,
        }
    }

    fn busy(&self) -> bool {
        !self.pending.is_empty()
            || self.snapshot.is_some()
            || self.force
            || self.pending_snapshot
            || self.more.values().any(|more| *more)
            || self.detail.as_ref().is_some_and(|detail| detail.more)
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
        self.snapshot_taken = true;
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
        let mut budget = PAGES_PER_ROUND;
        // 当前详情优先。
        let detail = self
            .detail
            .as_ref()
            .filter(|detail| detail.more && budget > 0)
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
                    budget -= 1;
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
                        message: format!("{error:#}"),
                    });
                }
            }
        }
        while budget > 0 && !self.order.is_empty() {
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
            self.turn = (index + 1) % len;
            let cursor = self.cursors.get(&agent).cloned().flatten();
            match self.read(&agent, cursor.as_ref()) {
                Ok(page) => {
                    let reset = page.reset;
                    self.cursors.insert(agent.clone(), page.cursor.clone());
                    self.more.insert(agent.clone(), page.has_more);
                    budget -= 1;
                    if reset {
                        self.push(Reply::Reset {
                            epoch: self.epoch,
                            agent: agent.clone(),
                        });
                    }
                    self.push(Reply::Activity {
                        epoch: self.epoch,
                        agent,
                        page,
                    });
                }
                Err(error) => {
                    self.more.insert(agent, false);
                    self.push(Reply::Error {
                        epoch: self.epoch,
                        message: format!("{error:#}"),
                    });
                }
            }
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

fn run(requests: Receiver<Request>, replies: SyncSender<Reply>) {
    let mut state: Option<State> = None;
    loop {
        let busy = state.as_ref().is_some_and(State::busy);
        let request = if busy {
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
            Some(Request::Bind { epoch, binding }) => {
                state = Some(State::new(epoch, binding));
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
            Some(Request::Roles { epoch }) => {
                let roles = state
                    .as_ref()
                    .filter(|state| state.epoch == epoch)
                    .map(|state| state.binding.roles().map_err(|error| format!("{error:#}")));
                if let Some(roles) = roles
                    && let Some(state) = &mut state
                {
                    state.push(Reply::Roles { epoch, roles });
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
            state.step();
            state.flush(&replies);
        }
    }
}
