//! 根会话共享的有界 run registry（`PipeStore`）。

use brush_core::openfiles::OpenFile;
use std::{
    collections::{BTreeMap, VecDeque},
    fmt, io,
    sync::{Arc, Mutex, Weak, atomic::Ordering},
};

use super::super::display::Trace;
use super::super::relay::{self, Relay};
use super::observer::{PipeRun, RunObserver};
use super::ring::{CloseReason, StreamEvent, StreamHook, lock};
use super::run::{RunInfo, RunState};
use super::stream::{StreamRead, StreamState};
use super::{
    MAX_COMPLETED_RUNS, MAX_EMPTY_RUNS, MAX_STREAMS_PER_RUN, MAX_TOTAL_BODY, MAX_TOTAL_STREAMS,
    PIPE_URI_PREFIX, PIPES_UNAVAILABLE,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeStoreError {
    /// 未知 run、未知流、已淘汰或来自另一会话；调用方应提示而非返回空正文。
    Unavailable,
    DuplicateRun,
}

impl PipeStoreError {
    pub const fn message(self) -> &'static str {
        match self {
            Self::Unavailable => PIPES_UNAVAILABLE,
            Self::DuplicateRun => "A pipe run with this id already exists in this session.",
        }
    }
}

impl fmt::Display for PipeStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for PipeStoreError {}

pub(crate) fn run_uri(run_id: &str) -> String {
    format!("{PIPE_URI_PREFIX}{run_id}")
}

pub(crate) fn stream_uri(run_id: &str, stream: u64) -> String {
    format!("{PIPE_URI_PREFIX}{run_id}/{stream}")
}

#[derive(Default)]
struct Registry {
    runs: BTreeMap<String, Arc<RunState>>,
    /// run 创建顺序；淘汰与“最近创建”选择都依赖它。
    order: Vec<String>,
    /// 已完成且仍有管道的 run，最老在前，最多 8 个。
    completed: VecDeque<String>,
    /// 已完成但没有管道的 run，最老在前，最多 8 个。
    empty: VecDeque<String>,
}

#[derive(Default)]
struct StoreInner {
    registry: Mutex<Registry>,
}

/// 根会话共享的临时管道 registry。clone 共享同一实例。
#[derive(Clone)]
pub struct PipeStore {
    inner: Arc<StoreInner>,
}

impl Default for PipeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PipeStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(StoreInner::default()),
        }
    }

    /// 根会话切换/事件 epoch 依据：只有同一实例才能继续沿用旧 URI。
    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// 为一个新 bash 调用创建 run。run ID 由调用方生成（16 字节小写 hex）。
    pub fn start_run(
        &self,
        run_id: impl Into<String>,
        owner: impl Into<String>,
    ) -> Result<PipeRun, PipeStoreError> {
        let run_id = run_id.into();
        let owner = owner.into();
        let mut registry = lock(&self.inner.registry);
        if registry.runs.contains_key(&run_id) {
            return Err(PipeStoreError::DuplicateRun);
        }
        let state = Arc::new(RunState::new(run_id.clone(), owner));
        registry.runs.insert(run_id.clone(), state.clone());
        registry.order.push(run_id);
        drop(registry);

        let observer = Arc::new(RunObserver {
            shell: Trace::new(),
            store: self.clone(),
            state: state.clone(),
        });
        Ok(PipeRun {
            store: self.clone(),
            state,
            observer,
        })
    }

    /// 最近创建的 run 在前；包含根与子 Agent 共用 store 的所有 run。
    pub fn list(&self) -> Vec<RunInfo> {
        let runs = {
            let registry = lock(&self.inner.registry);
            registry
                .order
                .iter()
                .rev()
                .filter_map(|id| registry.runs.get(id).cloned())
                .collect::<Vec<_>>()
        };
        runs.iter().map(|run| run.snapshot()).collect()
    }

    pub fn snapshot(&self, run_id: &str) -> Result<RunInfo, PipeStoreError> {
        Ok(self.run_state(run_id)?.snapshot())
    }

    pub fn stream_revision(&self, run_id: &str, stream: u64) -> Option<u64> {
        let state = self.run_state(run_id).ok()?;
        state.stream(stream).map(|stream| stream.revision())
    }

    /// 读取一条流的保留正文。请求起点被夹到当前保留区间，并显式报告前缀淘汰。
    pub fn read(
        &self,
        run_id: &str,
        stream_id: u64,
        from: Option<u64>,
    ) -> Result<StreamRead, PipeStoreError> {
        let state = self.run_state(run_id)?;
        let stream = state.stream(stream_id).ok_or(PipeStoreError::Unavailable)?;
        let slice = stream.read_snapshot(from);
        let close = stream.close_reason();
        Ok(StreamRead {
            start: slice.start,
            end: slice.end,
            data: slice.data,
            open: close.is_none(),
            revision: stream.revision(),
            total_bytes: slice.end,
            prefix_evicted: slice.prefix_evicted,
        })
    }

    fn run_state(&self, run_id: &str) -> Result<Arc<RunState>, PipeStoreError> {
        lock(&self.inner.registry)
            .runs
            .get(run_id)
            .cloned()
            .ok_or(PipeStoreError::Unavailable)
    }

    pub(super) fn capture_edge(
        &self,
        run: &Arc<RunState>,
        pipeline: u64,
        edge: usize,
        producer: &str,
        consumer: &str,
    ) -> io::Result<Option<(OpenFile, OpenFile)>> {
        if run.is_finished() {
            run.note_untapped();
            return Ok(None);
        }
        let stream_id = run.next_stream.fetch_add(1, Ordering::Relaxed) + 1;
        let stream = Arc::new(StreamState::new(
            stream_id,
            pipeline,
            edge,
            edge.saturating_add(1),
            producer,
            consumer,
            stream_hooks(run),
        ));
        let relay::Edge {
            consumer: downstream,
            producer: upstream,
            relay,
        } = match relay::spawn(stream.clone(), run.stop.clone()) {
            Ok(edge) => edge,
            Err(error) => {
                run.note_capture_error(format!("pipe relay: {error}"));
                return Err(error);
            }
        };
        match self.register_stream(run, stream.clone(), relay) {
            Ok(()) => {
                if run.is_finished() {
                    // 与 run 收尾竞争：收尾已取走 relay 列表，这条边不能再留下未封存的流。
                    drop((downstream, upstream));
                    stream.finish(CloseReason::Interrupted);
                    return Ok(None);
                }
                Ok(Some((downstream, upstream)))
            }
            Err(mut relay) => {
                // 配额拒绝：先释放上游写端让中继见到 EOF，再 join，句柄不残留。
                drop(upstream);
                drop(downstream);
                relay.join();
                run.note_untapped();
                Ok(None)
            }
        }
    }

    fn register_stream(
        &self,
        run: &Arc<RunState>,
        stream: Arc<StreamState>,
        relay: Relay,
    ) -> Result<(), Relay> {
        // 在取 registry 锁之前回收已完成的中继：长 run 的句柄数量只与并发 open
        // 流相关，不随历史边数增长；join 也不会阻塞 registry/UI 锁。
        run.reap_finished_relays();
        let mut registry = lock(&self.inner.registry);
        while registry.completed.len() > MAX_COMPLETED_RUNS {
            let Some(oldest) = registry.completed.pop_front() else {
                break;
            };
            registry.runs.remove(&oldest);
            registry.order.retain(|candidate| *candidate != oldest);
        }
        if run.stream_count() >= MAX_STREAMS_PER_RUN && !evict_closed_in_run(run) {
            return Err(relay);
        }
        let mut guard = 0usize;
        while total_streams(&registry) >= MAX_TOTAL_STREAMS
            || total_body(&registry) >= MAX_TOTAL_BODY
        {
            guard += 1;
            if guard > MAX_TOTAL_STREAMS || !evict_closed_globally(&mut registry) {
                return Err(relay);
            }
        }
        run.push_stream(stream);
        run.push_relay(relay);
        run.bump();
        Ok(())
    }

    /// run 收尾：无管道的 run 单独限界保留（可查询终态但不挤占管道历史）；
    /// 有管道的完成 run 超过 8 个时淘汰最老的整条。
    pub(super) fn finalize_run(&self, run: &Arc<RunState>) {
        let mut registry = lock(&self.inner.registry);
        if run.pipeline_count() == 0 && run.stream_count() == 0 {
            if !registry.empty.iter().any(|id| *id == run.id) {
                registry.empty.push_back(run.id.clone());
            }
            while registry.empty.len() > MAX_EMPTY_RUNS {
                let Some(oldest) = registry.empty.pop_front() else {
                    break;
                };
                registry.runs.remove(&oldest);
                registry.order.retain(|candidate| *candidate != oldest);
            }
            return;
        }
        // 有管道的 run 若曾以空 run 记录过（不应发生），保证不重复登记。
        registry.empty.retain(|id| *id != run.id);
        if !registry.completed.iter().any(|id| *id == run.id) {
            registry.completed.push_back(run.id.clone());
        }
        while registry.completed.len() > MAX_COMPLETED_RUNS {
            let Some(oldest) = registry.completed.pop_front() else {
                break;
            };
            registry.runs.remove(&oldest);
            registry.order.retain(|candidate| *candidate != oldest);
        }
    }
}

fn stream_hooks(run: &Arc<RunState>) -> StreamHook {
    let weak: Weak<RunState> = Arc::downgrade(run);
    Arc::new(move |event: StreamEvent| {
        let Some(run) = weak.upgrade() else {
            return;
        };
        match event {
            StreamEvent::Data => run.bump(),
            StreamEvent::Closed => run.stream_closed(),
        }
    })
}

fn total_streams(registry: &Registry) -> usize {
    registry
        .runs
        .values()
        .map(|run| run.stream_count())
        .sum::<usize>()
}

fn total_body(registry: &Registry) -> usize {
    let mut total = 0usize;
    for run in registry.runs.values() {
        let streams = lock(&run.streams).clone();
        for stream in streams {
            let (start, end) = stream.retained_range();
            total = total.saturating_add((end - start) as usize);
        }
    }
    total
}

fn evict_closed_in_run(run: &Arc<RunState>) -> bool {
    let victim = {
        let mut streams = lock(&run.streams);
        let Some(index) = streams.iter().position(|stream| !stream.is_open()) else {
            return false;
        };
        streams.remove(index)
    };
    drop(victim);
    lock(&run.meta).evicted_streams += 1;
    run.bump();
    true
}

/// 优先淘汰最老已完成 run 的已关闭流，其次按创建顺序；open 流永远不参与。
fn evict_closed_globally(registry: &mut Registry) -> bool {
    let mut candidates: Vec<String> = registry.completed.iter().cloned().collect();
    candidates.extend(
        registry
            .order
            .iter()
            .filter(|id| !registry.completed.contains(*id))
            .cloned(),
    );
    for id in candidates {
        let Some(run) = registry.runs.get(&id).cloned() else {
            continue;
        };
        if evict_closed_in_run(&run) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::super::MAX_CLOSED_STREAMS_PER_RUN;
    use super::super::MAX_COMPLETED_RUNS;
    use super::super::MAX_EMPTY_RUNS;
    use super::super::STREAM_RING_CAP;
    use super::super::fixtures::{edge, producer_writer, unique_run, wait_for};
    use super::super::run::{RunEnd, RunStateKind};
    use super::*;
    use std::io::Write;

    #[test]
    fn pipeline_closed_streams_keep_only_four_per_run() {
        let store = PipeStore::new();
        let run = unique_run(&store);
        for index in 1..=6u64 {
            let pipeline = run.observer().begin(None, 2, false, false).unwrap();
            let (consumer, producer) = edge(&run, pipeline, 1);
            let mut writer = producer_writer(producer);
            writer.write_all(b"stage body\n").expect("write edge");
            drop(writer);
            assert!(
                wait_for(|| store
                    .read(run.id(), index, None)
                    .is_ok_and(|read| !read.open && !read.data.is_empty())),
                "stream {index} must close with mirrored bytes"
            );
            drop(consumer);
        }
        let info = run.snapshot();
        assert_eq!(info.streams.len(), MAX_CLOSED_STREAMS_PER_RUN);
        assert_eq!(info.evicted_streams, 2);
        assert_eq!(
            info.streams
                .iter()
                .map(|stream| stream.id)
                .collect::<Vec<_>>(),
            vec![3, 4, 5, 6]
        );
        // 淘汰后的旧 URI 永远不会读到别的流。
        assert_eq!(
            store.read(run.id(), 1, None).unwrap_err(),
            PipeStoreError::Unavailable
        );
        run.finish(RunEnd::exited(Some(0)));
    }

    #[test]
    fn pipeline_completed_runs_keep_only_eight() {
        let store = PipeStore::new();
        let mut runs = Vec::new();
        for _ in 0..(MAX_COMPLETED_RUNS + 2) {
            let run = unique_run(&store);
            let pipeline = run.observer().begin(None, 2, false, false).unwrap();
            let (consumer, producer) = edge(&run, pipeline, 1);
            drop((consumer, producer));
            run.finish(RunEnd::exited(Some(0)));
            runs.push(run);
        }
        assert_eq!(store.list().len(), MAX_COMPLETED_RUNS);
        assert_eq!(
            store.read(runs[0].id(), 1, None).unwrap_err(),
            PipeStoreError::Unavailable
        );
        assert!(
            store
                .read(runs[MAX_COMPLETED_RUNS + 1].id(), 1, None)
                .is_ok()
        );
        // 无管道的 run 单独限界保留终态，不挤占 8 个有管道的完成 run。
        let mut empties = Vec::new();
        for _ in 0..(MAX_EMPTY_RUNS + 4) {
            let run = unique_run(&store);
            run.finish(RunEnd::interrupted("setup_failed"));
            empties.push(run);
        }
        assert_eq!(
            store.list().len(),
            MAX_COMPLETED_RUNS + MAX_EMPTY_RUNS,
            "empty runs must be bounded separately from pipe history"
        );
        assert!(
            store.snapshot(empties[0].id()).is_err(),
            "oldest empty run must be evicted"
        );
        assert!(
            store
                .snapshot(empties[MAX_EMPTY_RUNS + 3].id())
                .is_ok_and(|info| info.state == RunStateKind::Interrupted && !info.incomplete)
        );
        // 有管道 run 的 8 条历史不受空 run 影响。
        assert!(store.read(runs[0].id(), 1, None).is_err());
        assert!(
            store
                .read(runs[MAX_COMPLETED_RUNS + 1].id(), 1, None)
                .is_ok()
        );
    }

    #[test]
    fn pipeline_read_reports_empty_open_and_prefix_eviction() {
        let store = PipeStore::new();
        let run = unique_run(&store);
        let pipeline = run.observer().begin(None, 2, false, false).unwrap();
        let (consumer, producer) = edge(&run, pipeline, 1);

        // 有效空流：open 且正文为空，和淘汰状态可区分。
        let empty = store.read(run.id(), 1, None).unwrap();
        assert!(empty.data.is_empty());
        assert!(empty.open);
        assert_eq!(empty.total_bytes, 0);
        assert_eq!(empty.start, 0);

        // 直接向已登记流追加超过 1 MiB，验证 read 的保留区间与总量。
        let state = run.state.stream(1).expect("registered stream");
        let payload: Vec<u8> = (0..STREAM_RING_CAP + 1024)
            .map(|index| (index % 97) as u8)
            .collect();
        state.observe(&payload);
        let read = store.read(run.id(), 1, Some(0)).unwrap();
        assert!(read.prefix_evicted);
        assert_eq!(read.start, 1024);
        assert_eq!(read.end, payload.len() as u64);
        assert_eq!(read.total_bytes, payload.len() as u64);
        assert_eq!(read.data, payload[1024..]);
        assert_eq!(read.data.len(), STREAM_RING_CAP);
        // 超出末端的起点夹到末端，返回空正文但保留总量。
        let beyond = store.read(run.id(), 1, Some(u64::MAX / 2)).unwrap();
        assert!(beyond.data.is_empty());
        assert!(!beyond.prefix_evicted);
        assert_eq!(beyond.start, payload.len() as u64);

        // 未知流 / 未知 run 都是不可用，而不是空正文。
        assert_eq!(
            store.read(run.id(), 99, None).unwrap_err(),
            PipeStoreError::Unavailable
        );
        assert_eq!(
            store.read("no-such-run", 1, None).unwrap_err(),
            PipeStoreError::Unavailable
        );
        drop(producer);
        drop(consumer);
        run.finish(RunEnd::exited(Some(0)));
    }
}
