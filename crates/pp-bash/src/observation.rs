//! Command display and bounded diagnostics from real shell execution events.

use crate::{display::Trace, lock};
use brush_core::{PipelineObserver, openfiles::OpenFile};
use serde::Serialize;
use std::{
    io,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

const MAX_FAILURE_DETAILS: usize = 32;

pub(super) struct Observer {
    pub(super) shell: Trace,
    next_pipeline: AtomicU64,
    failures: Mutex<PipelineFailures>,
}

#[derive(Clone, Default, Serialize)]
pub(super) struct PipelineFailures {
    count: u64,
    stages: Vec<StageFailure>,
}

#[derive(Clone, Serialize)]
struct StageFailure {
    pipeline: u64,
    stage: usize,
    status: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
}

impl Observer {
    pub(super) fn new(command: &str) -> Self {
        let shell = Trace::new();
        shell.prepare(command);
        Self {
            shell,
            next_pipeline: AtomicU64::new(0),
            failures: Mutex::new(PipelineFailures::default()),
        }
    }

    pub(super) fn failures(&self) -> Option<PipelineFailures> {
        let failures = lock(&self.failures);
        (failures.count > 0).then(|| failures.clone())
    }
}

impl PipelineObserver for Observer {
    fn parsed(
        &self,
        program: &brush_core::parser::ast::Program,
        source: Option<&str>,
        parent: Option<u64>,
    ) -> Option<u64> {
        self.shell.parsed(program, source, parent)
    }

    fn command_begin(
        &self,
        unit: Option<u64>,
        command: &brush_core::parser::ast::Command,
        _parent: Option<u64>,
    ) -> Option<u64> {
        self.shell.begin(unit, command)
    }

    fn command_end(&self, execution: u64, status: Option<u8>, _signal: Option<i32>) {
        self.shell.end(execution, status);
    }

    fn command_resolved(&self, execution: u64, kind: &str) {
        self.shell.resolved(execution, kind);
    }

    fn skipped(&self, unit: Option<u64>, span: Option<brush_core::SourceSpan>) {
        self.shell.skipped(unit, span);
    }

    fn iteration(&self, unit: Option<u64>, span: Option<brush_core::SourceSpan>) {
        self.shell.iteration(unit, span);
    }

    fn begin(
        &self,
        _parent: Option<u64>,
        _stages: usize,
        _bang: bool,
        _pipefail: bool,
    ) -> io::Result<u64> {
        Ok(self.next_pipeline.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn label(&self, _id: u64, _stage: usize, _command: &str) -> io::Result<()> {
        Ok(())
    }

    fn pipe(
        &self,
        _id: u64,
        _edge: usize,
        _producer: &str,
        _consumer: &str,
    ) -> io::Result<Option<(OpenFile, OpenFile)>> {
        Ok(None)
    }

    fn stage_done(&self, id: u64, stage: usize, status: u8, signal: Option<i32>) -> io::Result<()> {
        if status != 0 || signal.is_some() {
            let mut failures = lock(&self.failures);
            failures.count = failures.count.saturating_add(1);
            if failures.stages.len() < MAX_FAILURE_DETAILS {
                failures.stages.push(StageFailure {
                    pipeline: id,
                    stage,
                    status,
                    signal,
                });
            }
        }
        Ok(())
    }

    fn finish(&self, _id: u64, _status: u8) -> io::Result<()> {
        Ok(())
    }
}
