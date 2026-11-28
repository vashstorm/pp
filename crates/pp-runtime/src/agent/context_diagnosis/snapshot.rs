//! Reports are produced on the Agent thread; the UI only reads the in-memory snapshot of the latest stable boundary.
//!
//! The classification cache and the report handle live together in `Snapshot` (owned by the Agent, no process-wide static):
//! the publishing path reuses classified prefixes by fingerprint, and `invalidate`/a diagnosis failure drops the cache and forces a recompute.

use super::{Agent, ContextReport, DiagnosisCache};
use pp_core::context::ContextUsage;
use std::sync::{Arc, Mutex};

type Report = Result<Arc<ContextReport>, String>;

#[derive(Clone, Default)]
pub struct Snapshot {
    report: Arc<Mutex<Option<Report>>>,
    cache: Arc<Mutex<DiagnosisCache>>,
}

impl Snapshot {
    /// Copies only the report handle; waits for no main task and performs no diagnosis computation or I/O.
    pub fn read(&self) -> Option<Report> {
        self.report
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Drop the cached classified prefix; the next publish forces a full recompute.
    pub fn invalidate(&self) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate();
    }
}

impl Agent {
    /// Enable the snapshot for the TUI; line mode and subagents produce no extra diagnostic report by default.
    pub fn observe_context(&mut self) -> Snapshot {
        let snapshot = Snapshot::default();
        self.context_snapshot = Some(snapshot.clone());
        snapshot
    }

    /// Call only on the Agent's own thread; a diagnosis failure does not change the task result.
    pub fn publish_context_snapshot(&self) {
        self.publish_context_snapshot_with_usage(None);
    }

    /// Reuse the usage already computed before this turn's request; when `None`, the publishing path re-estimates.
    ///
    /// Callers must pass `None` when history changed after the usage estimate (for example a supplementary developer message).
    pub fn publish_context_snapshot_with_usage(&self, usage: Option<ContextUsage>) {
        let Some(snapshot) = &self.context_snapshot else {
            return;
        };
        let report = {
            let mut cache = snapshot.cache.lock().unwrap_or_else(|e| e.into_inner());
            self.diagnose_context_with(&mut cache, usage)
        }
        .map(Arc::new)
        .map_err(|error| {
            // On failure keep no partial cache, so the next publish does not reuse a prefix related to the error.
            snapshot.invalidate();
            pp_core::text::display_text(&format!("{error:#}")).into_owned()
        });
        *snapshot.report.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
    }
}
