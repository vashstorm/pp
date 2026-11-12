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
use crate::parser::{SourceSpan, ast};

/// Receives pipeline structure and stage lifecycle information.
///
/// All stage and edge indexes are 1-based. An edge `e` connects stage `e` to
/// stage `e + 1`. Implementations are invoked from the interpreter thread (and
/// from job bookkeeping for background pipelines), so they must be cheap and
/// must not block indefinitely.
pub trait PipelineObserver: Send + Sync {
    /// 登记一个真实解析单元；返回的身份只属于本次观察器。
    fn parsed(
        &self,
        _program: &ast::Program,
        _source: Option<&str>,
        _parent: Option<u64>,
    ) -> Option<u64> {
        None
    }

    /// 开始一次命令执行；同一语法节点的重复执行得到独立身份。
    fn command_begin(
        &self,
        _unit: Option<u64>,
        _command: &ast::Command,
        _parent: Option<u64>,
    ) -> Option<u64> {
        None
    }

    /// 未观察到真实终态时 status 为 None，不能据此补造退出码。
    fn command_end(&self, _execution: u64, _status: Option<u8>, _signal: Option<i32>) {}

    /// 名称查找完成后的实际命令类型。
    fn command_resolved(&self, _execution: u64, _kind: &str) {}

    /// 由解释器确认的短路或未选择分支。
    fn skipped(&self, _unit: Option<u64>, _span: Option<SourceSpan>) {}

    /// 每次实际进入循环体时调用。
    fn iteration(&self, _unit: Option<u64>, _span: Option<SourceSpan>) {}

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

/// 把命令终态跟随到实际等待者；丢弃句柄只表示观测中断。
pub(crate) struct CommandObservation {
    observer: std::sync::Arc<dyn PipelineObserver>,
    execution: u64,
    cancellation: Option<tokio_util::sync::CancellationToken>,
    finished: bool,
}

impl CommandObservation {
    pub(crate) fn new(
        observer: std::sync::Arc<dyn PipelineObserver>,
        execution: u64,
        cancellation: Option<tokio_util::sync::CancellationToken>,
    ) -> Self {
        Self {
            observer,
            execution,
            cancellation,
            finished: false,
        }
    }

    pub(crate) fn finish(&mut self, result: &crate::ExecutionResult) {
        if !self.finished {
            let status = (!self
                .cancellation
                .as_ref()
                .is_some_and(tokio_util::sync::CancellationToken::is_cancelled))
            .then(|| u8::from(&result.exit_code));
            self.observer
                .command_end(self.execution, status, result.signal);
            self.finished = true;
        }
    }
}

impl Drop for CommandObservation {
    fn drop(&mut self) {
        if !self.finished {
            self.observer.command_end(self.execution, None, None);
        }
    }
}
