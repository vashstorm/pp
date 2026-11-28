//! Incremental ingestion of the paginated observation log; a page boundary is not treated as the end of a run.

use super::*;

/// Map an existing timestamp onto this process's monotonic clock; missing, future or out-of-range values stay unknown.
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
        self.background_bash.clear();
        let mut activity = Activity::new(now);
        activity.label = label.into();
        activity.started = recorded_start(timestamp_ms, now_ms, now);
        activity.seconds = activity
            .started
            .map(|started| now.duration_since(started).as_secs());
        self.activity = Some(activity);
    }

    /// History and the log tail share card updates; a completed tool produces no wall time derived from import speed.
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
        if let Some(entry) = started.and_then(|index| self.entries.get_mut(index)) {
            entry.started = recorded_start(timestamp_ms, now_ms, now);
            if entry.kind == EntryKind::Bash {
                entry.update_bash_footer(entry.started.map(|started| now.duration_since(started)));
            } else if entry.kind == EntryKind::Eval {
                entry.update_eval_footer(entry.started.map(|started| now.duration_since(started)));
            }
        }
    }

    /// Cancellation and failure also close out with history semantics so unfinished tools are not given fabricated elapsed time.
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
