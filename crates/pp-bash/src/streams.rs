//! Pipe stage mirroring and temporary body retention (`PipeStore`).
//!
//! This module is the storage layer of plan §2.4–2.6 and §3.1–3.4: each bash call creates a run, and
//! the multi-stage pipelines, per-edge stage states and real bytes flowing through them are kept in a
//! bounded registry shared by the root session. It does not depend on interpreter implementation
//! details; it only implements [`brush_core::PipelineObserver`] and creates in-process pipes through
//! [`relay`](super::relay).
//!
//! Bounded rules (all internal constants, not exposed as settings):
//! - each stream keeps only the newest 1 MiB of raw bytes;
//! - at most 16 cached streams per run, keeping only the latest 4 closed streams;
//! - the root session holds at most 64 cached streams / 64 MiB of body and the latest 8 completed runs
//!   that have pipelines;
//! - each run keeps the latest 16 pipeline occurrences with at most 32 stage details each;
//! - evicting details never loses the fact that a non-zero stage was observed; counts keep
//!   accumulating;
//! - eviction prefers the oldest completed run / oldest closed stream and never evicts open streams;
//!   when no slot can be evicted the edge is simply not captured (`untapped_edges`) and the command
//!   still runs as a normal pipeline.

/// Retained body limit for a single stream.
pub(crate) const STREAM_RING_CAP: usize = 1024 * 1024;
const MAX_STREAMS_PER_RUN: usize = 16;
const MAX_CLOSED_STREAMS_PER_RUN: usize = 4;
const MAX_TOTAL_STREAMS: usize = 64;
const MAX_TOTAL_BODY: usize = 64 * 1024 * 1024;
/// Retention limit for completed runs that still have pipelines; background-task regressions also
/// build cases against this boundary.
pub(super) const MAX_COMPLETED_RUNS: usize = 8;
/// Completed runs without pipelines remain visible to `/pipes`/status queries (for example when host
/// startup failed), but they are bounded separately and never crowd out the "latest 8 runs with
/// pipelines" history.
const MAX_EMPTY_RUNS: usize = 8;
const MAX_PIPELINES_PER_RUN: usize = 16;
const MAX_STAGES_PER_PIPELINE: usize = 32;
const MAX_LABEL_BYTES: usize = 512;
/// Cards copy only a small recent body fragment so each refresh does not copy the whole stream cache.
const PREVIEW_BYTES: u64 = 50 * 1024;

/// Scheme of temporary pipe URIs.
pub(crate) const PIPE_URI_PREFIX: &str = "pipe://";
/// Fixed hint for unknown, evicted or other-session streams; an empty stream must never masquerade as
/// this state.
pub(crate) const PIPES_UNAVAILABLE: &str =
    "Temporary pipe output is unavailable (evicted or from another session).";

mod observer;
mod ring;
mod run;
mod store;
mod stream;

pub use observer::PipeRun;
pub(super) use ring::CloseReason;
pub(crate) use run::RunEnd;
pub use store::PipeStore;
pub(super) use stream::StreamState;
#[cfg(test)]
pub(super) use stream::detached_stream;

#[cfg(test)]
mod fixtures {
    use brush_core::openfiles::OpenFile;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::observer::PipeRun;
    use super::store::{PipeStore, PipeStoreError};

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    static NEXT: AtomicU64 = AtomicU64::new(1);

    pub(super) fn unique_run(store: &PipeStore) -> PipeRun {
        loop {
            let id = format!("run{:032x}", NEXT.fetch_add(1, Ordering::Relaxed));
            match store.start_run(id, "test") {
                Ok(run) => return run,
                Err(PipeStoreError::DuplicateRun) => {}
                Err(error) => panic!("unexpected start_run error: {error}"),
            }
        }
    }

    pub(super) fn edge(run: &PipeRun, pipeline: u64, index: usize) -> (OpenFile, OpenFile) {
        run.observer()
            .pipe(pipeline, index, "printf", "cat")
            .expect("capture edge")
            .expect("edge must be captured")
    }

    pub(super) fn producer_writer(open: OpenFile) -> std::io::PipeWriter {
        let OpenFile::PipeWriter(writer) = open else {
            panic!("producer must be a pipe writer");
        };
        writer
    }

    pub(super) fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }
}
