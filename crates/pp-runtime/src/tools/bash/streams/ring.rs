//! 单流环形缓存、关闭原因与流事件钩子。

use std::sync::{Arc, Mutex, MutexGuard};

use super::STREAM_RING_CAP;

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tools::bash) enum CloseReason {
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

pub(super) type StreamHook = Arc<dyn Fn(StreamEvent) + Send + Sync>;

/// 每流固定 1 MiB 的尾部环形缓存；绝对偏移从 0 开始、只增不减。
#[derive(Default)]
pub(super) struct Ring {
    body: Vec<u8>,
    pub(super) total: u64,
}

impl Ring {
    pub(super) fn push(&mut self, data: &[u8]) {
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
        self.body[(low - start) as usize..(high - start) as usize].to_vec()
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

        // 单个超过容量块只保留它的尾部，且绝对偏移继续累加。
        let big = vec![7u8; STREAM_RING_CAP + 100];
        ring.push(&big);
        assert_eq!(ring.start(), payload.len() as u64 + 100);
        assert_eq!(ring.read(ring.start(), ring.end()), big[100..]);
        assert_eq!(ring.len(), STREAM_RING_CAP as u64);
    }
}
