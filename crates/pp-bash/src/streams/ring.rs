//! Single-stream ring buffer, close reasons and stream event hooks.

use std::sync::{Arc, Mutex, MutexGuard};

use super::STREAM_RING_CAP;

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseReason {
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

/// Stream internal state change events; used to tie the run-level revision and closed-stream pruning together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StreamEvent {
    Data,
    Closed,
}

pub(super) type StreamHook = Arc<dyn Fn(StreamEvent) + Send + Sync>;

/// Fixed 1 MiB tail ring buffer per stream; absolute offsets start at 0 and only grow.
///
/// `body` is allocated once to `STREAM_RING_CAP` on the first write and its length never changes
/// afterwards: `head` indexes the oldest retained byte and `filled` is the retained byte count. Writes
/// past capacity only overwrite the slots about to be evicted and advance `head`, leaving the retained
/// body in place, so `push` costs O(written length) regardless of the retained size (the old `drain`
/// implementation moved all remaining bytes).
///
/// Capacity is reserved as virtual memory: `vec![0; CAP]` only touches pages actually written, so a
/// small output stream does not cost 1 MiB of resident memory; the session-wide total is still bounded
/// by `MAX_TOTAL_STREAMS`.
#[derive(Default)]
pub(super) struct Ring {
    body: Vec<u8>,
    head: usize,
    filled: usize,
    pub(super) total: u64,
}

impl Ring {
    pub(super) fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if self.body.is_empty() {
            self.body = vec![0u8; STREAM_RING_CAP];
        }
        self.total = self.total.saturating_add(data.len() as u64);
        if data.len() >= STREAM_RING_CAP {
            // A single block already fills the buffer: keep only its tail and move the read cursor back to
            // the start.
            self.body[..STREAM_RING_CAP].copy_from_slice(&data[data.len() - STREAM_RING_CAP..]);
            self.head = 0;
            self.filled = STREAM_RING_CAP;
            return;
        }
        let overflow = (self.filled + data.len()).saturating_sub(STREAM_RING_CAP);
        if overflow > 0 {
            // Eviction only advances the read cursor; the remaining body stays in place.
            self.head = (self.head + overflow) % STREAM_RING_CAP;
            self.filled -= overflow;
        }
        let write = (self.head + self.filled) % STREAM_RING_CAP;
        let first = (STREAM_RING_CAP - write).min(data.len());
        self.body[write..write + first].copy_from_slice(&data[..first]);
        if first < data.len() {
            // The write crosses the physical end, so the remainder wraps back to the start of `body`.
            self.body[..data.len() - first].copy_from_slice(&data[first..]);
        }
        self.filled += data.len();
    }

    fn len(&self) -> u64 {
        self.filled as u64
    }

    pub(super) fn start(&self) -> u64 {
        self.total.saturating_sub(self.len())
    }

    pub(super) const fn end(&self) -> u64 {
        self.total
    }

    pub(super) fn read(&self, from: u64, to: u64) -> Vec<u8> {
        let start = self.start();
        let low = from.clamp(start, self.total);
        let high = to.clamp(low, self.total);
        let offset = (low - start) as usize;
        let count = (high - low) as usize;
        let mut out = Vec::with_capacity(count);
        if count == 0 {
            return out;
        }
        let begin = (self.head + offset) % STREAM_RING_CAP;
        let first = (STREAM_RING_CAP - begin).min(count);
        out.extend_from_slice(&self.body[begin..begin + first]);
        if first < count {
            out.extend_from_slice(&self.body[..count - first]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // For a single block larger than capacity only its tail is kept, and absolute offsets keep
        // accumulating.
        let big = vec![7u8; STREAM_RING_CAP + 100];
        ring.push(&big);
        assert_eq!(ring.start(), payload.len() as u64 + 100);
        assert_eq!(ring.read(ring.start(), ring.end()), big[100..]);
        assert_eq!(ring.len(), STREAM_RING_CAP as u64);
    }

    /// Reference model: keep the last 1 MiB of the concatenated sequence (independent of the ring
    /// buffer's physical layout). Multi-block writes repeatedly cross the read/write wrap points and
    /// cover both a block exactly filling capacity and a single oversized block.
    #[test]
    fn pipeline_stream_ring_wraps_and_matches_tail_model() {
        use std::collections::VecDeque;

        let mut ring = Ring::default();
        let mut model: VecDeque<u8> = VecDeque::new();
        let mut total = 0u64;
        let chunks = [
            1usize,
            8_191,
            STREAM_RING_CAP / 2,
            STREAM_RING_CAP / 3,
            65_537,
            STREAM_RING_CAP,
            STREAM_RING_CAP + 7,
            4_093,
            STREAM_RING_CAP / 5,
        ];
        for chunk in chunks {
            // Each byte is determined by its absolute offset in the stream, so the retained window can be
            // derived independently.
            let data: Vec<u8> = (0..chunk)
                .map(|offset| ((total as usize + offset) % 251) as u8)
                .collect();
            ring.push(&data);
            total += chunk as u64;
            model.extend(data);
            while model.len() > STREAM_RING_CAP {
                model.pop_front();
            }

            assert_eq!(ring.total, total);
            assert_eq!(ring.end(), total);
            assert_eq!(ring.start(), total.saturating_sub(STREAM_RING_CAP as u64));
            assert_eq!(ring.len(), model.len() as u64);

            let retained: Vec<u8> = model.iter().copied().collect();
            // Full-span read: may physically cross the wrap point.
            assert_eq!(ring.read(ring.start(), ring.end()), retained);
            // A start earlier than the retained range is clamped to the range start.
            assert_eq!(ring.read(0, total), retained);

            // The tail window must match the model too (including windows that cross the wrap point).
            let low = ring
                .end()
                .saturating_sub((STREAM_RING_CAP / 3) as u64)
                .max(ring.start());
            let expected: Vec<u8> = model
                .iter()
                .skip((low - ring.start()) as usize)
                .copied()
                .collect();
            assert_eq!(ring.read(low, ring.end()), expected);
            // An empty range and a request past the end are clamped to the current end.
            assert!(ring.read(ring.end(), ring.end()).is_empty());
            assert!(
                ring.read(ring.end() + 10, ring.end() + 20).is_empty(),
                "requests past the end must clamp to an empty slice"
            );
        }
    }
}
