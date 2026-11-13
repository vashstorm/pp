//! 单条被观察管道边（`StreamState`）及其读取快照类型。

use serde::Serialize;
#[cfg(test)]
use std::sync::Arc;
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};

use super::observer::truncate_label;
use super::ring::{CloseReason, Ring, StreamEvent, StreamHook, lock};
use super::store::stream_uri;

/// 一条被观察的管道边。id 在 run 内单调递增且永不复用。
pub(in crate::tools::bash) struct StreamState {
    pub(super) id: u64,
    pub(super) pipeline: u64,
    pub(super) producer_stage: usize,
    consumer_stage: usize,
    producer: String,
    consumer: String,
    pub(super) ring: Mutex<Ring>,
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
    pub(in crate::tools::bash) fn observe(&self, data: &[u8]) {
        if data.is_empty() || !self.is_open() {
            return;
        }
        lock(&self.ring).push(data);
        self.revision.fetch_add(1, Ordering::Release);
        (self.hook)(StreamEvent::Data);
    }

    /// 封存该流。只有第一次调用生效，之后返回 `false`。
    pub(in crate::tools::bash) fn finish(&self, reason: CloseReason) -> bool {
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

    pub(in crate::tools::bash) fn close_reason(&self) -> Option<CloseReason> {
        *lock(&self.close)
    }

    pub(super) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(in crate::tools::bash) fn retained(&self) -> Vec<u8> {
        let ring = lock(&self.ring);
        ring.read(ring.start(), ring.end())
    }

    pub(in crate::tools::bash) fn retained_range(&self) -> (u64, u64) {
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

    pub(super) fn meta(&self, run_id: &str) -> StreamMeta {
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
pub(in crate::tools::bash) fn detached_stream(id: u64) -> Arc<StreamState> {
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

/// 一次流读取的一致快照（区间与正文来自同一把 ring 锁）。
pub(super) struct StreamSlice {
    pub(super) start: u64,
    pub(super) end: u64,
    pub(super) data: Vec<u8>,
    pub(super) prefix_evicted: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct StreamMeta {
    pub id: u64,
    pub pipeline: u64,
    pub producer_stage: usize,
    pub consumer_stage: usize,
    pub producer: String,
    pub consumer: String,
    pub(crate) uri: String,
    pub open: bool,
    pub total_bytes: u64,
    pub retained_start: u64,
    pub retained_end: u64,
    pub revision: u64,
    pub close_reason: Option<String>,
}

/// 一次 `read` 的快照：正文 + 该流的保留区间、总观察字节与 revision。
#[derive(Clone, Debug)]
pub struct StreamRead {
    /// 本次返回正文在流中的绝对起始偏移（已夹到当前保留区间）。
    pub start: u64,
    /// 本次返回正文的绝对结束偏移，等于此刻观察到的总字节数。
    pub end: u64,
    pub data: Vec<u8>,
    pub open: bool,
    pub(crate) revision: u64,
    pub total_bytes: u64,
    /// 请求起点早于保留区间：前缀已被覆盖。
    pub prefix_evicted: bool,
}
