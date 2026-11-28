//! Hub observer thread: keeps registry reads and event paging out of the UI main loop and the supervisor.
//!
//! The thread only holds the [`Binding`] snapshot and cursors and performs no writes beyond task control. Requests and
//! replies both carry the root epoch; the detail page adds a view epoch, and late replies for an old root/detail are dropped by the UI.

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

/// Revision polling interval while the Hub is visible.
const POLL: Duration = Duration::from_millis(250);
/// Briefly yield the CPU when replies are congested; control requests can still be received immediately.
const REPLY_RETRY: Duration = Duration::from_millis(5);
/// Snapshot merge window: streaming events only rebuild the typed snapshot at this rate.
const COALESCE: Duration = Duration::from_millis(200);
/// Time budget per round of the batched overview scan, after which the thread returns to the request queue to check control or yield the channel.
const OVERVIEW_BATCH_BUDGET: Duration = Duration::from_millis(25);
/// Reply channel capacity; when full, unsent pages and cursors are kept and ordered events are never dropped.
const REPLY_CAPACITY: usize = 4;

/// Fixed control actions; the worker calls the existing `Binding::control` internally.
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
    /// Activity tail without a detail index (single-page tests and the compatibility entry point).
    #[cfg_attr(not(test), allow(dead_code))]
    Activity {
        epoch: u64,
        agent: String,
        page: EventPage,
    },
    /// Full or incremental page after the batched overview aggregation.
    ActivityBatch {
        epoch: u64,
        pages: Vec<(String, OverviewPage)>,
    },
    /// Full history imported for the detail from the beginning.
    Detail {
        epoch: u64,
        view: u64,
        agent: String,
        history: DetailHistory,
    },
    /// The old projection and anchor of that agent are stale and only that lane must be rebuilt.
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

/// The detail body and its metadata share one read-page boundary; snapshot completion may only use Runs finished before the read.
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
    /// Started on the first opening of the Hub or a role definition page; never blocks the UI.
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
        // Dropped silently once the thread has exited: the UI shows a notice on the next reply timeout/broken link.
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
    /// Time of the last forced re-read of the observation log; drives the 250ms expired-buffer flush.
    last_read: Instant,
    order: Vec<String>,
    /// Activity cursor per agent.
    cursors: BTreeMap<String, Option<EventCursor>>,
    /// Whether that agent still has an unread full page.
    more: BTreeMap<String, bool>,
    /// Agents whose first history scan has reached EOF; before that, segments are staged instead of pushing a half-finished result to the UI.
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
            last_read: Instant::now(),
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
        // The runtime keeps consecutive streaming fragments in memory and only flushes the expired part before a read;
        // the revision only changes when a fragment enters the buffer, so a visible Hub must re-read once per poll
        // interval, otherwise the last segment after the stream goes quiet is only visible on the next model event.
        if self.visible && self.last_read.elapsed() >= POLL {
            self.last_read = Instant::now();
            for more in self.more.values_mut() {
                *more = true;
            }
            if let Some(detail) = &mut self.detail {
                detail.more = true;
            }
        }
        // The current detail takes priority (at most one page per round so control stays responsive).
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
        // A new Run may have been registered while the page was being read; only its metadata is added, without extending the previously frozen terminal set.
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
                    // Control replies take priority over paging so status notices do not queue behind history.
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
            // When the channel is full, keep the pages read this round and do not advance any cursor until they are queued.
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

        // Beyond OVERVIEW_PAGE_ENTRIES(2048) paging is forced, but a history that was never primed is still merged into a single delivery.
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
            "the first history must be merged into a single ActivityBatch delivery"
        );
        assert_eq!(batches[0].len(), 1);
        assert!(!batches[0][0].1.has_more);
        assert_eq!(batches[0][0].1.entries.len(), 2200);

        // Append a later record (simulating continued conversation after /resume) and trigger an incremental read while visible.
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
