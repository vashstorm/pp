//! 分页观察日志的增量接入；不把页边界当作运行结束。

use super::*;

/// 将已有时间戳映射到本进程单调时钟，缺失、未来或越界的值保持未知。
fn recorded_start(timestamp_ms: Option<u64>, now_ms: u64, now: Instant) -> Option<Instant> {
    now.checked_sub(Duration::from_millis(now_ms.checked_sub(timestamp_ms?)?))
}

impl Transcript {
    pub(in crate::tui) fn begin_recorded_run(
        &mut self,
        label: &str,
        timestamp_ms: Option<u64>,
        now_ms: u64,
        now: Instant,
    ) {
        let mut activity = Activity::new(now);
        activity.label = label.into();
        activity.started = recorded_start(timestamp_ms, now_ms, now);
        activity.seconds = activity
            .started
            .map(|started| now.duration_since(started).as_secs());
        activity.refresh();
        self.activity = Some(activity);
    }

    /// 历史与日志尾部共用卡片更新；完成工具不生成基于导入速度的 Wall。
    pub(in crate::tui) fn progress_recorded(
        &mut self,
        event: AgentEvent,
        timestamp_ms: Option<u64>,
        now_ms: u64,
        now: Instant,
    ) {
        self.progress_phase(&event);
        let started = matches!(event, AgentEvent::ToolStarted { .. }).then_some(self.entries.len());
        let replaying = self.replaying;
        self.replaying = true;
        self.progress(event);
        self.replaying = replaying;
        if let Some(index) = started {
            let entry = &mut self.entries[index];
            entry.started = recorded_start(timestamp_ms, now_ms, now);
            if entry.kind == EntryKind::Bash {
                entry.update_bash_footer(entry.started.map(|started| now.duration_since(started)));
            }
        }
    }

    /// 取消和失败也按历史语义收尾，避免给未完成的工具补造耗时。
    pub(in crate::tui) fn stop_recorded_run(&mut self, cancelled: bool) {
        let replaying = self.replaying;
        self.replaying = true;
        if cancelled {
            self.cancel();
        } else {
            self.fail();
        }
        self.replaying = replaying;
    }
}
