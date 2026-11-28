//! A single observed pipe edge (`StreamState`) and its read snapshot types.

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

/// One observed pipe edge. The id increases monotonically inside a run and is never reused.
pub(crate) struct StreamState {
    pub(super) id: u64,
    pub(super) pipeline: u64,
    pub(super) producer_stage: usize,
    consumer_stage: usize,
    producer: String,
    consumer: String,
    pub(super) ring: Mutex<Ring>,
    /// `None` means still open; once sealed, the real reason is kept. Open state and reason share the
    /// same lock, so a snapshot never reads the intermediate "closed but reason missing" state.
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

    /// Append bytes actually observed by the relay thread. The relay guarantees mirroring before forwarding.
    pub(crate) fn observe(&self, data: &[u8]) {
        if data.is_empty() || !self.is_open() {
            return;
        }
        lock(&self.ring).push(data);
        self.revision.fetch_add(1, Ordering::Release);
        (self.hook)(StreamEvent::Data);
    }

    /// Seal the stream. Only the first call takes effect; later calls return `false`.
    pub(crate) fn finish(&self, reason: CloseReason) -> bool {
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

    pub(crate) fn close_reason(&self) -> Option<CloseReason> {
        *lock(&self.close)
    }

    pub(super) fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn retained(&self) -> Vec<u8> {
        let ring = lock(&self.ring);
        ring.read(ring.start(), ring.end())
    }

    pub(crate) fn retained_range(&self) -> (u64, u64) {
        let ring = lock(&self.ring);
        (ring.start(), ring.end())
    }

    /// Build a consistent retained range, total observed bytes and body within a single lock hold.
    fn retained_snapshot(&self) -> (u64, u64, u64) {
        let ring = lock(&self.ring);
        (ring.start(), ring.end(), ring.total)
    }

    /// Single-lock read: the returned `start`/`end` and `data` always come from the same snapshot.
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
        let close = self.close_reason();
        let (start, end, total) = self.retained_snapshot();
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
pub(crate) fn detached_stream(id: u64) -> Arc<StreamState> {
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

/// Consistent snapshot of one stream read (range and body come from the same ring lock).
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

/// Snapshot of one `read`: body plus that stream's retained range, total observed bytes and revision.
#[derive(Clone, Debug)]
pub struct StreamRead {
    /// Absolute start offset in the stream of the body returned this time (clamped to the current
    /// retained range).
    pub start: u64,
    /// Absolute end offset of the body returned this time, equal to the total bytes observed so far.
    pub end: u64,
    pub data: Vec<u8>,
    pub open: bool,
    pub revision: u64,
    pub total_bytes: u64,
    /// The requested start is earlier than the retained range: the prefix has been overwritten.
    pub prefix_evicted: bool,
}
