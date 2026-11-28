//! Fixed notice box during automatic retry waits: shown above the input box only and never
//! entering the conversation or history.
use crate::tui::{App, Instant, transcript_style};
use pp_core::events::RequestRetryReason;
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::Line,
    widgets::{Block, Paragraph},
};
use std::time::Duration;

pub(in crate::tui) struct Notice {
    reason: RequestRetryReason,
    /// Sequence number of the attempt that just failed (starting at 1).
    attempt: u32,
    deadline: Instant,
    /// Remaining seconds rounded up, so the display stays stable within one second.
    shown_seconds: u64,
}

impl Notice {
    fn new(reason: RequestRetryReason, attempt: u32, wait: Duration, now: Instant) -> Self {
        let mut notice = Self {
            reason,
            attempt,
            deadline: now + wait,
            shown_seconds: 0,
        };
        notice.shown_seconds = notice.remaining_seconds(now);
        notice
    }

    fn remaining_seconds(&self, now: Instant) -> u64 {
        let remaining = self.deadline.saturating_duration_since(now);
        remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0)
    }

    fn line(&self) -> String {
        format!(
            "attempt {} failed: {}; retrying in {}s · Esc cancels",
            self.attempt, self.reason, self.shown_seconds
        )
    }
}

impl App {
    pub(in crate::tui) fn retry_started(
        &mut self,
        reason: RequestRetryReason,
        attempt: u32,
        wait_ms: u64,
    ) {
        self.retry_notice = Some(Notice::new(
            reason,
            attempt,
            Duration::from_millis(wait_ms),
            Instant::now(),
        ));
    }

    /// The request restarted, finished or was cancelled: collapse the fixed notice box at
    /// once.
    pub(in crate::tui) fn retry_finished(&mut self) {
        self.retry_notice = None;
    }

    /// Advances the countdown; returns whether a redraw is needed.
    pub(in crate::tui) fn retry_tick(&mut self, now: Instant) -> bool {
        let Some(notice) = self.retry_notice.as_ref() else {
            return false;
        };
        let (expired, seconds) = (now >= notice.deadline, notice.remaining_seconds(now));
        if expired {
            self.retry_notice = None;
            return true;
        }
        let Some(notice) = self.retry_notice.as_mut() else {
            return false;
        };
        let changed = notice.shown_seconds != seconds;
        notice.shown_seconds = seconds;
        changed
    }

    pub(in crate::tui) fn retry_visible(&self) -> bool {
        self.retry_notice.is_some()
    }

    /// Fixed notice box: separate from the conversation area and never scrolled out of the
    /// viewport.
    pub(in crate::tui) fn draw_retry_notice(&self, frame: &mut Frame, area: Rect) {
        let Some(notice) = &self.retry_notice else {
            return;
        };
        if area.width < 4 || area.height == 0 {
            return;
        }
        let accent = Style::default().fg(transcript_style::accent());
        let block = Block::bordered()
            .border_style(accent)
            .title(Line::styled(" Retrying ", accent));
        let text = Line::styled(
            format!(" {}", notice.line()),
            Style::default().fg(transcript_style::text()),
        );
        frame.render_widget(Paragraph::new(text).block(block), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_countdown_uses_stable_whole_seconds_at_boundaries() {
        let now = Instant::now();
        let notice = Notice::new(RequestRetryReason::Timeout, 1, Duration::from_secs(2), now);
        assert!(notice.line().contains("retrying in 2s"));
        for (elapsed, seconds) in [
            (Duration::ZERO, 2),
            (Duration::from_millis(100), 2),
            (Duration::from_secs(1) - Duration::from_nanos(1), 2),
            (Duration::from_secs(1), 1),
            (Duration::from_secs(2) - Duration::from_nanos(1), 1),
            (Duration::from_secs(2), 0),
            (Duration::from_secs(3), 0),
        ] {
            assert_eq!(notice.remaining_seconds(now + elapsed), seconds);
        }
    }
}
