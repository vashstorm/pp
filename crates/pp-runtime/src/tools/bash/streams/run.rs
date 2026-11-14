//! run 级元数据、状态与终态快照。

use serde::Serialize;
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use super::super::relay::Relay;
use super::observer::truncate_label;
use super::ring::lock;
use super::store::run_uri;
use super::stream::{StreamMeta, StreamState};
use super::{MAX_CLOSED_STREAMS_PER_RUN, MAX_PIPELINES_PER_RUN, MAX_STAGES_PER_PIPELINE};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StageState {
    Running,
    Exited,
    Unknown,
    Interrupted,
}

struct StageRecord {
    index: usize,
    command: String,
    state: StageState,
    status: Option<u8>,
    signal: Option<i32>,
}

pub(super) struct PipelineRecord {
    pub(super) id: u64,
    pub(super) parent: Option<u64>,
    stage_count: usize,
    bang: bool,
    pipefail: bool,
    status: Option<u8>,
    stages: Vec<StageRecord>,
}

impl PipelineRecord {
    fn materialize(&self) -> PipelineInfo {
        PipelineInfo {
            id: self.id,
            parent: self.parent,
            stage_count: self.stage_count,
            bang: self.bang,
            pipefail: self.pipefail,
            status: self.status,
            stages: self
                .stages
                .iter()
                .map(|stage| StageInfo {
                    index: stage.index,
                    command: stage.command.clone(),
                    state: stage.state,
                    status: stage.status,
                    signal: stage.signal,
                })
                .collect(),
        }
    }
}

#[derive(Default)]
pub(super) struct RunMeta {
    pub(super) pipelines: VecDeque<PipelineRecord>,
    omitted_pipelines: u64,
    omitted_stages: u64,
    untapped_edges: u64,
    pub(super) evicted_streams: u64,
    nonzero_stages: u64,
    capture_error: Option<String>,
}

/// 一次 run 的终态。`exit_code` 来自受监督 shell；中断原因只用于展示。
#[derive(Clone, Debug)]
pub(crate) struct RunEnd {
    pub(crate) exit_code: Option<i32>,
    pub(crate) interrupted: Option<String>,
    pub(crate) capture_error: Option<String>,
}

impl RunEnd {
    pub(crate) fn exited(exit_code: Option<i32>) -> Self {
        Self {
            exit_code,
            interrupted: None,
            capture_error: None,
        }
    }

    pub(crate) fn interrupted(reason: impl Into<String>) -> Self {
        Self {
            exit_code: None,
            interrupted: Some(reason.into()),
            capture_error: None,
        }
    }
}

pub(super) struct RunState {
    pub(super) id: String,
    owner: String,
    pub(super) stop: Arc<AtomicBool>,
    next_pipeline: AtomicU64,
    pub(super) next_stream: AtomicU64,
    pub(super) revision: AtomicU64,
    end: Mutex<Option<RunEnd>>,
    pub(super) meta: Mutex<RunMeta>,
    pub(super) streams: Mutex<Vec<Arc<StreamState>>>,
    pub(super) relays: Mutex<Vec<Relay>>,
}

impl RunState {
    pub(super) fn new(id: String, owner: String) -> Self {
        Self {
            id,
            owner,
            stop: Arc::new(AtomicBool::new(false)),
            next_pipeline: AtomicU64::new(0),
            next_stream: AtomicU64::new(0),
            revision: AtomicU64::new(0),
            end: Mutex::new(None),
            meta: Mutex::new(RunMeta::default()),
            streams: Mutex::new(Vec::new()),
            relays: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn bump(&self) {
        self.revision.fetch_add(1, Ordering::Release);
    }

    pub(super) fn is_finished(&self) -> bool {
        lock(&self.end).is_some()
    }

    pub(super) fn interrupted(&self) -> bool {
        lock(&self.end)
            .as_ref()
            .is_some_and(|end| end.interrupted.is_some())
    }

    pub(super) fn record_pipeline(
        &self,
        parent: Option<u64>,
        stages: usize,
        bang: bool,
        pipefail: bool,
    ) -> u64 {
        let id = self.next_pipeline.fetch_add(1, Ordering::Relaxed) + 1;
        if self.is_finished() {
            return id;
        }
        let mut meta = lock(&self.meta);
        if meta.pipelines.len() >= MAX_PIPELINES_PER_RUN {
            meta.pipelines.pop_front();
            meta.omitted_pipelines += 1;
        }
        let detailed = stages.min(MAX_STAGES_PER_PIPELINE);
        meta.omitted_stages += (stages - detailed) as u64;
        meta.pipelines.push_back(PipelineRecord {
            id,
            parent,
            stage_count: stages,
            bang,
            pipefail,
            status: None,
            stages: (1..=detailed)
                .map(|index| StageRecord {
                    index,
                    command: String::new(),
                    state: StageState::Running,
                    status: None,
                    signal: None,
                })
                .collect(),
        });
        drop(meta);
        self.bump();
        id
    }

    pub(super) fn set_label(&self, id: u64, stage: usize, command: &str) {
        let mut changed = false;
        {
            let mut meta = lock(&self.meta);
            if let Some(detail) = meta
                .pipelines
                .iter_mut()
                .find(|record| record.id == id)
                .and_then(|record| {
                    record
                        .stages
                        .iter_mut()
                        .find(|detail| detail.index == stage)
                })
            {
                let label = truncate_label(command);
                if detail.command != label {
                    detail.command = label.to_owned();
                    changed = true;
                }
            }
        }
        if changed {
            self.bump();
        }
    }

    pub(super) fn record_stage_done(&self, id: u64, stage: usize, status: u8, signal: Option<i32>) {
        let mut meta = lock(&self.meta);
        if status != 0 || signal.is_some() {
            // 即使明细已被配额淘汰，只要真实观察到非零状态就累计计数。
            meta.nonzero_stages += 1;
        }
        let mut changed = false;
        if let Some(detail) = meta
            .pipelines
            .iter_mut()
            .find(|record| record.id == id)
            .and_then(|record| {
                record
                    .stages
                    .iter_mut()
                    .find(|detail| detail.index == stage)
            })
        {
            detail.state = StageState::Exited;
            detail.status = Some(status);
            detail.signal = signal;
            changed = true;
        }
        let counted = status != 0 || signal.is_some();
        drop(meta);
        if changed || counted {
            self.bump();
        }
    }

    pub(super) fn finish_pipeline(&self, id: u64, status: u8) {
        let mut found = false;
        {
            let mut meta = lock(&self.meta);
            if let Some(record) = meta.pipelines.iter_mut().find(|record| record.id == id) {
                record.status = Some(status);
                for detail in &mut record.stages {
                    if detail.state == StageState::Running {
                        // 未收到真实等待结果的阶段只能是 unknown，不能补 0。
                        detail.state = StageState::Unknown;
                    }
                }
                found = true;
            }
        }
        if found {
            self.bump();
        }
    }

    pub(super) fn note_untapped(&self) {
        lock(&self.meta).untapped_edges += 1;
        self.bump();
    }

    pub(super) fn note_capture_error(&self, message: impl Into<String>) {
        let mut meta = lock(&self.meta);
        if meta.capture_error.is_none() {
            meta.capture_error = Some(message.into());
        }
        drop(meta);
        self.bump();
    }

    pub(super) fn push_stream(&self, stream: Arc<StreamState>) {
        lock(&self.streams).push(stream);
    }

    pub(super) fn push_relay(&self, relay: Relay) {
        lock(&self.relays).push(relay);
    }

    /// 回收已经结束的中继线程。
    ///
    /// 已封存/被淘汰的流不会在 run 结束前自动释放它的中继句柄，长 run 若只保留
    /// 到 finish 才 join 会无界积累 JoinHandle。这里在**不持有任何锁**的情况下
    /// 逐个 join 已完成的中继；未完成的条目留在列表里由本次或收尾继续等待。
    pub(super) fn reap_finished_relays(&self) {
        loop {
            let relay = {
                let mut relays = lock(&self.relays);
                let Some(index) = relays.iter().position(Relay::is_done) else {
                    break;
                };
                Some(relays.remove(index))
            };
            let Some(mut relay) = relay else {
                break;
            };
            relay.join();
        }
    }

    #[cfg(test)]
    pub(super) fn relay_handle_count(&self) -> usize {
        lock(&self.relays).len()
    }

    pub(super) fn stream(&self, id: u64) -> Option<Arc<StreamState>> {
        lock(&self.streams)
            .iter()
            .find(|stream| stream.id == id)
            .cloned()
    }

    pub(super) fn stream_count(&self) -> usize {
        lock(&self.streams).len()
    }

    pub(super) fn pipeline_count(&self) -> usize {
        lock(&self.meta).pipelines.len()
    }

    /// 已关闭流超过每 run 保留上限时淘汰最老的那几条。
    pub(super) fn stream_closed(&self) {
        let mut evicted = 0u64;
        {
            let mut streams = lock(&self.streams);
            let closed: Vec<usize> = streams
                .iter()
                .enumerate()
                .filter(|(_, stream)| !stream.is_open())
                .map(|(index, _)| index)
                .collect();
            let remove = closed.len().saturating_sub(MAX_CLOSED_STREAMS_PER_RUN);
            if remove > 0 {
                // 流 id 单调递增，保持插入顺序的 streams 前面就是更老的已关闭流。
                for index in closed[..remove].iter().rev() {
                    streams.remove(*index);
                    evicted += 1;
                }
            }
        }
        if evicted > 0 {
            lock(&self.meta).evicted_streams += evicted;
        }
        self.bump();
        // 封存/淘汰后立即回收其它已完成的中继；当前中继尚未置 done，不会被自 join。
        // 这样即使之后不再创建新边，句柄数量也只与并发 open 流相关。
        self.reap_finished_relays();
    }

    pub(super) fn begin_finish(&self, end: &RunEnd) -> bool {
        let mut slot = lock(&self.end);
        if slot.is_some() {
            return false;
        }
        if let Some(error) = &end.capture_error {
            let mut meta = lock(&self.meta);
            if meta.capture_error.is_none() {
                meta.capture_error = Some(error.clone());
            }
        }
        *slot = Some(end.clone());
        drop(slot);
        {
            let interrupted = end.interrupted.is_some();
            let mut meta = lock(&self.meta);
            for record in &mut meta.pipelines {
                for detail in &mut record.stages {
                    if detail.state == StageState::Running {
                        // 解释器收尾前仍没有真实等待结果的阶段：取消为 interrupted，
                        // 其他原因是 unknown；两者都不补造 0。
                        detail.state = if interrupted {
                            StageState::Interrupted
                        } else {
                            StageState::Unknown
                        };
                    }
                }
            }
        }
        self.bump();
        true
    }

    pub(super) fn snapshot(&self) -> RunInfo {
        let (
            pipelines,
            nonzero_stages,
            omitted_pipelines,
            omitted_stages,
            untapped_edges,
            evicted_streams,
            capture_error,
        ) = {
            let meta = lock(&self.meta);
            (
                meta.pipelines
                    .iter()
                    .map(PipelineRecord::materialize)
                    .collect::<Vec<_>>(),
                meta.nonzero_stages,
                meta.omitted_pipelines,
                meta.omitted_stages,
                meta.untapped_edges,
                meta.evicted_streams,
                meta.capture_error.clone(),
            )
        };
        let streams = lock(&self.streams)
            .iter()
            .map(|stream| stream.meta(&self.id))
            .collect::<Vec<_>>();
        let end = lock(&self.end).clone();
        let (state, exit_code) = match &end {
            None => (RunStateKind::Running, None),
            Some(end) if end.interrupted.is_some() => (RunStateKind::Interrupted, end.exit_code),
            Some(end) => (RunStateKind::Exited, end.exit_code),
        };
        RunInfo {
            run_id: self.id.clone(),
            uri: run_uri(&self.id),
            owner: self.owner.clone(),
            state,
            exit_code,
            revision: self.revision.load(Ordering::Acquire),
            pipelines,
            streams,
            nonzero_stages,
            omitted_pipelines,
            omitted_stages,
            untapped_edges,
            evicted_streams,
            incomplete: omitted_pipelines > 0
                || omitted_stages > 0
                || untapped_edges > 0
                || evicted_streams > 0
                || capture_error.is_some(),
            capture_error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStateKind {
    Running,
    Exited,
    Interrupted,
}

#[derive(Clone, Debug, Serialize)]
pub struct StageInfo {
    pub(crate) index: usize,
    pub command: String,
    pub state: StageState,
    pub status: Option<u8>,
    pub signal: Option<i32>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PipelineInfo {
    pub id: u64,
    pub parent: Option<u64>,
    pub stage_count: usize,
    pub bang: bool,
    pub pipefail: bool,
    pub status: Option<u8>,
    pub stages: Vec<StageInfo>,
}

/// run 级别的有界元数据；`pipes_json` 与列表/快照共用同一结构，不含阶段正文。
#[derive(Clone, Debug, Serialize)]
pub struct RunInfo {
    pub run_id: String,
    pub(crate) uri: String,
    pub owner: String,
    pub state: RunStateKind,
    pub exit_code: Option<i32>,
    pub revision: u64,
    pub pipelines: Vec<PipelineInfo>,
    pub streams: Vec<StreamMeta>,
    pub nonzero_stages: u64,
    pub omitted_pipelines: u64,
    pub omitted_stages: u64,
    pub untapped_edges: u64,
    pub evicted_streams: u64,
    pub incomplete: bool,
    pub capture_error: Option<String>,
}

impl RunInfo {
    pub fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::super::MAX_LABEL_BYTES;
    use super::super::MAX_PIPELINES_PER_RUN;
    use super::super::MAX_STAGES_PER_PIPELINE;
    use super::super::fixtures::{edge, unique_run};
    use super::super::store::{PipeStore, run_uri, stream_uri};
    use super::*;

    #[test]
    fn pipeline_metadata_retains_recent_occurrences() {
        let store = PipeStore::new();
        let run = unique_run(&store);
        for index in 0..(MAX_PIPELINES_PER_RUN + 4) {
            let id = run
                .observer()
                .begin(None, 2, index % 2 == 0, false)
                .unwrap();
            run.observer().stage_done(id, 1, 0, None).unwrap();
            run.observer().finish(id, 0).unwrap();
        }
        let info = run.snapshot();
        assert_eq!(info.pipelines.len(), MAX_PIPELINES_PER_RUN);
        assert_eq!(info.omitted_pipelines, 4);
        assert_eq!(info.nonzero_stages, 0);
        assert!(info.incomplete);
        // 前 4 个 occurrence 的明细被淘汰，但 id 序列保持单调且不复用。
        let ids = info.pipelines.iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(
            ids,
            (5u64..=(MAX_PIPELINES_PER_RUN as u64 + 4)).collect::<Vec<_>>()
        );
        run.finish(RunEnd::exited(Some(0)));
    }

    #[test]
    fn pipeline_metadata_truncates_labels_and_counts_nonzero_beyond_details() {
        let store = PipeStore::new();
        let run = unique_run(&store);
        let id = run
            .observer()
            .begin(None, MAX_STAGES_PER_PIPELINE + 8, false, true)
            .unwrap();
        let long = "λ".repeat(MAX_LABEL_BYTES);
        run.observer().label(id, 1, &long).unwrap();
        // 超出明细容量的阶段也必须计入非零事实。
        run.observer().stage_done(id, 1, 3, None).unwrap();
        run.observer()
            .stage_done(id, MAX_STAGES_PER_PIPELINE + 8, 7, Some(13))
            .unwrap();
        run.observer().finish(id, 3).unwrap();

        let info = run.snapshot();
        assert_eq!(info.pipelines.len(), 1);
        let pipeline = &info.pipelines[0];
        assert_eq!(pipeline.stage_count, MAX_STAGES_PER_PIPELINE + 8);
        assert_eq!(pipeline.stages.len(), MAX_STAGES_PER_PIPELINE);
        assert_eq!(pipeline.stages[0].command.len(), MAX_LABEL_BYTES);
        assert!(pipeline.stages[0].command.is_char_boundary(MAX_LABEL_BYTES));
        assert_eq!(pipeline.status, Some(3));
        assert_eq!(info.nonzero_stages, 2);
        assert_eq!(info.omitted_stages, 8);
        // 未报告真实等待结果的阶段残留为 unknown，绝不补 0。
        let tail = pipeline.stages.last().unwrap();
        assert_eq!(tail.state, StageState::Unknown);
        assert_eq!(tail.status, None);
        run.finish(RunEnd::exited(Some(3)));
    }

    #[test]
    fn pipeline_pipes_json_carries_bounded_metadata_only() {
        let store = PipeStore::new();
        let run = unique_run(&store);
        let id = run.observer().begin(None, 2, true, false).unwrap();
        run.observer().label(id, 1, "printf").unwrap();
        run.observer().label(id, 2, "cat").unwrap();
        let (consumer, producer) = edge(&run, id, 1);
        drop((consumer, producer));
        run.observer().stage_done(id, 1, 7, None).unwrap();
        run.observer().stage_done(id, 2, 0, Some(13)).unwrap();
        run.observer().finish(id, 0).unwrap();
        let info = run.finish(RunEnd::exited(Some(0)));
        let json = info.to_json();

        assert_eq!(json["run_id"], run.id());
        assert_eq!(json["uri"], run_uri(run.id()));
        assert_eq!(json["state"], "exited");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["nonzero_stages"], 2);
        assert_eq!(json["incomplete"], false);
        let pipeline = &json["pipelines"][0];
        assert_eq!(pipeline["bang"], true);
        assert_eq!(pipeline["status"], 0);
        let stage = &pipeline["stages"][0];
        assert_eq!(stage["index"], 1);
        assert_eq!(stage["command"], "printf");
        assert_eq!(stage["state"], "exited");
        assert_eq!(stage["status"], 7);
        let stream = &json["streams"][0];
        assert_eq!(stream["uri"], stream_uri(run.id(), 1));
        assert_eq!(stream["producer_stage"], 1);
        assert_eq!(stream["consumer_stage"], 2);
        // 快照只含元数据，绝不携带阶段正文。
        assert!(stream.get("data").is_none());
        assert!(json["pipelines"][0]["stages"][0].get("data").is_none());
    }
}
