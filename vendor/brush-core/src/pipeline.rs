//! Observation hooks for pipelines executed by the interpreter.
//!
//! The interpreter reports the structure and lifecycle of every pipeline that
//! has more than one stage. An embedding host can use the hook to mirror the
//! bytes that actually flow between stages without parsing or rewriting the
//! command text. The hook is advisory for metadata but structural for pipes:
//! when [`PipelineObserver::pipe`] returns a pair of endpoints, the interpreter
//! uses them in place of its own `std::io::pipe()`.

use std::io;

use crate::openfiles::OpenFile;

/// Receives pipeline structure and stage lifecycle information.
///
/// All stage and edge indexes are 1-based. An edge `e` connects stage `e` to
/// stage `e + 1`. Implementations are invoked from the interpreter thread (and
/// from job bookkeeping for background pipelines), so they must be cheap and
/// must not block indefinitely.
pub trait PipelineObserver: Send + Sync {
    /// Starts observing a pipeline occurrence.
    ///
    /// `parent` is the nearest enclosing multi-stage pipeline occurrence, if
    /// any. `stages` is the number of stages in this pipeline, `bang` reports a
    /// `!`-negated pipeline, and `pipefail` reports whether the `pipefail`
    /// option is enabled. Returns the run-local occurrence identifier.
    fn begin(
        &self,
        parent: Option<u64>,
        stages: usize,
        bang: bool,
        pipefail: bool,
    ) -> io::Result<u64>;

    /// Records the command text of one stage.
    ///
    /// `stage` is in `1..=stages`.
    fn label(&self, id: u64, stage: usize, command: &str) -> io::Result<()>;

    /// Supplies the endpoints of the pipe for one adjacent edge.
    ///
    /// `edge` is in `1..stages`. The returned tuple is
    /// `(consumer reader, producer writer)`: the reader is installed as the
    /// downstream stage's standard input and the writer as the upstream stage's
    /// standard output. `Ok(None)` means the edge is not tapped; the
    /// interpreter falls back to an ordinary pipe. An error aborts the pipeline
    /// before any stage is spawned.
    fn pipe(
        &self,
        id: u64,
        edge: usize,
        producer: &str,
        consumer: &str,
    ) -> io::Result<Option<(OpenFile, OpenFile)>>;

    /// Records the observed completion of one stage.
    ///
    /// `status` is the real exit status of the stage and `signal` is set only
    /// when the stage was a directly reaped external process that terminated
    /// because of a signal. Stages that were not observed (cancellation, spawn
    /// or wait errors) are never reported with a fabricated status.
    fn stage_done(&self, id: u64, stage: usize, status: u8, signal: Option<i32>) -> io::Result<()>;

    /// Finishes an observed pipeline with its final status.
    ///
    /// This is called after `pipefail` and `!` handling, before the `ERR` trap.
    fn finish(&self, id: u64, status: u8) -> io::Result<()>;
}
