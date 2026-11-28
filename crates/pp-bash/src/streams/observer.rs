//! Interpreter observer (`RunObserver`) and the observation handle of one bash call (`PipeRun`).

use brush_core::PipelineObserver;
use brush_core::openfiles::OpenFile;
use serde_json::Value;
#[cfg(test)]
use std::sync::Weak;
use std::{
    io,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use super::super::display::Trace;
use super::super::relay::FINISH_GRACE;
use super::ring::{CloseReason, lock};
use super::run::{RunEnd, RunInfo, RunState};
use super::store::PipeStore;
use super::{MAX_LABEL_BYTES, PREVIEW_BYTES};

pub(super) struct RunObserver {
    pub(super) shell: Trace,
    pub(super) store: PipeStore,
    pub(super) state: Arc<RunState>,
}

impl PipelineObserver for RunObserver {
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
        parent: Option<u64>,
        stages: usize,
        bang: bool,
        pipefail: bool,
    ) -> io::Result<u64> {
        Ok(self.state.record_pipeline(parent, stages, bang, pipefail))
    }

    fn label(&self, id: u64, stage: usize, command: &str) -> io::Result<()> {
        self.state.set_label(id, stage, command);
        Ok(())
    }

    fn pipe(
        &self,
        id: u64,
        edge: usize,
        producer: &str,
        consumer: &str,
    ) -> io::Result<Option<(OpenFile, OpenFile)>> {
        self.store
            .capture_edge(&self.state, id, edge, producer, consumer)
    }

    fn stage_done(&self, id: u64, stage: usize, status: u8, signal: Option<i32>) -> io::Result<()> {
        self.state.record_stage_done(id, stage, status, signal);
        Ok(())
    }

    fn finish(&self, id: u64, status: u8) -> io::Result<()> {
        self.state.finish_pipeline(id, status);
        Ok(())
    }
}

/// Observation handle of one bash call. Clones share the same run.
#[derive(Clone)]
pub struct PipeRun {
    pub(super) store: PipeStore,
    pub(super) state: Arc<RunState>,
    pub(super) observer: Arc<RunObserver>,
}

impl PipeRun {
    pub fn id(&self) -> &str {
        &self.state.id
    }

    /// In-process observer handed to `ExecutionParameters::set_pipeline_observer`.
    pub fn observer(&self) -> Arc<dyn PipelineObserver> {
        self.observer.clone()
    }

    pub(crate) fn prepare_shell(&self, source: &str) {
        self.observer.shell.prepare(source);
    }

    pub(crate) fn shell_revision(&self) -> u64 {
        self.observer.shell.revision()
    }

    pub(crate) fn shell_snapshot(&self) -> Option<pp_core::shell_display::ShellPresentation> {
        self.observer.shell.snapshot()
    }

    pub(crate) fn revision(&self) -> u64 {
        self.state.revision.load(Ordering::Acquire)
    }

    /// Number of relay thread handles still held; during a long run this should only track the number
    /// of concurrently open streams.
    #[cfg(test)]
    pub(crate) fn relay_handle_count(&self) -> usize {
        self.state.relay_handle_count()
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.state.is_finished()
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> RunInfo {
        self.state.snapshot()
    }

    /// `pipes` JSON shared by the live run and the final result (bounded metadata, no stage bodies).
    pub(crate) fn pipes_json(&self) -> Value {
        self.state.snapshot().to_json()
    }

    /// Preview only the first edge of the newest top-level pipeline so multi-stage pipelines do not
    /// display the same body repeatedly.
    pub(crate) fn preview(&self) -> Option<String> {
        let pipeline = lock(&self.state.meta)
            .pipelines
            .iter()
            .rev()
            .find(|pipeline| pipeline.parent.is_none())?
            .id;
        let stream = lock(&self.state.streams)
            .iter()
            .find(|stream| stream.pipeline == pipeline && stream.producer_stage == 1)
            .cloned()?;
        let ring = lock(&stream.ring);
        let start = ring.start().max(ring.end().saturating_sub(PREVIEW_BYTES));
        let bytes = ring.read(start, ring.end());
        drop(ring);
        // The tail window may start mid UTF-8 character; drop the partial prefix.
        let start = if start > 0 {
            bytes
                .iter()
                .position(|byte| byte & 0xc0 != 0x80)
                .unwrap_or(bytes.len())
        } else {
            0
        };
        // Reuse the main output's decoding and control-sequence filtering; incomplete characters are
        // left for a later snapshot.
        let mut normalizer = pp_workspace::output::Normalizer::default();
        let mut preview = normalizer.feed(&bytes[start..], false);
        if !stream.is_open() {
            preview.push_str(&normalizer.feed(&[], true));
        }
        (!preview.is_empty()).then_some(preview)
    }

    /// Seal this run: wait for/stop relays, close remaining streams and record completion history.
    ///
    /// Repeated calls have no effect; returns the final snapshot. Normal shutdown first gives relays
    /// a grace period to observe EOF; on timeout or after an interrupt they are stopped cooperatively
    /// (`Interrupted`) and streams still running are never closed as complete.
    pub(crate) fn finish(&self, end: RunEnd) -> RunInfo {
        self.observer.shell.finish();
        if !self.state.begin_finish(&end) {
            return self.state.snapshot();
        }
        let mut relays = {
            let mut guard = lock(&self.state.relays);
            std::mem::take(&mut *guard)
        };
        let interrupted = self.state.interrupted();
        if interrupted {
            self.state.stop.store(true, Ordering::Release);
        }
        if !relays.is_empty() {
            let deadline = Instant::now()
                + if interrupted {
                    Duration::ZERO
                } else {
                    FINISH_GRACE
                };
            while Instant::now() < deadline && relays.iter().any(|relay| !relay.is_done()) {
                std::thread::sleep(Duration::from_millis(5));
            }
            if relays.iter().any(|relay| !relay.is_done()) {
                self.state.stop.store(true, Ordering::Release);
            }
            for relay in &mut relays {
                relay.join();
            }
        }
        // A stream still open after every relay joined can only mean the relay exited abnormally;
        // seal it with its real terminal state.
        let streams = lock(&self.state.streams).clone();
        for stream in streams {
            if stream.is_open() {
                stream.finish(if interrupted {
                    CloseReason::Interrupted
                } else {
                    CloseReason::Error
                });
            }
        }
        self.store.finalize_run(&self.state);
        self.state.snapshot()
    }
}

/// Weak observation handle for regression tests: confirms the run state (including ring buffers) has
/// no strong references left.
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct RunWatch(Weak<RunState>);

#[cfg(test)]
impl RunWatch {
    pub(crate) fn is_alive(&self) -> bool {
        self.0.strong_count() > 0
    }
}

#[cfg(test)]
impl PipeRun {
    pub(crate) fn watch(&self) -> RunWatch {
        RunWatch(Arc::downgrade(&self.state))
    }
}

/// Stage labels are truncated to 512 bytes on a UTF-8 boundary.
pub(super) fn truncate_label(label: &str) -> &str {
    if label.len() <= MAX_LABEL_BYTES {
        return label;
    }
    let mut end = MAX_LABEL_BYTES;
    while end > 0 && !label.is_char_boundary(end) {
        end -= 1;
    }
    &label[..end]
}

#[cfg(test)]
mod tests {
    use super::super::MAX_CLOSED_STREAMS_PER_RUN;
    use super::super::MAX_STREAMS_PER_RUN;
    use super::super::PREVIEW_BYTES;
    use super::super::fixtures::{edge, producer_writer, unique_run, wait_for};
    use super::super::run::{RunEnd, StageState};
    use super::super::store::PipeStore;
    use super::super::stream::StreamState;
    use std::io::Write;
    use std::sync::Arc;

    #[test]
    fn pipeline_preview_ids_and_open_slot_quota() {
        {
            let run = unique_run(&PipeStore::default());
            let pipeline = run.observer().begin(None, 3, false, false).unwrap();
            let stream = Arc::new(StreamState::new(
                1,
                pipeline,
                1,
                2,
                "producer",
                "cat",
                Arc::new(|_| {}),
            ));
            run.state.push_stream(stream.clone());
            stream.observe(b"\x1b[31mA\xff\xe4");
            assert_eq!(run.preview().as_deref(), Some("A�"));
            stream.observe(b"\xb8\xad\x1b[0m");
            assert_eq!(run.preview().as_deref(), Some("A�中"));
            let duplicate = Arc::new(StreamState::new(
                2,
                pipeline,
                2,
                3,
                "cat",
                "tail",
                Arc::new(|_| {}),
            ));
            duplicate.observe(b"duplicate");
            run.state.push_stream(duplicate);
            let nested = run
                .observer()
                .begin(Some(pipeline), 2, false, false)
                .unwrap();
            let nested_stream = Arc::new(StreamState::new(
                3,
                nested,
                1,
                2,
                "nested",
                "cat",
                Arc::new(|_| {}),
            ));
            nested_stream.observe(b"nested");
            run.state.push_stream(nested_stream);
            assert_eq!(run.preview().as_deref(), Some("A�中"));
            stream.observe("中".repeat(PREVIEW_BYTES as usize).as_bytes());
            let preview = run.preview().unwrap();
            assert!(preview.len() <= PREVIEW_BYTES as usize);
            assert!(preview.chars().all(|ch| ch == '中'));
            run.observer().begin(None, 2, false, false).unwrap();
            assert_eq!(
                run.preview(),
                None,
                "a new pipeline must not reuse the previous preview"
            );
        }
        {
            let store = PipeStore::new();
            let first = unique_run(&store);
            let second = unique_run(&store);

            let (_, producer_a) = edge(&first, 1, 1);
            let (_, producer_b) = edge(&first, 1, 2);
            let (_, producer_c) = edge(&second, 1, 1);
            drop((producer_a, producer_b, producer_c));

            let first_info = first.snapshot();
            let second_info = second.snapshot();
            assert_eq!(
                first_info.streams.iter().map(|s| s.id).collect::<Vec<_>>(),
                vec![1, 2]
            );
            assert_eq!(
                second_info.streams.iter().map(|s| s.id).collect::<Vec<_>>(),
                vec![1]
            );
            assert!(store.snapshot(first.id()).is_ok());
            assert!(store.snapshot("missing-run").is_err());
            first.finish(RunEnd::exited(Some(0)));
            second.finish(RunEnd::exited(Some(0)));
        }
        {
            let store = PipeStore::new();
            let run = unique_run(&store);
            let pipeline = run
                .observer()
                .begin(None, MAX_STREAMS_PER_RUN + 2, false, false)
                .unwrap();
            // Keep the interpreter-side endpoints alive so these streams stay open.
            let mut live = Vec::new();
            for stream in 1..=MAX_STREAMS_PER_RUN {
                let pair = run
                    .observer()
                    .pipe(pipeline, stream, "yes", "head")
                    .expect("capture edge")
                    .unwrap_or_else(|| panic!("stream {stream} must be captured"));
                live.push(pair);
            }
            // Every slot holds an open stream: the 17th edge is simply not captured and never evicts an
            // open stream.
            let rejected = run
                .observer()
                .pipe(pipeline, MAX_STREAMS_PER_RUN + 1, "yes", "head")
                .expect("quota rejection is not an execution error");
            assert!(rejected.is_none());

            let info = run.snapshot();
            assert_eq!(info.streams.len(), MAX_STREAMS_PER_RUN);
            assert_eq!(info.untapped_edges, 1);
            assert!(info.incomplete);
            assert!(info.streams.iter().all(|stream| stream.open));
            // A quota rejection does not consume a stream id either: ids stay monotonically increasing.
            let mut ids = info
                .streams
                .iter()
                .map(|stream| stream.id)
                .collect::<Vec<_>>();
            ids.sort_unstable();
            assert_eq!(ids, (1..=MAX_STREAMS_PER_RUN as u64).collect::<Vec<_>>());
            drop(live);
            run.finish(RunEnd::exited(Some(0)));
        }
    }

    #[test]
    fn pipeline_relay_cleanup_and_real_shell_observer() {
        {
            let store = PipeStore::new();
            let run = unique_run(&store);
            let mut peak = 0usize;
            for index in 0..48u64 {
                let pipeline = run.observer().begin(None, 2, false, false).unwrap();
                let (consumer, producer) = edge(&run, pipeline, 1);
                let mut writer = producer_writer(producer);
                writer.write_all(b"stage body\n").expect("write edge");
                drop(writer);
                let stream_id = index + 1;
                assert!(
                    wait_for(|| store
                        .read(run.id(), stream_id, None)
                        .is_ok_and(|read| !read.open && !read.data.is_empty())),
                    "iteration {index}: stream {stream_id} must close"
                );
                drop(consumer);
                // Relay handles of sealed streams must be reclaimed on the next registration instead of
                // piling up until the run ends.
                peak = peak.max(run.relay_handle_count());
                assert!(
                    run.relay_handle_count() <= MAX_CLOSED_STREAMS_PER_RUN + 1,
                    "iteration {index}: {} live relay handles",
                    run.relay_handle_count()
                );
            }
            assert!(
                peak <= MAX_CLOSED_STREAMS_PER_RUN + 1,
                "sequential pipelines accumulated {peak} relay handles"
            );
            run.finish(RunEnd::exited(Some(0)));
            assert_eq!(run.relay_handle_count(), 0);
        }
        {
            use brush_builtins::{BuiltinSet, default_builtins};
            use brush_core::extensions::DefaultShellExtensions;
            use brush_core::{ProfileLoadBehavior, RcLoadBehavior, Shell, SourceInfo};

            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .unwrap();
            let store = PipeStore::new();
            let run = unique_run(&store);
            let observer = run.observer();
            runtime.block_on(async move {
                let mut shell = Shell::builder()
                    .builtins(default_builtins::<DefaultShellExtensions>(
                        BuiltinSet::BashMode,
                    ))
                    .profile(ProfileLoadBehavior::Skip)
                    .rc(RcLoadBehavior::Skip)
                    .command_string_mode(true)
                    .build()
                    .await
                    .expect("creating shell");
                shell.start_command_string_mode();
                let mut params = shell.default_exec_params();
                params.set_pipeline_observer(observer);
                let source = SourceInfo::from("-c");
                let result = shell
                    .run_string("printf 'hi\\n' | cat", &source, &params)
                    .await
                    .expect("running pipeline");
                assert_eq!(u8::from(result.exit_code), 0);
                let _ = shell.end_command_string_mode();
            });

            let info = run.finish(RunEnd::exited(Some(0)));
            assert_eq!(info.pipelines.len(), 1);
            let pipeline = &info.pipelines[0];
            assert_eq!(pipeline.stage_count, 2);
            assert_eq!(pipeline.status, Some(0));
            assert_eq!(
                pipeline
                    .stages
                    .iter()
                    .map(|stage| (stage.index, stage.state, stage.status))
                    .collect::<Vec<_>>(),
                vec![
                    (1, StageState::Exited, Some(0)),
                    (2, StageState::Exited, Some(0))
                ]
            );
            assert_eq!(pipeline.stages[0].command, "printf 'hi\\n'");
            assert_eq!(pipeline.stages[1].command, "cat");
            assert_eq!(info.streams.len(), 1);
            let stream = &info.streams[0];
            assert_eq!((stream.producer_stage, stream.consumer_stage), (1, 2));
            assert_eq!(stream.close_reason.as_deref(), Some("eof"));
            assert_eq!(stream.total_bytes, 3);
            let read = store
                .read(run.id(), stream.id, None)
                .expect("read captured bytes");
            assert_eq!(read.data, b"hi\n");
            assert!(!read.open);
        }
    }
}
