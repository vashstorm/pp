//! 管道阶段镜像与临时正文保留（`PipeStore`）。
//!
//! 该模块是 plan §2.4–2.6 与 §3.1–3.4 的存储层：每次 bash 调用创建一个 run，
//! run 内观察到的多阶段管道、每条边的阶段状态和真实流经字节都保存在根会话共享
//! 的一棵有界 registry 里。它不依赖解释器实现细节，只实现
//! [`brush_core::PipelineObserver`] 并通过 [`relay`](super::relay) 创建进程内管道。
//!
//! 有界规则（全部为内部常量，不暴露设置）：
//! - 每流只保留最新 1 MiB 原始字节；
//! - 每 run 最多 16 条缓存流，流关闭后只保留最近 4 条已关闭流；
//! - 根会话合计最多 64 条缓存流 / 64 MiB 正文，最近 8 个已完成且有管道的 run；
//! - 每 run 保留最近 16 个 pipeline occurrence，每个最多 32 个阶段明细；
//! - 明细被淘汰不会丢掉“观察到过非零阶段”这一事实，计数始终累计；
//! - 淘汰优先最老已完成 run / 最老已关闭流，永不淘汰 open 流；没有可淘汰槽位时
//!   该边直接不捕获（`untapped_edges`），命令仍按普通管道执行。

use super::relay::{self, FINISH_GRACE, Relay};
use brush_core::PipelineObserver;
use brush_core::openfiles::OpenFile;
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    fmt, io,
    sync::{
        Arc, Mutex, MutexGuard, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// 单条流的保留正文上限。
pub(crate) const STREAM_RING_CAP: usize = 1024 * 1024;
const MAX_STREAMS_PER_RUN: usize = 16;
const MAX_CLOSED_STREAMS_PER_RUN: usize = 4;
const MAX_TOTAL_STREAMS: usize = 64;
const MAX_TOTAL_BODY: usize = 64 * 1024 * 1024;
/// 已完成仍有管道的 run 保留上限；后台任务回归也按此边界构造用例。
pub(super) const MAX_COMPLETED_RUNS: usize = 8;
/// 已完成但没有管道的 run 仍然可被 `/pipes`/状态查询看到（例如宿主启动失败），
/// 但单独限界，绝不挤占“最近 8 个有管道的 run”历史。
const MAX_EMPTY_RUNS: usize = 8;
const MAX_PIPELINES_PER_RUN: usize = 16;
const MAX_STAGES_PER_PIPELINE: usize = 32;
const MAX_LABEL_BYTES: usize = 512;
/// 卡片只复制最近一小段正文，避免每次刷新复制整个流缓存。
const PREVIEW_BYTES: u64 = 50 * 1024;

/// 临时管道 URI 的 scheme。
pub(crate) const PIPE_URI_PREFIX: &str = "pipe://";
/// 未知、已淘汰或来自另一会话时的固定提示；空流绝不能伪装成这个状态。
pub(crate) const PIPES_UNAVAILABLE: &str =
    "Temporary pipe output is unavailable (evicted or from another session).";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CloseReason {
    Eof,
    DownstreamClosed,
    Interrupted,
    Error,
}

impl CloseReason {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Eof => "eof",
            Self::DownstreamClosed => "downstream_closed",
            Self::Interrupted => "interrupted",
            Self::Error => "error",
        }
    }
}

/// 流内部状态变化事件；用于把 run 级 revision 与已关闭流修剪串起来。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StreamEvent {
    Data,
    Closed,
}

type StreamHook = Arc<dyn Fn(StreamEvent) + Send + Sync>;

/// 每流固定 1 MiB 的尾部环形缓存；绝对偏移从 0 开始、只增不减。
#[derive(Default)]
struct Ring {
    body: Vec<u8>,
    total: u64,
}

impl Ring {
    fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.total = self.total.saturating_add(data.len() as u64);
        if data.len() >= STREAM_RING_CAP {
            self.body.clear();
            self.body
                .extend_from_slice(&data[data.len() - STREAM_RING_CAP..]);
            return;
        }
        let overflow = (self.body.len() + data.len()).saturating_sub(STREAM_RING_CAP);
        if overflow > 0 {
            self.body.drain(..overflow);
        }
        self.body.extend_from_slice(data);
    }

    fn len(&self) -> u64 {
        self.body.len() as u64
    }

    fn start(&self) -> u64 {
        self.total.saturating_sub(self.len())
    }

    const fn end(&self) -> u64 {
        self.total
    }

    fn read(&self, from: u64, to: u64) -> Vec<u8> {
        let start = self.start();
        let low = from.clamp(start, self.total);
        let high = to.clamp(low, self.total);
        self.body[(low - start) as usize..(high - start) as usize].to_vec()
    }
}

/// 一条被观察的管道边。id 在 run 内单调递增且永不复用。
pub(super) struct StreamState {
    id: u64,
    pipeline: u64,
    producer_stage: usize,
    consumer_stage: usize,
    producer: String,
    consumer: String,
    ring: Mutex<Ring>,
    /// `None` 表示仍打开；封存后保存真实原因。open 与原因在同一把锁下，快照不会
    /// 读到“已关闭但原因缺失”的中间状态。
    close: Mutex<Option<CloseReason>>,
    revision: AtomicU64,
    hook: StreamHook,
}

impl StreamState {
    pub(super) fn new(
        id: u64,
        pipeline: u64,
        producer_stage: usize,
        consumer_stage: usize,
        producer: &str,
        consumer: &str,
        hook: StreamHook,
    ) -> Self {
        Self {
            id,
            pipeline,
            producer_stage,
            consumer_stage,
            producer: truncate_label(producer).to_owned(),
            consumer: truncate_label(consumer).to_owned(),
            ring: Mutex::new(Ring::default()),
            close: Mutex::new(None),
            revision: AtomicU64::new(0),
            hook,
        }
    }

    /// 追加中继线程实际观察到的字节。先镜像、后转发由 relay 保证。
    pub(super) fn observe(&self, data: &[u8]) {
        if data.is_empty() || !self.is_open() {
            return;
        }
        lock(&self.ring).push(data);
        self.revision.fetch_add(1, Ordering::Release);
        (self.hook)(StreamEvent::Data);
    }

    /// 封存该流。只有第一次调用生效，之后返回 `false`。
    pub(super) fn finish(&self, reason: CloseReason) -> bool {
        {
            let mut close = lock(&self.close);
            if close.is_some() {
                return false;
            }
            *close = Some(reason);
        }
        self.revision.fetch_add(1, Ordering::Release);
        (self.hook)(StreamEvent::Closed);
        true
    }

    pub(super) fn is_open(&self) -> bool {
        lock(&self.close).is_none()
    }

    pub(super) fn close_reason(&self) -> Option<CloseReason> {
        *lock(&self.close)
    }

    pub(super) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(super) fn retained(&self) -> Vec<u8> {
        let ring = lock(&self.ring);
        ring.read(ring.start(), ring.end())
    }

    pub(super) fn retained_range(&self) -> (u64, u64) {
        let ring = lock(&self.ring);
        (ring.start(), ring.end())
    }

    /// 在单次持锁内生成一致的保留区间、总观察字节与正文。
    fn retained_snapshot(&self) -> (u64, u64, u64) {
        let ring = lock(&self.ring);
        (ring.start(), ring.end(), ring.total)
    }

    /// 单次持锁读取：返回的 `start`/`end` 与 `data` 一定来自同一份快照。
    pub(super) fn read_snapshot(&self, from: Option<u64>) -> StreamSlice {
        let ring = lock(&self.ring);
        let retained_start = ring.start();
        let end = ring.end();
        let requested = from.unwrap_or(retained_start);
        let start = requested.clamp(retained_start, end);
        StreamSlice {
            start,
            end,
            data: ring.read(start, end),
            prefix_evicted: requested < retained_start,
        }
    }

    fn meta(&self, run_id: &str) -> StreamMeta {
        let (start, end, total) = self.retained_snapshot();
        let close = self.close_reason();
        StreamMeta {
            id: self.id,
            pipeline: self.pipeline,
            producer_stage: self.producer_stage,
            consumer_stage: self.consumer_stage,
            producer: self.producer.clone(),
            consumer: self.consumer.clone(),
            uri: stream_uri(run_id, self.id),
            open: close.is_none(),
            total_bytes: total,
            retained_start: start,
            retained_end: end,
            revision: self.revision(),
            close_reason: close.map(|reason| reason.as_str().to_owned()),
        }
    }
}

#[cfg(test)]
pub(super) fn detached_stream(id: u64) -> Arc<StreamState> {
    Arc::new(StreamState::new(
        id,
        1,
        1,
        2,
        "producer",
        "consumer",
        Arc::new(|_| {}),
    ))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StageState {
    Running,
    Exited,
    Unknown,
    Interrupted,
}

struct StageRecord {
    index: usize,
    command: String,
    state: StageState,
    status: Option<u8>,
    signal: Option<i32>,
}

struct PipelineRecord {
    id: u64,
    parent: Option<u64>,
    stage_count: usize,
    bang: bool,
    pipefail: bool,
    status: Option<u8>,
    stages: Vec<StageRecord>,
}

impl PipelineRecord {
    fn materialize(&self) -> PipelineInfo {
        PipelineInfo {
            id: self.id,
            parent: self.parent,
            stage_count: self.stage_count,
            bang: self.bang,
            pipefail: self.pipefail,
            status: self.status,
            stages: self
                .stages
                .iter()
                .map(|stage| StageInfo {
                    index: stage.index,
                    command: stage.command.clone(),
                    state: stage.state,
                    status: stage.status,
                    signal: stage.signal,
                })
                .collect(),
        }
    }
}

#[derive(Default)]
struct RunMeta {
    pipelines: VecDeque<PipelineRecord>,
    omitted_pipelines: u64,
    omitted_stages: u64,
    untapped_edges: u64,
    evicted_streams: u64,
    nonzero_stages: u64,
    capture_error: Option<String>,
}

/// 一次 run 的终态。`exit_code` 来自受监督 shell；中断原因只用于展示。
#[derive(Clone, Debug)]
pub(crate) struct RunEnd {
    pub(crate) exit_code: Option<i32>,
    pub(crate) interrupted: Option<String>,
    pub(crate) capture_error: Option<String>,
}

impl RunEnd {
    pub(crate) fn exited(exit_code: Option<i32>) -> Self {
        Self {
            exit_code,
            interrupted: None,
            capture_error: None,
        }
    }

    pub(crate) fn interrupted(reason: impl Into<String>) -> Self {
        Self {
            exit_code: None,
            interrupted: Some(reason.into()),
            capture_error: None,
        }
    }
}

struct RunState {
    id: String,
    owner: String,
    stop: Arc<AtomicBool>,
    next_pipeline: AtomicU64,
    next_stream: AtomicU64,
    revision: AtomicU64,
    end: Mutex<Option<RunEnd>>,
    meta: Mutex<RunMeta>,
    streams: Mutex<Vec<Arc<StreamState>>>,
    relays: Mutex<Vec<Relay>>,
}

impl RunState {
    fn new(id: String, owner: String) -> Self {
        Self {
            id,
            owner,
            stop: Arc::new(AtomicBool::new(false)),
            next_pipeline: AtomicU64::new(0),
            next_stream: AtomicU64::new(0),
            revision: AtomicU64::new(0),
            end: Mutex::new(None),
            meta: Mutex::new(RunMeta::default()),
            streams: Mutex::new(Vec::new()),
            relays: Mutex::new(Vec::new()),
        }
    }

    fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Release);
    }

    fn is_finished(&self) -> bool {
        lock(&self.end).is_some()
    }

    fn interrupted(&self) -> bool {
        lock(&self.end)
            .as_ref()
            .is_some_and(|end| end.interrupted.is_some())
    }

    fn record_pipeline(
        &self,
        parent: Option<u64>,
        stages: usize,
        bang: bool,
        pipefail: bool,
    ) -> u64 {
        let id = self.next_pipeline.fetch_add(1, Ordering::Relaxed) + 1;
        if self.is_finished() {
            return id;
        }
        let mut meta = lock(&self.meta);
        if meta.pipelines.len() >= MAX_PIPELINES_PER_RUN {
            meta.pipelines.pop_front();
            meta.omitted_pipelines += 1;
        }
        let detailed = stages.min(MAX_STAGES_PER_PIPELINE);
        meta.omitted_stages += (stages - detailed) as u64;
        meta.pipelines.push_back(PipelineRecord {
            id,
            parent,
            stage_count: stages,
            bang,
            pipefail,
            status: None,
            stages: (1..=detailed)
                .map(|index| StageRecord {
                    index,
                    command: String::new(),
                    state: StageState::Running,
                    status: None,
                    signal: None,
                })
                .collect(),
        });
        drop(meta);
        self.bump();
        id
    }

    fn set_label(&self, id: u64, stage: usize, command: &str) {
        let mut changed = false;
        {
            let mut meta = lock(&self.meta);
            if let Some(detail) = meta
                .pipelines
                .iter_mut()
                .find(|record| record.id == id)
                .and_then(|record| {
                    record
                        .stages
                        .iter_mut()
                        .find(|detail| detail.index == stage)
                })
            {
                let label = truncate_label(command);
                if detail.command != label {
                    detail.command = label.to_owned();
                    changed = true;
                }
            }
        }
        if changed {
            self.bump();
        }
    }

    fn record_stage_done(&self, id: u64, stage: usize, status: u8, signal: Option<i32>) {
        let mut meta = lock(&self.meta);
        if status != 0 || signal.is_some() {
            // 即使明细已被配额淘汰，只要真实观察到非零状态就累计计数。
            meta.nonzero_stages += 1;
        }
        let mut changed = false;
        if let Some(detail) = meta
            .pipelines
            .iter_mut()
            .find(|record| record.id == id)
            .and_then(|record| {
                record
                    .stages
                    .iter_mut()
                    .find(|detail| detail.index == stage)
            })
        {
            detail.state = StageState::Exited;
            detail.status = Some(status);
            detail.signal = signal;
            changed = true;
        }
        let counted = status != 0 || signal.is_some();
        drop(meta);
        if changed || counted {
            self.bump();
        }
    }

    fn finish_pipeline(&self, id: u64, status: u8) {
        let mut found = false;
        {
            let mut meta = lock(&self.meta);
            if let Some(record) = meta.pipelines.iter_mut().find(|record| record.id == id) {
                record.status = Some(status);
                for detail in &mut record.stages {
                    if detail.state == StageState::Running {
                        // 未收到真实等待结果的阶段只能是 unknown，不能补 0。
                        detail.state = StageState::Unknown;
                    }
                }
                found = true;
            }
        }
        if found {
            self.bump();
        }
    }

    fn note_untapped(&self) {
        lock(&self.meta).untapped_edges += 1;
        self.bump();
    }

    fn note_capture_error(&self, message: impl Into<String>) {
        let mut meta = lock(&self.meta);
        if meta.capture_error.is_none() {
            meta.capture_error = Some(message.into());
        }
        drop(meta);
        self.bump();
    }

    fn push_stream(&self, stream: Arc<StreamState>) {
        lock(&self.streams).push(stream);
    }

    fn push_relay(&self, relay: Relay) {
        lock(&self.relays).push(relay);
    }

    /// 回收已经结束的中继线程。
    ///
    /// 已封存/被淘汰的流不会在 run 结束前自动释放它的中继句柄，长 run 若只保留
    /// 到 finish 才 join 会无界积累 JoinHandle。这里在**不持有任何锁**的情况下
    /// 逐个 join 已完成的中继；未完成的条目留在列表里由本次或收尾继续等待。
    fn reap_finished_relays(&self) {
        loop {
            let relay = {
                let mut relays = lock(&self.relays);
                let Some(index) = relays.iter().position(Relay::is_done) else {
                    break;
                };
                Some(relays.remove(index))
            };
            let Some(mut relay) = relay else {
                break;
            };
            relay.join();
        }
    }

    #[cfg(test)]
    fn relay_handle_count(&self) -> usize {
        lock(&self.relays).len()
    }

    fn stream(&self, id: u64) -> Option<Arc<StreamState>> {
        lock(&self.streams)
            .iter()
            .find(|stream| stream.id == id)
            .cloned()
    }

    fn stream_count(&self) -> usize {
        lock(&self.streams).len()
    }

    fn pipeline_count(&self) -> usize {
        lock(&self.meta).pipelines.len()
    }

    /// 已关闭流超过每 run 保留上限时淘汰最老的那几条。
    fn stream_closed(&self) {
        let mut evicted = 0u64;
        {
            let mut streams = lock(&self.streams);
            let closed: Vec<usize> = streams
                .iter()
                .enumerate()
                .filter(|(_, stream)| !stream.is_open())
                .map(|(index, _)| index)
                .collect();
            let remove = closed.len().saturating_sub(MAX_CLOSED_STREAMS_PER_RUN);
            if remove > 0 {
                // 流 id 单调递增，保持插入顺序的 streams 前面就是更老的已关闭流。
                for index in closed[..remove].iter().rev() {
                    streams.remove(*index);
                    evicted += 1;
                }
            }
        }
        if evicted > 0 {
            lock(&self.meta).evicted_streams += evicted;
        }
        self.bump();
        // 封存/淘汰后立即回收其它已完成的中继；当前中继尚未置 done，不会被自 join。
        // 这样即使之后不再创建新边，句柄数量也只与并发 open 流相关。
        self.reap_finished_relays();
    }

    fn begin_finish(&self, end: &RunEnd) -> bool {
        let mut slot = lock(&self.end);
        if slot.is_some() {
            return false;
        }
        if let Some(error) = &end.capture_error {
            let mut meta = lock(&self.meta);
            if meta.capture_error.is_none() {
                meta.capture_error = Some(error.clone());
            }
        }
        *slot = Some(end.clone());
        drop(slot);
        {
            let interrupted = end.interrupted.is_some();
            let mut meta = lock(&self.meta);
            for record in &mut meta.pipelines {
                for detail in &mut record.stages {
                    if detail.state == StageState::Running {
                        // 解释器收尾前仍没有真实等待结果的阶段：取消为 interrupted，
                        // 其他原因是 unknown；两者都不补造 0。
                        detail.state = if interrupted {
                            StageState::Interrupted
                        } else {
                            StageState::Unknown
                        };
                    }
                }
            }
        }
        self.bump();
        true
    }

    fn snapshot(&self) -> RunInfo {
        let (
            pipelines,
            nonzero_stages,
            omitted_pipelines,
            omitted_stages,
            untapped_edges,
            evicted_streams,
            capture_error,
        ) = {
            let meta = lock(&self.meta);
            (
                meta.pipelines
                    .iter()
                    .map(PipelineRecord::materialize)
                    .collect::<Vec<_>>(),
                meta.nonzero_stages,
                meta.omitted_pipelines,
                meta.omitted_stages,
                meta.untapped_edges,
                meta.evicted_streams,
                meta.capture_error.clone(),
            )
        };
        let streams = lock(&self.streams)
            .iter()
            .map(|stream| stream.meta(&self.id))
            .collect::<Vec<_>>();
        let end = lock(&self.end).clone();
        let (state, exit_code) = match &end {
            None => (RunStateKind::Running, None),
            Some(end) if end.interrupted.is_some() => (RunStateKind::Interrupted, end.exit_code),
            Some(end) => (RunStateKind::Exited, end.exit_code),
        };
        RunInfo {
            run_id: self.id.clone(),
            uri: run_uri(&self.id),
            owner: self.owner.clone(),
            state,
            exit_code,
            revision: self.revision.load(Ordering::Acquire),
            pipelines,
            streams,
            nonzero_stages,
            omitted_pipelines,
            omitted_stages,
            untapped_edges,
            evicted_streams,
            incomplete: omitted_pipelines > 0
                || omitted_stages > 0
                || untapped_edges > 0
                || evicted_streams > 0
                || capture_error.is_some(),
            capture_error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RunStateKind {
    Running,
    Exited,
    Interrupted,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct StageInfo {
    pub(crate) index: usize,
    pub(crate) command: String,
    pub(crate) state: StageState,
    pub(crate) status: Option<u8>,
    pub(crate) signal: Option<i32>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct PipelineInfo {
    pub(crate) id: u64,
    pub(crate) parent: Option<u64>,
    pub(crate) stage_count: usize,
    pub(crate) bang: bool,
    pub(crate) pipefail: bool,
    pub(crate) status: Option<u8>,
    pub(crate) stages: Vec<StageInfo>,
}

/// 一次流读取的一致快照（区间与正文来自同一把 ring 锁）。
pub(super) struct StreamSlice {
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) data: Vec<u8>,
    pub(super) prefix_evicted: bool,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct StreamMeta {
    pub(crate) id: u64,
    pub(crate) pipeline: u64,
    pub(crate) producer_stage: usize,
    pub(crate) consumer_stage: usize,
    pub(crate) producer: String,
    pub(crate) consumer: String,
    pub(crate) uri: String,
    pub(crate) open: bool,
    pub(crate) total_bytes: u64,
    pub(crate) retained_start: u64,
    pub(crate) retained_end: u64,
    pub(crate) revision: u64,
    pub(crate) close_reason: Option<String>,
}

/// run 级别的有界元数据；`pipes_json` 与列表/快照共用同一结构，不含阶段正文。
#[derive(Clone, Debug, Serialize)]
pub(crate) struct RunInfo {
    pub(crate) run_id: String,
    pub(crate) uri: String,
    pub(crate) owner: String,
    pub(crate) state: RunStateKind,
    pub(crate) exit_code: Option<i32>,
    pub(crate) revision: u64,
    pub(crate) pipelines: Vec<PipelineInfo>,
    pub(crate) streams: Vec<StreamMeta>,
    pub(crate) nonzero_stages: u64,
    pub(crate) omitted_pipelines: u64,
    pub(crate) omitted_stages: u64,
    pub(crate) untapped_edges: u64,
    pub(crate) evicted_streams: u64,
    pub(crate) incomplete: bool,
    pub(crate) capture_error: Option<String>,
}

impl RunInfo {
    pub(crate) fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// 一次 `read` 的快照：正文 + 该流的保留区间、总观察字节与 revision。
#[derive(Clone, Debug)]
pub(crate) struct StreamRead {
    /// 本次返回正文在流中的绝对起始偏移（已夹到当前保留区间）。
    pub(crate) start: u64,
    /// 本次返回正文的绝对结束偏移，等于此刻观察到的总字节数。
    pub(crate) end: u64,
    pub(crate) data: Vec<u8>,
    pub(crate) open: bool,
    pub(crate) revision: u64,
    pub(crate) total_bytes: u64,
    /// 请求起点早于保留区间：前缀已被覆盖。
    pub(crate) prefix_evicted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PipeStoreError {
    /// 未知 run、未知流、已淘汰或来自另一会话；调用方应提示而非返回空正文。
    Unavailable,
    DuplicateRun,
}

impl PipeStoreError {
    pub(crate) const fn message(self) -> &'static str {
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
pub(crate) struct PipeStore {
    inner: Arc<StoreInner>,
}

impl Default for PipeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PipeStore {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(StoreInner::default()),
        }
    }

    /// 根会话切换/事件 epoch 依据：只有同一实例才能继续沿用旧 URI。
    pub(crate) fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// 为一个新 bash 调用创建 run。run ID 由调用方生成（16 字节小写 hex）。
    pub(crate) fn start_run(
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
    pub(crate) fn list(&self) -> Vec<RunInfo> {
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

    pub(crate) fn snapshot(&self, run_id: &str) -> Result<RunInfo, PipeStoreError> {
        Ok(self.run_state(run_id)?.snapshot())
    }

    pub(crate) fn stream_revision(&self, run_id: &str, stream: u64) -> Option<u64> {
        let state = self.run_state(run_id).ok()?;
        state.stream(stream).map(|stream| stream.revision())
    }

    /// 读取一条流的保留正文。请求起点被夹到当前保留区间，并显式报告前缀淘汰。
    pub(crate) fn read(
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

    fn capture_edge(
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
    fn finalize_run(&self, run: &Arc<RunState>) {
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

struct RunObserver {
    store: PipeStore,
    state: Arc<RunState>,
}

impl PipelineObserver for RunObserver {
    fn begin(
        &self,
        parent: Option<u64>,
        stages: usize,
        bang: bool,
        pipefail: bool,
    ) -> io::Result<u64> {
        Ok(self.state.record_pipeline(parent, stages, bang, pipefail))
    }

    fn label(&self, id: u64, stage: usize, command: &str) -> io::Result<()> {
        self.state.set_label(id, stage, command);
        Ok(())
    }

    fn pipe(
        &self,
        id: u64,
        edge: usize,
        producer: &str,
        consumer: &str,
    ) -> io::Result<Option<(OpenFile, OpenFile)>> {
        self.store
            .capture_edge(&self.state, id, edge, producer, consumer)
    }

    fn stage_done(&self, id: u64, stage: usize, status: u8, signal: Option<i32>) -> io::Result<()> {
        self.state.record_stage_done(id, stage, status, signal);
        Ok(())
    }

    fn finish(&self, id: u64, status: u8) -> io::Result<()> {
        self.state.finish_pipeline(id, status);
        Ok(())
    }
}

/// 一次 bash 调用的观察句柄。clone 共享同一 run。
#[derive(Clone)]
pub(crate) struct PipeRun {
    store: PipeStore,
    state: Arc<RunState>,
    observer: Arc<RunObserver>,
}

impl PipeRun {
    #[cfg(test)]
    pub(crate) fn id(&self) -> &str {
        &self.state.id
    }

    /// 交给 `ExecutionParameters::set_pipeline_observer` 的进程内观察者。
    pub(crate) fn observer(&self) -> Arc<dyn PipelineObserver> {
        self.observer.clone()
    }

    pub(crate) fn revision(&self) -> u64 {
        self.state.revision.load(Ordering::Acquire)
    }

    /// 当前仍持有的中继线程句柄数；长 run 中应只与并发 open 流数量相关。
    #[cfg(test)]
    pub(crate) fn relay_handle_count(&self) -> usize {
        self.state.relay_handle_count()
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.state.is_finished()
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> RunInfo {
        self.state.snapshot()
    }

    /// 运行中与最终结果共用的 `pipes` JSON（有界元数据，不含阶段正文）。
    pub(crate) fn pipes_json(&self) -> Value {
        self.state.snapshot().to_json()
    }

    /// 只预览最新顶层管道的第一条边，避免多级管道把同一正文重复展示。
    pub(crate) fn preview(&self) -> Option<String> {
        let pipeline = lock(&self.state.meta)
            .pipelines
            .iter()
            .rev()
            .find(|pipeline| pipeline.parent.is_none())?
            .id;
        let stream = lock(&self.state.streams)
            .iter()
            .find(|stream| stream.pipeline == pipeline && stream.producer_stage == 1)
            .cloned()?;
        let ring = lock(&stream.ring);
        let start = ring.start().max(ring.end().saturating_sub(PREVIEW_BYTES));
        let bytes = ring.read(start, ring.end());
        drop(ring);
        // 尾部窗口可能从 UTF-8 字符中间开始，丢弃残缺前缀。
        let start = if start > 0 {
            bytes
                .iter()
                .position(|byte| byte & 0xc0 != 0x80)
                .unwrap_or(bytes.len())
        } else {
            0
        };
        // 复用主输出的解码与控制序列过滤；不完整字符留待后续快照显示。
        let mut normalizer = crate::tools::output::Normalizer::default();
        let mut preview = normalizer.feed(&bytes[start..], false);
        if !stream.is_open() {
            preview.push_str(&normalizer.feed(&[], true));
        }
        (!preview.is_empty()).then_some(preview)
    }

    /// 封存本次 run：等待/停止中继、封闭剩余流、登记完成历史。
    ///
    /// 重复调用无副作用；返回最终快照。正常收尾先给中继一个观察 EOF 的宽限，
    /// 超时或已中断则协作停止（`Interrupted`），绝不完结仍在运行的流。
    pub(crate) fn finish(&self, end: RunEnd) -> RunInfo {
        if !self.state.begin_finish(&end) {
            return self.state.snapshot();
        }
        let mut relays = {
            let mut guard = lock(&self.state.relays);
            std::mem::take(&mut *guard)
        };
        let interrupted = self.state.interrupted();
        if interrupted {
            self.state.stop.store(true, Ordering::Release);
        }
        if !relays.is_empty() {
            let deadline = Instant::now()
                + if interrupted {
                    Duration::ZERO
                } else {
                    FINISH_GRACE
                };
            while Instant::now() < deadline && relays.iter().any(|relay| !relay.is_done()) {
                std::thread::sleep(Duration::from_millis(5));
            }
            if relays.iter().any(|relay| !relay.is_done()) {
                self.state.stop.store(true, Ordering::Release);
            }
            for relay in &mut relays {
                relay.join();
            }
        }
        // 中继都 join 过之后仍 open 的流只能是中继异常退出；按真实终态封存。
        let streams = lock(&self.state.streams).clone();
        for stream in streams {
            if stream.is_open() {
                stream.finish(if interrupted {
                    CloseReason::Interrupted
                } else {
                    CloseReason::Error
                });
            }
        }
        self.store.finalize_run(&self.state);
        self.state.snapshot()
    }
}

/// 回归测试用弱观察句柄：确认 run 状态（含环形缓存）已无强引用。
#[cfg(test)]
#[derive(Clone)]
pub(super) struct RunWatch(Weak<RunState>);

#[cfg(test)]
impl RunWatch {
    pub(super) fn is_alive(&self) -> bool {
        self.0.strong_count() > 0
    }
}

#[cfg(test)]
impl PipeRun {
    pub(super) fn watch(&self) -> RunWatch {
        RunWatch(Arc::downgrade(&self.state))
    }
}

/// 阶段标签按 UTF-8 边界裁到 512 字节。
fn truncate_label(label: &str) -> &str {
    if label.len() <= MAX_LABEL_BYTES {
        return label;
    }
    let mut end = MAX_LABEL_BYTES;
    while end > 0 && !label.is_char_boundary(end) {
        end -= 1;
    }
    &label[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::AtomicU64;
    use std::thread;
    use std::time::Duration;

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn unique_run(store: &PipeStore) -> PipeRun {
        loop {
            let id = format!("run{:032x}", NEXT.fetch_add(1, Ordering::Relaxed));
            match store.start_run(id, "test") {
                Ok(run) => return run,
                Err(PipeStoreError::DuplicateRun) => {}
                Err(error) => panic!("unexpected start_run error: {error}"),
            }
        }
    }

    #[test]
    fn pipeline_card_preview_is_bounded_and_decodes_partial_utf8() {
        let run = unique_run(&PipeStore::default());
        let pipeline = run.observer().begin(None, 3, false, false).unwrap();
        let stream = Arc::new(StreamState::new(
            1,
            pipeline,
            1,
            2,
            "producer",
            "cat",
            Arc::new(|_| {}),
        ));
        run.state.push_stream(stream.clone());
        stream.observe(b"\x1b[31mA\xff\xe4");
        assert_eq!(run.preview().as_deref(), Some("A�"));
        stream.observe(b"\xb8\xad\x1b[0m");
        assert_eq!(run.preview().as_deref(), Some("A�中"));
        let duplicate = Arc::new(StreamState::new(
            2,
            pipeline,
            2,
            3,
            "cat",
            "tail",
            Arc::new(|_| {}),
        ));
        duplicate.observe(b"duplicate");
        run.state.push_stream(duplicate);
        let nested = run
            .observer()
            .begin(Some(pipeline), 2, false, false)
            .unwrap();
        let nested_stream = Arc::new(StreamState::new(
            3,
            nested,
            1,
            2,
            "nested",
            "cat",
            Arc::new(|_| {}),
        ));
        nested_stream.observe(b"nested");
        run.state.push_stream(nested_stream);
        assert_eq!(run.preview().as_deref(), Some("A�中"));
        stream.observe("中".repeat(PREVIEW_BYTES as usize).as_bytes());
        let preview = run.preview().unwrap();
        assert!(preview.len() <= PREVIEW_BYTES as usize);
        assert!(preview.chars().all(|ch| ch == '中'));
        run.observer().begin(None, 2, false, false).unwrap();
        assert_eq!(run.preview(), None, "新管道不能沿用旧管道的预览");
    }

    fn edge(run: &PipeRun, pipeline: u64, index: usize) -> (OpenFile, OpenFile) {
        run.observer()
            .pipe(pipeline, index, "printf", "cat")
            .expect("capture edge")
            .expect("edge must be captured")
    }

    fn producer_writer(open: OpenFile) -> std::io::PipeWriter {
        let OpenFile::PipeWriter(writer) = open else {
            panic!("producer must be a pipe writer");
        };
        writer
    }

    fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }

    #[test]
    fn pipeline_stream_ring_keeps_latest_mebibyte() {
        let mut ring = Ring::default();
        let payload: Vec<u8> = (0..STREAM_RING_CAP + 4096)
            .map(|index| (index % 251) as u8)
            .collect();
        ring.push(&payload);
        assert_eq!(ring.total, payload.len() as u64);
        assert_eq!(ring.start(), 4096);
        assert_eq!(ring.read(ring.start(), ring.end()), payload[4096..]);
        assert_eq!(ring.read(0, ring.end()), payload[4096..]);
        assert_eq!(
            ring.read(ring.end() - 16, ring.end()),
            payload[payload.len() - 16..]
        );

        // 单个超过容量块只保留它的尾部，且绝对偏移继续累加。
        let big = vec![7u8; STREAM_RING_CAP + 100];
        ring.push(&big);
        assert_eq!(ring.start(), payload.len() as u64 + 100);
        assert_eq!(ring.read(ring.start(), ring.end()), big[100..]);
        assert_eq!(ring.len(), STREAM_RING_CAP as u64);
    }

    #[test]
    fn pipeline_stream_ids_are_run_local_and_monotonic() {
        let store = PipeStore::new();
        let first = unique_run(&store);
        let second = unique_run(&store);

        let (_, producer_a) = edge(&first, 1, 1);
        let (_, producer_b) = edge(&first, 1, 2);
        let (_, producer_c) = edge(&second, 1, 1);
        drop((producer_a, producer_b, producer_c));

        let first_info = first.snapshot();
        let second_info = second.snapshot();
        assert_eq!(
            first_info.streams.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            second_info.streams.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1]
        );
        assert!(store.snapshot(first.id()).is_ok());
        assert!(store.snapshot("missing-run").is_err());
        first.finish(RunEnd::exited(Some(0)));
        second.finish(RunEnd::exited(Some(0)));
    }

    #[test]
    fn pipeline_run_untapped_when_all_slots_are_open() {
        let store = PipeStore::new();
        let run = unique_run(&store);
        let pipeline = run
            .observer()
            .begin(None, MAX_STREAMS_PER_RUN + 2, false, false)
            .unwrap();
        // 保持解释器侧端点存活，确保这些流始终是 open 流。
        let mut live = Vec::new();
        for stream in 1..=MAX_STREAMS_PER_RUN {
            let pair = run
                .observer()
                .pipe(pipeline, stream, "yes", "head")
                .expect("capture edge")
                .unwrap_or_else(|| panic!("stream {stream} must be captured"));
            live.push(pair);
        }
        // 所有槽位都是 open 流：第 17 条边直接不捕获，不淘汰任何 open 流。
        let rejected = run
            .observer()
            .pipe(pipeline, MAX_STREAMS_PER_RUN + 1, "yes", "head")
            .expect("quota rejection is not an execution error");
        assert!(rejected.is_none());

        let info = run.snapshot();
        assert_eq!(info.streams.len(), MAX_STREAMS_PER_RUN);
        assert_eq!(info.untapped_edges, 1);
        assert!(info.incomplete);
        assert!(info.streams.iter().all(|stream| stream.open));
        // 配额拒绝也不占用 stream id：仍保持单调递增。
        let mut ids = info
            .streams
            .iter()
            .map(|stream| stream.id)
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, (1..=MAX_STREAMS_PER_RUN as u64).collect::<Vec<_>>());
        drop(live);
        run.finish(RunEnd::exited(Some(0)));
    }

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
    fn pipeline_sequential_pipelines_do_not_accumulate_relay_handles() {
        let store = PipeStore::new();
        let run = unique_run(&store);
        let mut peak = 0usize;
        for index in 0..48u64 {
            let pipeline = run.observer().begin(None, 2, false, false).unwrap();
            let (consumer, producer) = edge(&run, pipeline, 1);
            let mut writer = producer_writer(producer);
            writer.write_all(b"stage body\n").expect("write edge");
            drop(writer);
            let stream_id = index + 1;
            assert!(
                wait_for(|| store
                    .read(run.id(), stream_id, None)
                    .is_ok_and(|read| !read.open && !read.data.is_empty())),
                "iteration {index}: stream {stream_id} must close"
            );
            drop(consumer);
            // 已封存流的中继句柄必须在下一次登记时被回收，而不是只堆积到 run 结束。
            peak = peak.max(run.relay_handle_count());
            assert!(
                run.relay_handle_count() <= MAX_CLOSED_STREAMS_PER_RUN + 1,
                "iteration {index}: {} live relay handles",
                run.relay_handle_count()
            );
        }
        assert!(
            peak <= MAX_CLOSED_STREAMS_PER_RUN + 1,
            "sequential pipelines accumulated {peak} relay handles"
        );
        run.finish(RunEnd::exited(Some(0)));
        assert_eq!(run.relay_handle_count(), 0);
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
    fn pipeline_metadata_retains_recent_occurrences() {
        let store = PipeStore::new();
        let run = unique_run(&store);
        for index in 0..(MAX_PIPELINES_PER_RUN + 4) {
            let id = run
                .observer()
                .begin(None, 2, index % 2 == 0, false)
                .unwrap();
            run.observer().stage_done(id, 1, 0, None).unwrap();
            run.observer().finish(id, 0).unwrap();
        }
        let info = run.snapshot();
        assert_eq!(info.pipelines.len(), MAX_PIPELINES_PER_RUN);
        assert_eq!(info.omitted_pipelines, 4);
        assert_eq!(info.nonzero_stages, 0);
        assert!(info.incomplete);
        // 前 4 个 occurrence 的明细被淘汰，但 id 序列保持单调且不复用。
        let ids = info.pipelines.iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(
            ids,
            (5u64..=(MAX_PIPELINES_PER_RUN as u64 + 4)).collect::<Vec<_>>()
        );
        run.finish(RunEnd::exited(Some(0)));
    }

    #[test]
    fn pipeline_metadata_truncates_labels_and_counts_nonzero_beyond_details() {
        let store = PipeStore::new();
        let run = unique_run(&store);
        let id = run
            .observer()
            .begin(None, MAX_STAGES_PER_PIPELINE + 8, false, true)
            .unwrap();
        let long = "λ".repeat(MAX_LABEL_BYTES);
        run.observer().label(id, 1, &long).unwrap();
        // 超出明细容量的阶段也必须计入非零事实。
        run.observer().stage_done(id, 1, 3, None).unwrap();
        run.observer()
            .stage_done(id, MAX_STAGES_PER_PIPELINE + 8, 7, Some(13))
            .unwrap();
        run.observer().finish(id, 3).unwrap();

        let info = run.snapshot();
        assert_eq!(info.pipelines.len(), 1);
        let pipeline = &info.pipelines[0];
        assert_eq!(pipeline.stage_count, MAX_STAGES_PER_PIPELINE + 8);
        assert_eq!(pipeline.stages.len(), MAX_STAGES_PER_PIPELINE);
        assert_eq!(pipeline.stages[0].command.len(), MAX_LABEL_BYTES);
        assert!(pipeline.stages[0].command.is_char_boundary(MAX_LABEL_BYTES));
        assert_eq!(pipeline.status, Some(3));
        assert_eq!(info.nonzero_stages, 2);
        assert_eq!(info.omitted_stages, 8);
        // 未报告真实等待结果的阶段残留为 unknown，绝不补 0。
        let tail = pipeline.stages.last().unwrap();
        assert_eq!(tail.state, StageState::Unknown);
        assert_eq!(tail.status, None);
        run.finish(RunEnd::exited(Some(3)));
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

    #[test]
    fn pipeline_pipes_json_carries_bounded_metadata_only() {
        let store = PipeStore::new();
        let run = unique_run(&store);
        let id = run.observer().begin(None, 2, true, false).unwrap();
        run.observer().label(id, 1, "printf").unwrap();
        run.observer().label(id, 2, "cat").unwrap();
        let (consumer, producer) = edge(&run, id, 1);
        drop((consumer, producer));
        run.observer().stage_done(id, 1, 7, None).unwrap();
        run.observer().stage_done(id, 2, 0, Some(13)).unwrap();
        run.observer().finish(id, 0).unwrap();
        let info = run.finish(RunEnd::exited(Some(0)));
        let json = info.to_json();

        assert_eq!(json["run_id"], run.id());
        assert_eq!(json["uri"], run_uri(run.id()));
        assert_eq!(json["state"], "exited");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["nonzero_stages"], 2);
        assert_eq!(json["incomplete"], false);
        assert!(json.get("capture_error").is_some());
        let pipeline = &json["pipelines"][0];
        for key in [
            "id",
            "parent",
            "stage_count",
            "bang",
            "pipefail",
            "status",
            "stages",
        ] {
            assert!(pipeline.get(key).is_some(), "missing pipeline key {key}");
        }
        assert_eq!(pipeline["bang"], true);
        assert_eq!(pipeline["status"], 0);
        let stage = &pipeline["stages"][0];
        assert_eq!(stage["index"], 1);
        assert_eq!(stage["command"], "printf");
        assert_eq!(stage["state"], "exited");
        assert_eq!(stage["status"], 7);
        let stream = &json["streams"][0];
        for key in [
            "id",
            "pipeline",
            "producer_stage",
            "consumer_stage",
            "producer",
            "consumer",
            "uri",
            "open",
            "total_bytes",
            "retained_start",
            "retained_end",
            "close_reason",
        ] {
            assert!(stream.get(key).is_some(), "missing stream key {key}");
        }
        assert_eq!(stream["uri"], stream_uri(run.id(), 1));
        assert_eq!(stream["producer_stage"], 1);
        assert_eq!(stream["consumer_stage"], 2);
        // 快照只含元数据，绝不携带阶段正文。
        assert!(stream.get("data").is_none());
        assert!(json["pipelines"][0]["stages"][0].get("data").is_none());
    }

    /// 用真实 brush 解释器验证 observer 接缝：begin/label/pipe/stage_done/finish
    /// 都由解释器触发，且返回的端点确实被上游/下游阶段使用。
    #[test]
    fn pipeline_observer_captures_real_shell_pipeline() {
        use brush_builtins::{BuiltinSet, default_builtins};
        use brush_core::extensions::DefaultShellExtensions;
        use brush_core::{ProfileLoadBehavior, RcLoadBehavior, Shell, SourceInfo};

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .unwrap();
        let store = PipeStore::new();
        let run = unique_run(&store);
        let observer = run.observer();
        runtime.block_on(async move {
            let mut shell = Shell::builder()
                .builtins(default_builtins::<DefaultShellExtensions>(
                    BuiltinSet::BashMode,
                ))
                .profile(ProfileLoadBehavior::Skip)
                .rc(RcLoadBehavior::Skip)
                .command_string_mode(true)
                .build()
                .await
                .expect("creating shell");
            shell.start_command_string_mode();
            let mut params = shell.default_exec_params();
            params.set_pipeline_observer(observer);
            let source = SourceInfo::from("-c");
            let result = shell
                .run_string("printf 'hi\\n' | cat", &source, &params)
                .await
                .expect("running pipeline");
            assert_eq!(u8::from(result.exit_code), 0);
            let _ = shell.end_command_string_mode();
        });

        let info = run.finish(RunEnd::exited(Some(0)));
        assert_eq!(info.pipelines.len(), 1);
        let pipeline = &info.pipelines[0];
        assert_eq!(pipeline.stage_count, 2);
        assert_eq!(pipeline.status, Some(0));
        assert_eq!(
            pipeline
                .stages
                .iter()
                .map(|stage| (stage.index, stage.state, stage.status))
                .collect::<Vec<_>>(),
            vec![
                (1, StageState::Exited, Some(0)),
                (2, StageState::Exited, Some(0))
            ]
        );
        assert_eq!(pipeline.stages[0].command, "printf 'hi\\n'");
        assert_eq!(pipeline.stages[1].command, "cat");
        assert_eq!(info.streams.len(), 1);
        let stream = &info.streams[0];
        assert_eq!((stream.producer_stage, stream.consumer_stage), (1, 2));
        assert_eq!(stream.close_reason.as_deref(), Some("eof"));
        assert_eq!(stream.total_bytes, 3);
        let read = store
            .read(run.id(), stream.id, None)
            .expect("read captured bytes");
        assert_eq!(read.data, b"hi\n");
        assert!(!read.open);
    }
}
