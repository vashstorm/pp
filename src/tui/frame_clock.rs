//! 所有正常画面共用帧截止时间；输入处理不等待绘制。
use std::time::{Duration, Instant};

pub(super) struct FrameClock {
    interval: Duration,
    next: Option<Instant>,
}
impl FrameClock {
    pub fn new(fps: u16) -> Self {
        Self {
            interval: Duration::from_secs_f64(1.0 / f64::from(fps)),
            next: None,
        }
    }
    pub fn ready(&self, now: Instant) -> bool {
        self.next.is_none_or(|next| now >= next)
    }
    pub fn started(&mut self, now: Instant) {
        self.next = Some(now + self.interval);
    }
    pub fn remaining(&self, now: Instant) -> Duration {
        self.next
            .map_or(Duration::ZERO, |next| next.saturating_duration_since(now))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_clock_caps_all_dirty_sources_without_catchup() {
        for fps in [30, 60, 120] {
            let mut clock = FrameClock::new(fps);
            let mut now = Instant::now();
            assert!(clock.ready(now));
            for _ in 0..20 {
                clock.started(now);
                for _source in ["input", "stream", "notification", "history"] {
                    assert!(!clock.ready(now + clock.interval - Duration::from_nanos(1)));
                }
                now += clock.interval;
                assert!(clock.ready(now));
            }
            now += Duration::from_secs(1);
            clock.started(now);
            assert!(!clock.ready(now));
            assert_eq!(clock.remaining(now), clock.interval);
        }
    }
}
