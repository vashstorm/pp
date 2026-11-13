//! 流式请求的 token 速度采样与显示快照。

use super::*;

const TICK_INTERVAL: Duration = Duration::from_millis(250);
const SPEED_WINDOW: Duration = Duration::from_secs(2);

/// 当前流式请求的输出速度：生成中按已观测字节估算，完成后用服务端 usage 校正。
#[derive(Debug, Default)]
pub(in crate::tui) struct TokenSpeed {
    started_at: Option<Instant>,
    output_bytes: u64,
    /// 显示快照只在采样或请求完成时更新，重绘不重新计算。
    displayed_rate: Option<(f64, bool)>,
    /// 按刷新节奏保存累计字节基线，避免每个 delta 分配样本。
    samples: VecDeque<(Instant, u64)>,
    last_tick: Option<Instant>,
}

impl TokenSpeed {
    pub(in crate::tui) fn start(&mut self, now: Instant) {
        *self = Self {
            started_at: Some(now),
            // 等待下一轮和工具执行期间保留上一笔速度，直到有新的有效采样。
            displayed_rate: self.displayed_rate,
            ..Self::default()
        };
    }

    /// 本次请求截至当前的累计输出字节数。
    pub(in crate::tui) fn progress(&mut self, output_bytes: u64, now: Instant) {
        if self.started_at.is_some() && output_bytes > self.output_bytes {
            if self.samples.is_empty() {
                // 从首个输出开始计时，排除建连、排队和限流等待。
                self.samples.push_back((now, 0));
                self.last_tick = Some(now);
            }
            self.output_bytes = output_bytes;
        }
    }

    /// 结束计时：成功且有服务端结果时显示权威速率，否则保留或丢弃估算。
    pub(in crate::tui) fn finish(
        &mut self,
        elapsed_ms: u64,
        output_tokens: Option<u64>,
        success: bool,
    ) {
        let elapsed_secs = elapsed_ms as f64 / 1000.0;
        self.started_at = None;
        self.last_tick = None;
        self.samples.clear();
        self.displayed_rate = if !success || elapsed_secs <= 0.0 {
            None
        } else if let Some(tokens) = output_tokens {
            Some((tokens as f64 / elapsed_secs, false))
        } else {
            estimate_rate(self.output_bytes, elapsed_secs)
                .map(|rate| (rate, true))
                .or(self.displayed_rate)
        };
    }

    pub(in crate::tui) fn clear(&mut self) {
        *self = Self::default();
    }

    /// 每 250ms 更新约两秒窗口内的估速，采样数量与 delta 频率无关。
    pub(in crate::tui) fn tick(&mut self, now: Instant) -> bool {
        if self.started_at.is_none() || self.output_bytes == 0 {
            return false;
        }
        match self.last_tick {
            Some(last) if now.saturating_duration_since(last) < TICK_INTERVAL => false,
            _ => {
                self.last_tick = Some(now);
                let cutoff = now.checked_sub(SPEED_WINDOW).unwrap_or(now);
                while self.samples.len() > 1 && self.samples[1].0 <= cutoff {
                    self.samples.pop_front();
                }
                let Some(&(baseline, bytes)) = self.samples.front() else {
                    return false;
                };
                let elapsed = now.saturating_duration_since(baseline).as_secs_f64();
                let rate = (self.output_bytes.saturating_sub(bytes) as f64 / 3.0) / elapsed;
                self.samples.push_back((now, self.output_bytes));
                let next = Some((rate, true));
                let changed = self.displayed_rate != next;
                self.displayed_rate = next;
                changed
            }
        }
    }

    /// 返回缓存速率；`bool` 表示是否为估算值。
    pub(in crate::tui) fn value(&self) -> Option<(f64, bool)> {
        self.displayed_rate
    }

    /// 生成中按项目统一的 `/3` 字节启发式估算本次请求的输出 token；
    /// 请求结束后返回 `None`，交给服务端 usage 校正。
    pub(in crate::tui) fn live_output_tokens(&self) -> Option<u64> {
        (self.started_at.is_some() && self.output_bytes > 0).then(|| self.output_bytes.div_ceil(3))
    }

    pub(in crate::tui) fn is_active(&self) -> bool {
        self.started_at.is_some()
    }
}

/// 沿用项目 `/3` 的字节启发式；首个 token 等待时间计入分母。
fn estimate_rate(output_bytes: u64, elapsed_secs: f64) -> Option<f64> {
    if output_bytes == 0 || elapsed_secs <= 0.0 {
        return None;
    }
    Some(output_bytes.div_ceil(3) as f64 / elapsed_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_speed_samples_from_first_output_and_caches_between_ticks() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        let first = base + Duration::from_secs(10);
        assert!(!speed.tick(first));
        assert_eq!(speed.value(), None);
        speed.progress(75, first);
        assert!(!speed.tick(first + Duration::from_millis(249)));
        assert!(speed.tick(first + TICK_INTERVAL));
        assert_eq!(speed.value(), Some((100.0, true)), "不计首字节等待");
        speed.progress(150, first + Duration::from_millis(300));
        speed.progress(90, first + Duration::from_millis(310));
        assert_eq!(speed.value(), Some((100.0, true)), "delta 不改变显示快照");
        assert!(!speed.tick(first + Duration::from_millis(400)));
        speed.tick(first + Duration::from_millis(500));
        assert_eq!(speed.value(), Some((100.0, true)), "累计字节不能倒退");
    }

    #[test]
    fn token_speed_window_tracks_recent_output_and_bounds_samples() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        for i in 0..=40 {
            let now = base + TICK_INTERVAL * i;
            // 前两秒每秒 100 token，之后每秒 200 token。
            let bytes = if i <= 8 {
                75 * (i + 1)
            } else {
                675 + 150 * (i - 8)
            };
            speed.progress(u64::from(bytes), now);
            speed.tick(now);
            assert!(speed.samples.len() <= 10, "窗口样本必须有界");
        }
        assert_eq!(speed.value(), Some((200.0, true)), "旧速率退出窗口");
        let stopped = base + Duration::from_secs(10);
        for i in 1..=9 {
            speed.tick(stopped + TICK_INTERVAL * i);
        }
        assert_eq!(
            speed.value(),
            Some((0.0, true)),
            "停流后显示零，不能悬挂旧速率"
        );
    }

    #[test]
    fn token_speed_finish_uses_authoritative_usage_and_stops_ticks() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        speed.progress(600, base);
        speed.finish(2000, Some(300), true);
        assert!(!speed.is_active());
        assert_eq!(speed.value(), Some((150.0, false)));
        assert!(!speed.tick(base + Duration::from_secs(60)));
        assert_eq!(speed.value(), Some((150.0, false)));
    }

    #[test]
    fn token_speed_short_request_without_usage_keeps_final_estimate() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        speed.progress(600, base);
        speed.finish(2000, None, true);
        assert!(!speed.is_active());
        assert_eq!(speed.value(), Some((100.0, true)));
        // 没有新数据的成功请求不抹掉上一笔速度。
        speed.start(base);
        speed.finish(2000, None, true);
        assert_eq!(speed.value(), Some((100.0, true)));
    }

    #[test]
    fn token_speed_zero_elapsed_or_cancel_has_no_rate() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        speed.progress(600, base);
        assert_eq!(speed.value(), None, "零耗时不产生 inf/NaN");
        speed.finish(0, Some(300), true);
        assert!(!speed.is_active());
        assert_eq!(speed.value(), None);
        speed.start(base);
        speed.progress(600, base);
        speed.tick(base + TICK_INTERVAL);
        speed.finish(500, Some(300), false);
        assert!(!speed.is_active());
        assert_eq!(speed.value(), None);
    }

    #[test]
    fn token_speed_next_request_retains_display_but_resets_samples() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        speed.progress(600, base);
        speed.finish(2000, Some(300), true);
        let second = base + Duration::from_secs(5);
        speed.start(second);
        assert!(speed.is_active());
        assert!(!speed.tick(second + Duration::from_secs(1)));
        assert_eq!(speed.value(), Some((150.0, false)), "等待时保留上一笔速度");
        speed.progress(75, second + Duration::from_secs(2));
        assert_eq!(speed.value(), Some((150.0, false)), "采样未满时不闪回未知");
        speed.tick(second + Duration::from_millis(2250));
        assert_eq!(speed.value(), Some((100.0, true)), "仅计算当前请求字节");
        speed.clear();
        assert!(!speed.is_active());
        assert_eq!(speed.value(), None);
    }

    #[test]
    fn token_speed_live_output_grows_with_bytes_and_stops_at_usage() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        assert_eq!(speed.live_output_tokens(), None, "未开始请求时不估算");
        speed.start(base);
        assert_eq!(speed.live_output_tokens(), None, "尚无输出时保留上一个数字");
        speed.progress(600, base + Duration::from_millis(10));
        assert_eq!(speed.live_output_tokens(), Some(200));
        speed.progress(900, base + Duration::from_millis(20));
        assert_eq!(speed.live_output_tokens(), Some(300), "随已接收字节增长");
        speed.finish(1000, Some(280), true);
        assert_eq!(
            speed.live_output_tokens(),
            None,
            "完成后必须交给权威 usage，而不是保留估算"
        );
        speed.start(base);
        speed.progress(30, base);
        speed.finish(1000, None, false);
        assert_eq!(speed.live_output_tokens(), None, "失败或取消不保留估算");
    }
}
