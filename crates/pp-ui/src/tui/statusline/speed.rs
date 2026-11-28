//! Token speed sampling and display snapshot for streaming requests.

use super::*;

const TICK_INTERVAL: Duration = Duration::from_millis(250);
const SPEED_WINDOW: Duration = Duration::from_secs(2);

/// Output speed of the current streaming request: estimated from observed bytes while generating, corrected from server usage when done.
#[derive(Debug, Default)]
pub(in crate::tui) struct TokenSpeed {
    started_at: Option<Instant>,
    output_bytes: u64,
    /// The display snapshot updates only on a sample or request completion; repainting does not recompute.
    displayed_rate: Option<(f64, bool)>,
    /// Keep the accumulated-byte baseline at the refresh cadence so no sample is allocated per delta.
    samples: VecDeque<(Instant, u64)>,
    last_tick: Option<Instant>,
}

impl TokenSpeed {
    pub(in crate::tui) fn start(&mut self, now: Instant) {
        *self = Self {
            started_at: Some(now),
            // Keep the previous speed while waiting for the next turn or tool execution, until a new valid sample arrives.
            displayed_rate: self.displayed_rate,
            ..Self::default()
        };
    }

    /// Accumulated output bytes for this request so far.
    pub(in crate::tui) fn progress(&mut self, output_bytes: u64, now: Instant) {
        if self.started_at.is_some() && output_bytes > self.output_bytes {
            if self.samples.is_empty() {
                // Start timing at the first output, excluding connect, queueing and rate-limit waits.
                self.samples.push_back((now, 0));
                self.last_tick = Some(now);
            }
            self.output_bytes = output_bytes;
        }
    }

    /// Finish timing: show the authoritative rate on success with a server result, otherwise keep or drop the estimate.
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

    /// Update the estimate over a roughly two-second window every 250ms; the sample count does not depend on delta frequency.
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

    /// Return the cached rate; `bool` says whether it is an estimate.
    pub(in crate::tui) fn value(&self) -> Option<(f64, bool)> {
        self.displayed_rate
    }

    /// While generating, estimate this request's output tokens with the project-wide `/3` byte heuristic;
    /// once the request ends it returns `None`, leaving correction to the server usage.
    pub(in crate::tui) fn live_output_tokens(&self) -> Option<u64> {
        (self.started_at.is_some() && self.output_bytes > 0).then(|| self.output_bytes.div_ceil(3))
    }

    pub(in crate::tui) fn is_active(&self) -> bool {
        self.started_at.is_some()
    }
}

/// Uses the project's `/3` byte heuristic; time-to-first-token counts toward the denominator.
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
    fn token_speed_sampling_window_and_live_tokens() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        assert_eq!(speed.live_output_tokens(), None);
        speed.start(base);
        assert_eq!(speed.live_output_tokens(), None);
        let first = base + Duration::from_secs(10);
        assert!(!speed.tick(first));
        assert_eq!(speed.value(), None);
        speed.progress(75, first);
        assert!(!speed.tick(first + Duration::from_millis(249)));
        assert!(speed.tick(first + TICK_INTERVAL));
        assert_eq!(
            speed.value(),
            Some((100.0, true)),
            "first-byte wait is not counted"
        );
        speed.progress(150, first + Duration::from_millis(300));
        speed.progress(90, first + Duration::from_millis(310));
        assert_eq!(speed.value(), Some((100.0, true)));
        assert!(!speed.tick(first + Duration::from_millis(400)));
        speed.tick(first + Duration::from_millis(500));
        assert_eq!(speed.value(), Some((100.0, true)));

        let mut win = TokenSpeed::default();
        win.start(base);
        for i in 0..=40 {
            let now = base + TICK_INTERVAL * i;
            let bytes = if i <= 8 {
                75 * (i + 1)
            } else {
                675 + 150 * (i - 8)
            };
            win.progress(u64::from(bytes), now);
            win.tick(now);
            assert!(win.samples.len() <= 10);
        }
        assert_eq!(win.value(), Some((200.0, true)));
        let stopped = base + Duration::from_secs(10);
        for i in 1..=9 {
            win.tick(stopped + TICK_INTERVAL * i);
        }
        assert_eq!(win.value(), Some((0.0, true)));

        let mut live = TokenSpeed::default();
        live.start(base);
        live.progress(600, base + Duration::from_millis(10));
        assert_eq!(live.live_output_tokens(), Some(200));
        live.progress(900, base + Duration::from_millis(20));
        assert_eq!(live.live_output_tokens(), Some(300));
        live.finish(1000, Some(280), true);
        assert_eq!(live.live_output_tokens(), None);
        live.start(base);
        live.progress(30, base);
        live.finish(1000, None, false);
        assert_eq!(live.live_output_tokens(), None);
    }

    #[test]
    fn token_speed_finish_usage_and_short_requests() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        speed.progress(600, base);
        speed.finish(2000, Some(300), true);
        assert!(!speed.is_active() && speed.value() == Some((150.0, false)));
        assert!(!speed.tick(base + Duration::from_secs(60)));
        assert_eq!(speed.value(), Some((150.0, false)));

        let mut short = TokenSpeed::default();
        short.start(base);
        short.progress(600, base);
        short.finish(2000, None, true);
        assert!(!short.is_active() && short.value() == Some((100.0, true)));
        short.start(base);
        short.finish(2000, None, true);
        assert_eq!(short.value(), Some((100.0, true)));
    }

    #[test]
    fn token_speed_zero_cancel_and_next_request() {
        let base = Instant::now();
        let mut speed = TokenSpeed::default();
        speed.start(base);
        speed.progress(600, base);
        assert_eq!(speed.value(), None);
        speed.finish(0, Some(300), true);
        assert!(!speed.is_active() && speed.value().is_none());
        speed.start(base);
        speed.progress(600, base);
        speed.tick(base + TICK_INTERVAL);
        speed.finish(500, Some(300), false);
        assert!(!speed.is_active() && speed.value().is_none());

        let mut next = TokenSpeed::default();
        next.start(base);
        next.progress(600, base);
        next.finish(2000, Some(300), true);
        let second = base + Duration::from_secs(5);
        next.start(second);
        assert!(next.is_active() && !next.tick(second + Duration::from_secs(1)));
        assert_eq!(next.value(), Some((150.0, false)));
        next.progress(75, second + Duration::from_secs(2));
        assert_eq!(next.value(), Some((150.0, false)));
        next.tick(second + Duration::from_millis(2250));
        assert_eq!(next.value(), Some((100.0, true)));
        next.clear();
        assert!(!next.is_active() && next.value().is_none());
    }
}
