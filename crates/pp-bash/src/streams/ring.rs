//! 单流环形缓存、关闭原因与流事件钩子。

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

/// 流内部状态变化事件；用于把 run 级 revision 与已关闭流修剪串起来。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StreamEvent {
    Data,
    Closed,
}

pub(super) type StreamHook = Arc<dyn Fn(StreamEvent) + Send + Sync>;

/// 每流固定 1 MiB 的尾部环形缓存；绝对偏移从 0 开始、只增不减。
///
/// `body` 在首次写入时一次性分配到 `STREAM_RING_CAP` 后长度不再变化：`head`
/// 是最老保留字节的下标，`filled` 是保留字节数。超出容量的写入只覆盖即将
/// 淘汰的槽位并把 `head` 前移，保留正文原地不动，所以 `push` 的代价是
/// O(写入长度)，与已保留正文大小无关（旧实现的 `drain` 要搬移剩余全部字节）。
///
/// 容量按虚拟内存预留：`vec![0; CAP]` 只触碰真正写入的页，小输出流不会带来
/// 1 MiB 的常驻内存；会话级合计上限仍由 `MAX_TOTAL_STREAMS` 约束。
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
            // 单块已填满缓存：只保留它的尾部，读游标回到起点。
            self.body[..STREAM_RING_CAP].copy_from_slice(&data[data.len() - STREAM_RING_CAP..]);
            self.head = 0;
            self.filled = STREAM_RING_CAP;
            return;
        }
        let overflow = (self.filled + data.len()).saturating_sub(STREAM_RING_CAP);
        if overflow > 0 {
            // 淘汰只推进读游标，剩余正文留在原位置。
            self.head = (self.head + overflow) % STREAM_RING_CAP;
            self.filled -= overflow;
        }
        let write = (self.head + self.filled) % STREAM_RING_CAP;
        let first = (STREAM_RING_CAP - write).min(data.len());
        self.body[write..write + first].copy_from_slice(&data[..first]);
        if first < data.len() {
            // 写入跨过物理末端，剩余部分回到 `body` 开头。
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

        // 单个超过容量块只保留它的尾部，且绝对偏移继续累加。
        let big = vec![7u8; STREAM_RING_CAP + 100];
        ring.push(&big);
        assert_eq!(ring.start(), payload.len() as u64 + 100);
        assert_eq!(ring.read(ring.start(), ring.end()), big[100..]);
        assert_eq!(ring.len(), STREAM_RING_CAP as u64);
    }

    /// 参考模型：保留拼接序列的最后 1 MiB（与环形缓存的物理布局无关）。
    /// 多块写入反复跨过读/写游标的回绕点，并覆盖刚好满容量、单块越界两种边界。
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
            // 每个字节由它在流中的绝对偏移决定，所以保留窗口可独立推导。
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
            // 整段读取：物理上可能跨越回绕点。
            assert_eq!(ring.read(ring.start(), ring.end()), retained);
            // 起点早于保留区间时从保留区间起点开始。
            assert_eq!(ring.read(0, total), retained);

            // 尾部窗口同样与模型一致（含跨回绕点的窗口）。
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
            // 空区间与超出末端的请求夹到当前末端。
            assert!(ring.read(ring.end(), ring.end()).is_empty());
            assert!(
                ring.read(ring.end() + 10, ring.end() + 20).is_empty(),
                "requests past the end must clamp to an empty slice"
            );
        }
    }
}
