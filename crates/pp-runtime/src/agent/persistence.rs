mod format;
mod gc;
pub(crate) mod index;
mod listing;
mod load;
pub mod locks;
mod messages;
mod replay;
mod state;
mod stats;
mod summary;
pub mod tree;

use super::Session;
use anyhow::{Context, Result, bail};
use async_openai::types::{
    chat::CreateChatCompletionRequest,
    responses::{CreateResponse, InputParam},
};
use format::{
    Custom, CustomRef, Entry, EntryRef, Header, NativeAppend, Payload, PayloadRef, SavedMessage,
};
use locks::SessionLock;
use pp_config::config::EditMode;
use pp_config::config::ToolLimits;
use pp_core::events::AgentEvent;
use pp_core::model::ThinkLevel;
use pp_core::plan::WorkMode;
use pp_core::plan::WorkflowState;
use pp_core::session_jsonl::Storage;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::Instant,
};

pub(in crate::agent::persistence) use anyhow::ensure;
pub use gc::{GarbageCollection, gc};
pub use listing::{SessionListing, SessionSummary, delete, validate_id};
pub(in crate::agent::persistence) use listing::{
    compact_summary, create, latest, now_ms, parse_timestamp, timestamp,
};
pub use load::{load, load_readonly};
use stats::PendingToolStat;

pub enum ReplayRecord {
    User(String),
    Event(AgentEvent),
    Error(String),
    Cancelled,
}

pub struct RestoredSession {
    pub(super) checkpoint: Checkpoint,
    pub(super) journal: SessionJournal,
    pub(super) replay: Vec<ReplayRecord>,
}

impl RestoredSession {
    pub fn cwd(&self) -> &Path {
        &self.checkpoint.cwd
    }
    pub fn edit_mode(&self) -> EditMode {
        self.checkpoint.edit_mode
    }
    pub fn work_mode(&self) -> WorkMode {
        self.checkpoint.workflow.mode
    }
}

pub(super) struct Checkpoint {
    pub(super) role_route: Option<pp_config::config::RoleRoute>,
    id: String,
    created_at_ms: u64,
    pub(super) cwd: PathBuf,
    pub(super) model_reference: String,
    pub(super) think_level: ThinkLevel,
    pub(super) edit_mode: EditMode,
    pub(super) tool_limits: ToolLimits,
    pub(super) base_system: String,
    pub(super) prompt_source: Option<crate::system_prompt::Snapshot>,
    pub(super) session: Session,
    pub(super) workflow: WorkflowState,
}

#[derive(Clone, Copy, Serialize)]
#[serde(tag = "protocol", content = "request", rename_all = "lowercase")]
pub(super) enum SessionRef<'a> {
    Completion(&'a CreateChatCompletionRequest),
    Response(&'a CreateResponse),
}

impl<'a> From<&'a Session> for SessionRef<'a> {
    fn from(session: &'a Session) -> Self {
        match session {
            Session::Completion(request) => Self::Completion(request),
            Session::Response(request) => Self::Response(request),
        }
    }
}
impl SessionRef<'_> {
    pub(in crate::agent) fn len(self) -> usize {
        match self {
            Self::Completion(request) => request.messages.len(),
            Self::Response(request) => match &request.input {
                InputParam::Items(items) => items.len(),
                _ => unreachable!("native Responses input must be items"),
            },
        }
    }
}

#[derive(Serialize)]
pub(super) struct Metadata<'a> {
    pub(super) role_route: Option<&'a pp_config::config::RoleRoute>,
    pub(super) cwd: &'a Path,
    pub(super) model_reference: &'a str,
    pub(super) think_level: ThinkLevel,
    pub(super) edit_mode: EditMode,
    pub(super) tool_limits: ToolLimits,
    pub(super) base_system: &'a str,
    pub(super) prompt_source: Option<&'a crate::system_prompt::Snapshot>,
}

struct Active {
    id: String,
    created_at_ms: u64,
    file: File,
    _lock: SessionLock,
    truncate_to: Option<u64>,
    header_written: bool,
    tail: Option<String>,
    used_ids: HashSet<String>,
    storage: Storage,
}

pub(super) struct SessionJournal {
    dir: PathBuf,
    lock_dir: PathBuf,
    /// Stats database handle; supplied by the `Agent` constructor from
    /// `pp_stats::database_path` or a temporary directory.
    stats: pp_stats::Database,
    ephemeral: bool,
    active: Option<Active>,
    committed: usize,
    capture: replay::Capture,
    buffer: Vec<u8>,
    first_error: Option<String>,
    notice: Option<String>,
    model_reference: String,
    wire_model: String,
    thinking: String,
    capture_thinking: bool,
    pending_checkpoint: bool,
    pending_tools: bool,
    pending_stats: Option<String>,
    /// Request key of the most recent successful stats write; tool executions are
    /// associated with a request through it.
    last_stats: Option<String>,
    /// Tool call ID → (tool name, start instant), used to compute the duration on
    /// ToolFinished.
    tool_started: HashMap<String, (String, Instant)>,
    /// Tool stats whose status and duration are frozen and which await the final
    /// model-facing text.
    pending_tool_stats: HashMap<String, PendingToolStat>,
    unlinked_stats: Arc<Mutex<Vec<String>>>,
    first_request: Arc<AtomicBool>,
}

impl SessionJournal {
    pub(super) fn directory(&self) -> &Path {
        &self.dir
    }

    pub(in crate::agent) fn active_transcript_path(&mut self) -> Result<Option<PathBuf>> {
        self.check()?;
        let Some(active) = &mut self.active else {
            return Ok(None);
        };
        active.file.flush().context("flushing session transcript")?;
        Ok(Some(self.dir.join(format!("{}.jsonl", active.id))))
    }

    #[cfg(test)]
    pub(super) fn new(dir: PathBuf) -> Self {
        let lock_dir = dir.join(".locks");
        let stats_db = dir.join(".stats/stats.db");
        Self::with_locks(dir, lock_dir, stats_db)
    }

    pub(super) fn with_locks(
        dir: PathBuf,
        lock_dir: PathBuf,
        stats: impl Into<pp_stats::Database>,
    ) -> Self {
        Self {
            dir,
            lock_dir,
            stats: stats.into(),
            ephemeral: false,
            active: None,
            committed: 0,
            capture: replay::Capture::default(),
            buffer: Vec::new(),
            first_error: None,
            notice: None,
            model_reference: String::new(),
            wire_model: String::new(),
            thinking: String::new(),
            capture_thinking: false,
            pending_checkpoint: false,
            pending_tools: false,
            pending_stats: None,
            last_stats: None,
            tool_started: HashMap::new(),
            pending_tool_stats: HashMap::new(),
            unlinked_stats: Default::default(),
            first_request: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(super) fn ephemeral(
        dir: PathBuf,
        lock_dir: PathBuf,
        stats: impl Into<pp_stats::Database>,
    ) -> Self {
        Self {
            ephemeral: true,
            ..Self::with_locks(dir, lock_dir, stats)
        }
    }

    pub(super) fn is_ephemeral(&self) -> bool {
        self.ephemeral
    }

    pub(super) fn id(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.id.as_str())
    }
    pub(super) fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }
    pub(super) fn reset(&mut self) {
        let ephemeral = self.ephemeral;
        *self = Self::with_locks(
            std::mem::take(&mut self.dir),
            std::mem::take(&mut self.lock_dir),
            self.stats.clone(),
        );
        self.ephemeral = ephemeral;
    }
    pub(super) fn lock_dir(&self) -> &Path {
        &self.lock_dir
    }
    pub(super) fn tools_changed(&mut self) {
        self.pending_tools = true;
    }
    pub(super) fn history_replaced(&mut self) {
        // Restore only: do not change the journal or ordering; the new model and native
        // history are saved after the next request is sent.
        self.pending_checkpoint = true;
    }
    pub(super) fn check(&self) -> Result<()> {
        if let Some(error) = &self.first_error {
            bail!("{error}");
        }
        Ok(())
    }
    fn record(&mut self, payload: PayloadRef<'_>, sync: bool) -> Result<()> {
        self.check()?;
        if self.active.is_none() {
            return Ok(());
        }
        let result = (|| -> Result<()> {
            let timestamp = timestamp(now_ms()?)?;
            let id = self.next_id()?;
            let parent = self.active.as_ref().and_then(|active| active.tail.clone());
            let entry = EntryRef {
                id: &id,
                parent_id: parent.as_deref(),
                timestamp: &timestamp,
                payload,
            };
            let physical = {
                let active = self.active.as_mut().expect("active journal checked");
                let mut line = if active.header_written {
                    // Single serialization: the record value enters the shared value table
                    // directly instead of re-reading bytes into a Value.
                    let value =
                        serde_json::to_value(&entry).context("serializing session record")?;
                    active
                        .storage
                        .encode(&value)
                        .context("serializing session record")?
                } else {
                    serde_json::to_vec(&entry).context("serializing session record")?
                };
                line.push(b'\n');
                line
            };
            self.append_physical(&physical, sync)?;
            let active = self.active.as_mut().expect("active journal checked");
            active.used_ids.insert(id.clone());
            active.tail = Some(id);
            Ok(())
        })();
        self.latch(result)
    }
    fn next_id(&self) -> Result<String> {
        let active = self.active.as_ref().expect("active journal checked");
        loop {
            let mut bytes = [0; 4];
            getrandom::fill(&mut bytes)
                .map_err(|_| anyhow::anyhow!("session OS random source failed"))?;
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let mut id = String::with_capacity(8);
            for byte in bytes {
                id.push(char::from(HEX[usize::from(byte >> 4)]));
                id.push(char::from(HEX[usize::from(byte & 15)]));
            }
            if !active.used_ids.contains(&id) {
                return Ok(id);
            }
        }
    }
    fn latch(&mut self, result: Result<()>) -> Result<()> {
        if let Err(error) = result {
            if self.first_error.is_none() {
                self.first_error = Some(format!("persisting session: {error:#}"));
            }
            bail!("{}", self.first_error.as_ref().expect("error latched"));
        }
        Ok(())
    }
    fn custom(&mut self, custom: CustomRef<'_>, sync: bool) -> Result<()> {
        self.record(PayloadRef::Custom { custom }, sync)
    }
    /// Persist one encoded record: handle a pending incomplete tail first, then fsync as
    /// requested.
    fn append_physical(&mut self, physical: &[u8], sync: bool) -> Result<()> {
        let result = (|| -> Result<()> {
            let active = self.active.as_mut().expect("active journal checked");
            if let Some(offset) = active.truncate_to {
                active.file.set_len(offset)?;
                active.file.seek(SeekFrom::End(0))?;
                active.truncate_to = None;
            }
            active.file.write_all(physical)?;
            if sync {
                active.file.flush()?;
                active.file.sync_data()?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let id = self.active.as_ref().map_or("", |active| active.id.as_str());
            let message = format!("persisting session {id}: {error:#}");
            self.first_error = Some(message.clone());
            bail!("{message}");
        }
        Ok(())
    }
    /// The header record is written verbatim: the first line is not a shared value table
    /// record.
    fn append_serialized(&mut self, serialized: serde_json::Result<()>, sync: bool) -> Result<()> {
        serialized.context("serializing session record")?;
        self.buffer.push(b'\n');
        let physical = std::mem::take(&mut self.buffer);
        self.append_physical(&physical, sync)
    }

    /// The new context reuses the session and lock directories and holds the new
    /// session's own lock.
    pub(super) fn fork_empty(&self) -> Self {
        Self {
            ephemeral: self.ephemeral,
            ..Self::with_locks(self.dir.clone(), self.lock_dir.clone(), self.stats.clone())
        }
    }

    /// Test-only fault injection: swap the active descriptor for a read-only
    /// one so subsequent synced writes fail deterministically, even as root.
    #[cfg(test)]
    pub(super) fn use_read_only_file(&mut self, path: &Path) {
        self.active.as_mut().expect("active journal").file = File::open(path).unwrap();
    }

    pub(super) fn tree(&mut self) -> Result<tree::Tree> {
        self.check()?;
        let Some(active) = &mut self.active else {
            return Ok(tree::Tree::default());
        };
        active.file.flush().context("reading session tree")?;
        tree::Tree::read(&active.file)
    }

    pub(super) fn tree_view(&mut self) -> Result<tree::View> {
        let tree = self.tree()?;
        match &self.active {
            Some(active) => tree.view(&active.file),
            None => Ok(tree::View {
                tree,
                records: Vec::new(),
            }),
        }
    }

    pub(super) fn latest_conclusion(&mut self) -> Result<Option<String>> {
        let tree = self.tree()?;
        match &self.active {
            Some(active) => tree.latest_conclusion(&active.file),
            None => Ok(None),
        }
    }

    pub(super) fn prepare_tree(&mut self, id: &str) -> Result<tree::Prepared> {
        let tree = self.tree()?;
        let active = self.active.as_ref().context("empty session tree")?;
        tree.prepare(&active.file, id)
    }
}

#[cfg(test)]
pub(crate) fn indexed_listing(dir: &Path) -> Result<SessionListing> {
    // Every acceptance run explicitly uses a separate temporary database; a shared
    // /tmp/history.db must never be derived from the session directory.
    let database = tempfile::tempdir()?;
    let mut store = crate::history::Store::open(&database.path().join("history.db"))?;
    let diagnostics = store.sync_sessions(dir, &pp_core::cancellation::CancellationToken::new())?;
    Ok(SessionListing {
        sessions: store.search_sessions(None, "")?,
        diagnostics,
    })
}

/// Test-only: the stats database follows the session directory into a temporary
/// workspace, so the real `~/.pp/stats` is never written.
#[cfg(test)]
pub(super) fn load_test(
    dir: &Path,
    lock_dir: &Path,
    id: Option<&str>,
    include_replay: bool,
) -> Result<RestoredSession> {
    load(
        dir,
        lock_dir,
        id,
        include_replay,
        &dir.join(".stats/stats.db"),
    )
}

/// Frozen display-format fixtures enter UI tests through the same projector used by
/// production reads.
#[cfg(feature = "test-support")]
pub fn replay_fixture(lines: &str) -> Result<Vec<ReplayRecord>> {
    use format::Entry;
    let mut projector = replay::Projector::default();
    for line in lines.lines() {
        let entry: Entry = serde_json::from_str(line).context("invalid replay fixture")?;
        projector.feed(&entry.payload, line.as_bytes())?;
    }
    projector.finish()
}

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod slimming_tests;

/// Regression surface of the write path: single serialization may only reduce the number
/// of encodings, never change the on-disk bytes or the commit boundaries.
#[cfg(test)]
mod write_path_tests {
    use super::format::{Content, Message, StateRef};
    use super::*;
    use pp_config::models::Protocol;
    use pp_core::session_jsonl::Reader as SessionReader;
    use serde_json::{Value, json};
    use std::borrow::Cow;
    use std::fs;
    use std::io::BufReader;

    fn metadata(cwd: &Path) -> Metadata<'_> {
        Metadata {
            role_route: None,
            cwd,
            model_reference: "fixture/primary",
            think_level: ThinkLevel::None,
            edit_mode: EditMode::Replace,
            tool_limits: ToolLimits::default(),
            base_system: "system",
            prompt_source: None,
        }
    }

    fn session() -> Session {
        let mut session = Session::new(
            "fixture".into(),
            Protocol::Completion,
            "system".into(),
            EditMode::Replace,
        );
        session.set_think_level(ThinkLevel::None);
        session
    }

    fn open(dir: &Path) -> (SessionJournal, PathBuf) {
        let mut journal = SessionJournal::new(dir.to_owned());
        journal
            .sent(
                &metadata(dir),
                (&session()).into(),
                &WorkflowState::default(),
                None,
                None,
            )
            .unwrap();
        let path = dir.join(format!("{}.jsonl", journal.id().unwrap()));
        (journal, path)
    }

    /// The legacy path serializes to bytes and reads them back into a `Value`; the new
    /// path interns the record value directly.
    /// Both must be byte-identical: existing decode entry points and frozen logs depend on
    /// one and the same on-disk format.
    #[test]
    fn record_encoding_matches_the_legacy_byte_path() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = metadata(dir.path());
        let session = session();
        let workflow = WorkflowState::default();
        let state = StateRef::new(&metadata, (&session).into(), &workflow);
        let output = String::from("{\"ok\":false}");
        let tool_result = Message::ToolResult {
            tool_call_id: "call",
            tool_name: Some("read"),
            content: vec![Content::Text {
                text: Cow::Borrowed(&output),
            }],
            is_error: true,
            timestamp: 1_700_000_000_123,
        };
        let display = json!({
            "role": "assistant",
            "content": [{"type": "text", "text": "共享正文"}],
            "note": {"text": "共享正文"},
            "usage": {"input": 12, "output": 34, "total": 46.5},
        });
        let event = AgentEvent::ThinkingDelta {
            text: "thought".into(),
        };
        let payloads = [
            PayloadRef::Message {
                message: &tool_result,
            },
            PayloadRef::DisplayMessage { message: &display },
            PayloadRef::Custom {
                custom: CustomRef::State(state),
            },
            PayloadRef::Custom {
                custom: CustomRef::Event { event: &event },
            },
        ];
        for payload in payloads {
            let entry = EntryRef {
                id: "01234567",
                parent_id: Some("76543210"),
                timestamp: "2026-11-12T01:02:03.456Z",
                payload,
            };
            let legacy: Value =
                serde_json::from_slice(&serde_json::to_vec(&entry).unwrap()).unwrap();
            let mut legacy_storage = Storage::default();
            let mut storage = Storage::default();
            assert_eq!(
                storage
                    .encode(&serde_json::to_value(&entry).unwrap())
                    .unwrap(),
                legacy_storage.encode(&legacy).unwrap()
            );
        }
    }

    /// Decode the whole journal through the production read path.
    fn read_records(path: &Path) -> Vec<Value> {
        let mut reader = SessionReader::new(BufReader::new(File::open(path).unwrap()));
        let mut line = Vec::new();
        let mut decoded = Vec::new();
        while reader.read_record(&mut line).unwrap() != 0 {
            decoded.push(serde_json::from_slice::<Value>(&line).unwrap());
        }
        decoded
    }

    /// `sync=true` is a commit boundary: the record must already be in the file when the
    /// call returns and must not sit in a userspace buffer.
    #[test]
    fn synced_record_is_on_disk_before_it_returns() {
        let dir = tempfile::tempdir().unwrap();
        let (mut journal, path) = open(dir.path());
        journal.think(ThinkLevel::High).unwrap();
        let records = read_records(&path);
        let last = records.last().unwrap();
        assert_eq!(last["type"], "thinking_level_change");
        assert_eq!(last["thinkingLevel"], "high");
    }

    /// Files written by the new path must read back completely through the read path, and
    /// shared values must still be written only once.
    #[test]
    fn written_records_read_back_through_the_session_reader() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = metadata(dir.path());
        let session = session();
        let shared = "共享值表正文".repeat(64);
        let display = json!({
            "role": "assistant",
            "content": [{"type": "text", "text": shared}],
            "note": {"text": shared},
        });
        let mut journal = SessionJournal::new(dir.path().to_owned());
        journal
            .sent(
                &metadata,
                (&session).into(),
                &WorkflowState::default(),
                Some("user"),
                None,
            )
            .unwrap();
        let id = journal.id().unwrap().to_owned();
        let path = dir.path().join(format!("{id}.jsonl"));
        journal
            .record(PayloadRef::DisplayMessage { message: &display }, true)
            .unwrap();
        let records = read_records(&path);
        let record = records.last().unwrap();
        assert_eq!(record["type"], "message");
        assert_eq!(record["message"], display);
        assert_eq!(
            fs::read_to_string(&path).unwrap().matches(&shared).count(),
            1
        );
        drop(journal);
        let restored = load_test(dir.path(), &dir.path().join(".locks"), Some(&id), false).unwrap();
        assert_eq!(
            serde_json::to_value(&restored.checkpoint.session).unwrap(),
            serde_json::to_value(&session).unwrap()
        );
    }
}
