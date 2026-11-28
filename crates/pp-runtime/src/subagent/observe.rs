//! Read-only observation contract for subagents: typed snapshots, revision numbers and
//! byte-paged event reads.
//!
//! The observation layer does not change task ownership: it only reuses the registry and
//! `<agent>.events.jsonl`, establishing no second token log, database or model tool. The 64 KiB
//! model-side limit of `history://` keeps its original role; the Hub uses
//! [`Binding::read_events`] to page through the full history.

mod overview;
#[cfg(test)]
mod tests;

pub use overview::OverviewPage;

use super::{Binding, Inner, Record, lock, now};
use anyhow::{Context, Result, ensure};
use pp_core::events::AgentEvent;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

/// Maximum number of complete records returned per page.
const PAGE_ENTRIES: usize = 128;
/// Soft read budget per page; an oversized record keeps reading to end of line, never truncated.
const PAGE_BYTES: u64 = 256 * 1024;
/// Defensive cap for an abnormally large single record; a memory guard only, not a truncation point.
const MAX_RECORD_BYTES: usize = 32 * 1024 * 1024;
/// Window size read when checking whether a complete line follows "offset".
const LINE_SCAN_CHUNK: usize = 64 * 1024;
/// Text cap for a single pending coalesce buffer: flush at once when reached to avoid unbounded growth.
const COALESCE_BYTES: usize = 8 * 1024;
/// Buffer time limit: observation polling flushes expired buffers before reading, so the streaming tail does not wait for the next model event.
const COALESCE_DELAY: Duration = Duration::from_millis(250);

/// A single agent in the snapshot; record uses flatten to keep the existing flat JSON shape.
#[derive(Clone, Serialize)]
pub struct HubAgent {
    #[serde(flatten)]
    pub record: Record,
    pub model_change: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct HubSnapshot {
    #[serde(skip)]
    pub output_previews: Vec<ObservedRecord>,
    /// Most recent command display per subagent, kept independently of its main output.
    #[serde(skip)]
    pub shell_previews: Vec<ObservedRecord>,
    pub agents: Vec<HubAgent>,
    pub runs: Vec<super::Run>,
    /// Observation log health hints; not part of the legacy snapshots wire.
    #[serde(skip)]
    pub warnings: Vec<String>,
}

/// In-process event cursor; file identity plus line start offset determines the resumable position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventCursor {
    pub device: u64,
    pub inode: u64,
    pub offset: u64,
}

/// Hub-private lifecycle record; does not extend the global [`AgentEvent`] enum.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HubEvent {
    RunState { status: String },
    AgentState { status: String },
}

/// One line of `.events.jsonl`. Legacy records missing timestamp/hub/optional metadata remain readable.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservedRecord {
    #[serde(default)]
    pub root_id: Option<String>,
    /// Slimmed new lines default to an empty string; the reader fills it in from the requested agent id.
    #[serde(default)]
    pub agent_id: String,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub sequence: Option<u64>,
    #[serde(default)]
    pub timestamp_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<AgentEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub: Option<HubEvent>,
}

#[derive(Clone, Debug)]
pub struct ObservedEntry {
    pub start_offset: u64,
    pub record: ObservedRecord,
}

#[derive(Debug)]
pub struct EventPage {
    pub entries: Vec<ObservedEntry>,
    pub cursor: Option<EventCursor>,
    pub reset: bool,
    pub has_more: bool,
    pub warnings: Vec<String>,
}

pub(super) struct ModelProgress {
    run_id: String,
    generation: u64,
    request_sequence: u64,
    /// Cumulative output bytes and the monotonic time of the first output.
    sample: Option<(u64, Instant)>,
}

impl EventPage {
    fn empty() -> Self {
        Self {
            entries: Vec::new(),
            cursor: None,
            reset: false,
            has_more: false,
            warnings: Vec::new(),
        }
    }
}

/// Coalescible streaming delta kinds; `AssistantDelta`'s refusal participates in boundary matching.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DeltaKind {
    Thinking,
    ReasoningSummary,
    Assistant { refusal: bool },
}

impl DeltaKind {
    fn event(self, text: String) -> AgentEvent {
        match self {
            Self::Thinking => AgentEvent::ThinkingDelta { text },
            Self::ReasoningSummary => AgentEvent::ReasoningSummaryDelta { text },
            Self::Assistant { refusal } => AgentEvent::AssistantDelta { text, refusal },
        }
    }
}

/// Buffer of consecutive deltas awaiting coalescing: keeps the first fragment's anchor, appending text only.
struct PendingDelta {
    run_id: Option<String>,
    generation: u64,
    kind: DeltaKind,
    sequence: Option<u64>,
    timestamp_ms: u64,
    text: String,
    started: Instant,
}

/// Per-agent append state: a whole-line write lock plus an optional pending coalesce buffer.
#[derive(Default)]
pub(super) struct AppendState {
    pending: Option<PendingDelta>,
}

/// Recognizes a coalescible delta and returns its kind with a borrowed text; other events return `None`.
fn coalescible(event: &AgentEvent) -> Option<(DeltaKind, &str)> {
    match event {
        AgentEvent::ThinkingDelta { text } => Some((DeltaKind::Thinking, text.as_str())),
        AgentEvent::ReasoningSummaryDelta { text } => {
            Some((DeltaKind::ReasoningSummary, text.as_str()))
        }
        AgentEvent::AssistantDelta { text, refusal } => {
            Some((DeltaKind::Assistant { refusal: *refusal }, text.as_str()))
        }
        _ => None,
    }
}

/// Streaming envelope: builds no intermediate `Value`, avoiding a copy of the whole output.
/// New lines no longer serialize `root_id`/`agent_id`; the reader fills them in from the requested agent and current directory name.
#[derive(Serialize)]
struct Envelope<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<&'a str>,
    generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    sequence: Option<u64>,
    timestamp_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<&'a AgentEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hub: Option<&'a HubEvent>,
}

/// Optional metadata of an observation envelope; passed separately from the payload to avoid a long parameter list.
struct RecordMeta<'a> {
    run_id: Option<&'a str>,
    generation: u64,
    sequence: Option<u64>,
    timestamp_ms: u64,
}

impl Inner {
    /// One whole-line append lock plus coalesce buffer per agent; the worker holds its own `Arc` and control threads share the same one.
    pub(super) fn append_gate(&self, id: &str) -> std::sync::Arc<std::sync::Mutex<AppendState>> {
        lock(&self.appends)
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    fn append_lock(&self, agent_id: &str) -> std::sync::Arc<std::sync::Mutex<AppendState>> {
        self.append_gate(agent_id)
    }

    /// Writes one observation envelope to `<agent>.events.jsonl`; the caller must already hold that agent's gate.
    fn append_record(
        &self,
        agent_id: &str,
        meta: RecordMeta<'_>,
        event: Option<&AgentEvent>,
        hub: Option<&HubEvent>,
    ) -> Result<()> {
        let RecordMeta {
            run_id,
            generation,
            sequence,
            timestamp_ms,
        } = meta;
        let path = self.directory().join(format!("{agent_id}.events.jsonl"));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        serde_json::to_writer(
            &mut file,
            &Envelope {
                run_id,
                generation,
                sequence,
                timestamp_ms,
                event,
                hub,
            },
        )?;
        file.write_all(b"\n")?;
        Ok(())
    }

    /// Persists one buffered delta; the caller has already taken the buffer, so a failure discards it instead of accumulating.
    fn write_pending(&self, agent_id: &str, pending: PendingDelta) -> Result<()> {
        let event = pending.kind.event(pending.text);
        self.append_record(
            agent_id,
            RecordMeta {
                run_id: pending.run_id.as_deref(),
                generation: pending.generation,
                sequence: pending.sequence,
                timestamp_ms: pending.timestamp_ms,
            },
            Some(&event),
            None,
        )
    }

    /// Flushes and clears the agent's pending coalesce buffer; a no-op when nothing is buffered.
    fn flush_pending(&self, agent_id: &str, state: &mut AppendState) -> Result<()> {
        match state.pending.take() {
            Some(pending) => self.write_pending(agent_id, pending),
            None => Ok(()),
        }
    }

    /// Records one append outcome: success clears the agent's health hint, failure keeps a redacted summary; both bump the revision.
    fn record_append_outcome(&self, agent_id: &str, result: Result<()>) {
        let mut health = lock(&self.observe_errors);
        match result {
            Ok(()) => {
                health.remove(agent_id);
            }
            Err(error) => {
                // Keep only a redacted summary so path details or body text never reach the UI.
                health.insert(
                    agent_id.to_owned(),
                    format!("event log is possibly incomplete: {error}"),
                );
            }
        }
        drop(health);
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    /// Flushes timed-out buffers before an observation-poll read; no action means no revision bump.
    fn flush_expired(&self, agent_id: &str) {
        let gate = self.append_gate(agent_id);
        let mut state = lock(&gate);
        if !state
            .pending
            .as_ref()
            .is_some_and(|pending| pending.started.elapsed() >= COALESCE_DELAY)
        {
            return;
        }
        let result = self.flush_pending(agent_id, &mut state);
        drop(state);
        self.record_append_outcome(agent_id, result);
    }

    fn observe_model_progress(
        &self,
        agent_id: &str,
        run_id: &str,
        generation: u64,
        sequence: u64,
        event: &AgentEvent,
    ) {
        match event {
            AgentEvent::ModelRequestStarted => {
                let mut requests = lock(&self.model_progress);
                if requests
                    .get(agent_id)
                    .is_none_or(|request| generation >= request.generation)
                {
                    requests.insert(
                        agent_id.to_owned(),
                        ModelProgress {
                            run_id: run_id.to_owned(),
                            generation,
                            request_sequence: sequence,
                            sample: None,
                        },
                    );
                }
            }
            AgentEvent::ModelOutputProgress { output_bytes } if *output_bytes > 0 => {
                let mut requests = lock(&self.model_progress);
                if let Some(request) = requests.get_mut(agent_id)
                    && request.run_id == run_id
                    && request.generation == generation
                    && sequence > request.request_sequence
                {
                    let (bytes, _) = request.sample.get_or_insert_with(|| (0, Instant::now()));
                    *bytes = (*bytes).max(*output_bytes);
                }
            }
            AgentEvent::ModelRequestFinished { .. } => {
                let mut requests = lock(&self.model_progress);
                if requests.get(agent_id).is_some_and(|request| {
                    request.run_id == run_id
                        && request.generation == generation
                        && sequence > request.request_sequence
                }) {
                    requests.remove(agent_id);
                }
            }
            _ => {}
        }
    }

    /// Appends one observation record; failures only record a redacted health hint and never cancel the real task.
    pub(super) fn observe(
        &self,
        agent_id: &str,
        run_id: Option<&str>,
        generation: u64,
        sequence: Option<u64>,
        event: Option<&AgentEvent>,
        hub: Option<HubEvent>,
    ) {
        if let Some(run_id) = run_id {
            if let Some(HubEvent::RunState { status }) = &hub
                && !matches!(status.as_str(), "queued" | "running")
            {
                let mut requests = lock(&self.model_progress);
                if requests
                    .get(agent_id)
                    .is_some_and(|request| request.run_id == run_id)
                {
                    requests.remove(agent_id);
                }
            } else if let (Some(event), Some(sequence)) = (event, sequence) {
                self.observe_model_progress(agent_id, run_id, generation, sequence, event);
            }
        }
        // Output progress is kept only as an in-memory sample; diagnostics and rejections serve root-session display only.
        let event = if matches!(
            event,
            Some(
                AgentEvent::ModelOutputProgress { .. }
                    | AgentEvent::HookDiagnostic { .. }
                    | AgentEvent::UserInputRejected { .. }
            )
        ) {
            if hub.is_none() {
                return;
            }
            None
        } else {
            event
        };
        if matches!(event, Some(AgentEvent::ToolArgumentsDelta { .. })) {
            return;
        }
        if let Some(
            event @ (AgentEvent::ToolOutputSnapshot { .. } | AgentEvent::ToolShellSnapshot { .. }),
        ) = event
        {
            let previews = match event {
                AgentEvent::ToolShellSnapshot { .. } => &self.shell_previews,
                _ => &self.output_previews,
            };
            lock(previews).insert(
                agent_id.to_owned(),
                ObservedRecord {
                    root_id: None,
                    agent_id: agent_id.to_owned(),
                    run_id: run_id.map(str::to_owned),
                    generation,
                    sequence,
                    timestamp_ms: Some(now()),
                    event: Some(event.clone()),
                    hub: None,
                },
            );
            self.bump_revision();
            return;
        }
        if matches!(
            event,
            Some(AgentEvent::ToolStarted { .. } | AgentEvent::ToolFinished { .. })
        ) {
            lock(&self.output_previews).remove(agent_id);
            lock(&self.shell_previews).remove(agent_id);
        }
        let gate = self.append_lock(agent_id);
        // `None` means this step only updated the in-memory buffer without writing to disk, so it must not clear an existing write-failure hint.
        let result = (|| -> Option<Result<()>> {
            let mut state = lock(&gate);
            // Bounded coalescing of consecutive same-variant deltas: stay in memory until capacity, timeout or boundary flushes.
            if hub.is_none()
                && let Some(event) = event
                && let Some((kind, text)) = coalescible(event)
            {
                let timestamp = now();
                // Single fragment already at the capacity cap: flush the old buffer first, then write it whole without splitting or merging.
                if text.len() >= COALESCE_BYTES {
                    let flushed = self.flush_pending(agent_id, &mut state);
                    let written = self.append_record(
                        agent_id,
                        RecordMeta {
                            run_id,
                            generation,
                            sequence,
                            timestamp_ms: timestamp,
                        },
                        Some(event),
                        None,
                    );
                    return Some(flushed.and(written));
                }
                let compatible = state.pending.as_ref().is_some_and(|pending| {
                    pending.run_id.as_deref() == run_id
                        && pending.generation == generation
                        && pending.kind == kind
                });
                if compatible {
                    let pending = state.pending.as_mut().expect("compatible pending");
                    // Keep appending within the time limit without separators; persist at once once capacity is reached.
                    if pending.started.elapsed() < COALESCE_DELAY {
                        pending.text.push_str(text);
                        if pending.text.len() < COALESCE_BYTES {
                            return None;
                        }
                        let pending = state.pending.take().expect("merged pending");
                        return Some(self.write_pending(agent_id, pending));
                    }
                }
                // Past the time limit, or a run/generation/variant/refusal boundary: flush the old buffer before starting a new one.
                let flushed = self.flush_pending(agent_id, &mut state);
                state.pending = Some(PendingDelta {
                    run_id: run_id.map(str::to_owned),
                    generation,
                    kind,
                    sequence,
                    timestamp_ms: timestamp,
                    text: text.to_owned(),
                    started: Instant::now(),
                });
                return flushed.err().map(Err);
            }
            // Other records flush the existing buffer first to preserve tool, reply-final and lifecycle ordering;
            // a failed flush still attempts to write the current record, with the flush-side error taking priority.
            let flushed = self.flush_pending(agent_id, &mut state);
            let written = self.append_record(
                agent_id,
                RecordMeta {
                    run_id,
                    generation,
                    sequence,
                    timestamp_ms: now(),
                },
                event,
                hub.as_ref(),
            );
            Some(flushed.and(written))
        })();
        match result {
            Some(result) => self.record_append_outcome(agent_id, result),
            None => self.bump_revision(),
        }
    }

    /// Signals a metric change to the UI after request statistics update.
    pub(super) fn bump_revision(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
    }
}

impl Binding {
    /// Identity comparison of root instances; the directory name must not be compared because it changes from a pending binding to the real session.
    pub fn same_root(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// In-process observation revision, used only to decide incremental refreshes.
    pub fn hub_revision(&self) -> u64 {
        self.inner.revision.load(Ordering::Relaxed)
    }

    /// Reads cumulative output bytes and first-output time for the matching request, touching process memory only.
    pub fn model_output_progress(
        &self,
        agent_id: &str,
        run_id: &str,
        generation: u64,
        request_sequence: u64,
    ) -> Option<(u64, Instant)> {
        let requests = lock(&self.inner.model_progress);
        let request = requests.get(agent_id)?;
        if request.run_id == run_id
            && request.generation == generation
            && request.request_sequence == request_sequence
        {
            request.sample
        } else {
            None
        }
    }

    /// Typed snapshot: clone records under the state lock only, then query the model control registry after releasing it.
    pub fn hub_snapshot(&self) -> HubSnapshot {
        let (records, runs) = {
            let state = lock(&self.inner.state);
            (
                state.disk.agents.values().cloned().collect::<Vec<_>>(),
                state.disk.runs.values().cloned().collect::<Vec<_>>(),
            )
        };
        let runtime = lock(&self.inner.runtime_models).clone();
        let agents = records
            .into_iter()
            .map(|record| {
                let model_change = runtime.as_ref().and_then(|binding| {
                    binding
                        .registry
                        .describe(&record.id, record.spec.model_role)
                });
                HubAgent {
                    record,
                    model_change,
                }
            })
            .collect();
        let warnings = lock(&self.inner.observe_errors)
            .iter()
            .map(|(agent, message)| format!("{agent}: {message}"))
            .collect();
        HubSnapshot {
            output_previews: lock(&self.inner.output_previews)
                .values()
                .cloned()
                .collect(),
            shell_previews: lock(&self.inner.shell_previews).values().cloned().collect(),
            agents,
            runs,
            warnings,
        }
    }

    /// Builds the log path from a registered id and reads it page by page; never accepts arbitrary file paths from the UI.
    pub fn read_events(&self, agent_id: &str, cursor: Option<&EventCursor>) -> Result<EventPage> {
        ensure!(
            lock(&self.inner.state).disk.agents.contains_key(agent_id),
            "unknown agent"
        );
        // Observation polling advances the streaming tail: persist expired buffers, then open the file and compute the cursor.
        self.inner.flush_expired(agent_id);
        let directory = self.inner.directory();
        let root_id = directory
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let path = directory.join(format!("{agent_id}.events.jsonl"));
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(EventPage::empty());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("opening {}", path.display()));
            }
        };
        let metadata = file
            .metadata()
            .with_context(|| format!("inspecting {}", path.display()))?;
        use std::os::unix::fs::MetadataExt;
        let (device, inode, length) = (metadata.dev(), metadata.ino(), metadata.len());
        let mut reset = false;
        let mut start = 0;
        if let Some(cursor) = cursor {
            if cursor.device != device || cursor.inode != inode || length < cursor.offset {
                reset = true;
            } else {
                start = cursor.offset;
            }
        }
        if start > length {
            start = 0;
            reset = true;
        }
        let mut buffer = Vec::new();
        if start < length {
            file.seek(SeekFrom::Start(start))?;
            let budget = (length - start).min(PAGE_BYTES);
            (&mut file).take(budget).read_to_end(&mut buffer)?;
            // A single record may exceed the soft budget; keep reading to end of line and never truncate the record.
            loop {
                let end = start.saturating_add(buffer.len() as u64);
                if buffer.contains(&b'\n') || buffer.len() >= MAX_RECORD_BYTES || end >= length {
                    break;
                }
                let mut chunk = vec![0u8; 64 * 1024];
                let read = file.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
        }
        let mut entries = Vec::new();
        let mut warnings = Vec::new();
        let mut consumed = 0usize;
        let mut capped = false;
        for line in buffer.split_inclusive(|byte| *byte == b'\n') {
            if !line.ends_with(b"\n") {
                break;
            }
            if entries.len() >= PAGE_ENTRIES {
                capped = true;
                break;
            }
            let position = start + consumed as u64;
            consumed += line.len();
            match serde_json::from_slice::<ObservedRecord>(&line[..line.len() - 1]) {
                Ok(mut record) if record.event.is_some() || record.hub.is_some() => {
                    // Fill default identity of slimmed new lines from the requested agent and directory name; explicit values in legacy lines stay as is.
                    if record.agent_id.is_empty() {
                        record.agent_id = agent_id.to_owned();
                    }
                    if record.root_id.is_none() {
                        record.root_id = root_id.clone();
                    }
                    entries.push(ObservedEntry {
                        start_offset: position,
                        record,
                    });
                }
                Ok(_) => warnings.push(format!("record without payload at byte {position}")),
                Err(_) => warnings.push(format!("unreadable record at byte {position}")),
            }
        }
        let mut offset = start + consumed as u64;
        // A single line above MAX_RECORD_BYTES that contains a newline is skipped so the cursor never gets stuck permanently.
        if consumed == 0 && buffer.len() >= MAX_RECORD_BYTES && !buffer.contains(&b'\n') {
            let mut scanned = start + buffer.len() as u64;
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let read = file.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                if let Some(pos) = chunk[..read].iter().position(|byte| *byte == b'\n') {
                    offset = scanned + pos as u64 + 1;
                    warnings.push(format!("oversized record skipped at byte {start}"));
                    break;
                }
                scanned += read as u64;
            }
        }
        // Leave a trailing partial line for next time without advancing offset and without letting has_more cause a busy loop.
        let mut has_more = capped;
        if !has_more && offset < length {
            has_more = has_complete_line(&file, offset, length)?;
        }
        Ok(EventPage {
            entries,
            cursor: Some(EventCursor {
                device,
                inode,
                offset,
            }),
            reset,
            has_more,
            warnings,
        })
    }

    /// Force-flushes every agent's pending coalesce buffer before an export (`/dump all`).
    pub(crate) fn flush_observations(&self) {
        let agent_ids: Vec<String> = lock(&self.inner.appends).keys().cloned().collect();
        for agent_id in agent_ids {
            let gate = self.inner.append_gate(&agent_id);
            let mut state = lock(&gate);
            let result = match state.pending.take() {
                Some(pending) => self.inner.write_pending(&agent_id, pending),
                None => continue,
            };
            drop(state);
            self.inner.record_append_outcome(&agent_id, result);
        }
    }
}

use std::os::unix::fs::FileExt;

/// Whether a complete line remains after `offset`.
///
/// A coalesced record can far exceed 4 KiB, so a fixed-window probe mistakes "more records" for end of file;
/// this scans window by window to the next newline, bounded by `MAX_RECORD_BYTES` in total,
/// so a trailing partial line is still never treated as a complete record and never causes a busy loop.
fn has_complete_line(file: &File, offset: u64, length: u64) -> Result<bool> {
    let end = length.min(offset.saturating_add(MAX_RECORD_BYTES as u64));
    let mut scanned = offset;
    let mut chunk = vec![0u8; LINE_SCAN_CHUNK];
    while scanned < end {
        let want = (end - scanned).min(chunk.len() as u64) as usize;
        let read = file.read_at(&mut chunk[..want], scanned)?;
        if read == 0 {
            return Ok(false);
        }
        if chunk[..read].contains(&b'\n') {
            return Ok(true);
        }
        scanned += read as u64;
    }
    Ok(false)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubagentCounts {
    pub running: usize,
    pub completed: usize,
}

impl Binding {
    /// Whether the current session has registered any subagent (read-only in-memory check).
    pub fn has_agents(&self) -> bool {
        !lock(&self.inner.state).disk.agents.is_empty()
    }

    /// In-memory counts of running agents and agents whose latest run completed successfully.
    pub fn subagent_counts(&self) -> SubagentCounts {
        let state = lock(&self.inner.state);
        let mut latest_runs = BTreeMap::new();
        for run in state.disk.runs.values() {
            let latest = latest_runs.entry(run.agent_id.as_str()).or_insert(run);
            if run.number() >= latest.number() {
                *latest = run;
            }
        }
        let mut counts = SubagentCounts::default();
        for (id, agent) in &state.disk.agents {
            match agent.status.as_str() {
                "running" => counts.running += 1,
                "aborted" => {}
                _ if latest_runs
                    .get(id.as_str())
                    .is_some_and(|run| run.status == "completed") =>
                {
                    counts.completed += 1;
                }
                _ => {}
            }
        }
        counts
    }
    /// JSON shape read by the line-mode CLI and existing consumers: agents keep the flat Record fields.
    pub fn snapshots(&self) -> Value {
        serde_json::to_value(self.hub_snapshot()).unwrap_or_else(|_| json!({"agents":[],"runs":[]}))
    }
}
