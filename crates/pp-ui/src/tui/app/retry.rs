//! 自动重试等待期的固定信息框：只在输入框上方展示，不进入对话与历史。
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
    /// 刚刚失败的尝试序号（从 1 开始）。
    attempt: u32,
    deadline: Instant,
    /// 剩余秒数向上取整，同一秒内保持显示稳定。
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

    /// 请求已重新开始、收尾或取消：立刻收起固定信息框。
    pub(in crate::tui) fn retry_finished(&mut self) {
        self.retry_notice = None;
    }

    /// 推进倒计时；返回是否需要重绘。
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

    /// 固定信息框：与对话区分离，不随滚动移出视口。
    pub(in crate::tui) fn draw_retry_notice(&self, frame: &mut Frame, area: Rect) {
        let Some(notice) = &self.retry_notice else {
            return;
        };
        if area.width < 4 || area.height == 0 {
            return;
        }
        let accent = Style::default().fg(transcript_style::ACCENT);
        let block = Block::bordered()
            .border_style(accent)
            .title(Line::styled(" Retrying ", accent));
        let text = Line::styled(
            format!(" {}", notice.line()),
            Style::default().fg(transcript_style::TEXT),
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
