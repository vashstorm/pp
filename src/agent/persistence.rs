mod format;
pub(crate) mod tree;

use super::Session;
use crate::{
    config::{EditMode, ThinkLevel, ToolLimits},
    events::AgentEvent,
    plan::{WorkMode, WorkflowState},
};
use anyhow::{Context, Result, bail, ensure};
use async_openai::types::{
    chat::{ChatCompletionRequestMessage, CreateChatCompletionRequest},
    responses::{CreateResponse, InputParam},
};
use chrono::{DateTime, Datelike, Local, NaiveDateTime, SecondsFormat};
use format::{
    Custom, CustomRef, Entry, EntryRef, Header, HeaderRef, Message, NativeAppend, NativeAppendRef,
    Payload, PayloadRef, SavedMessage, StateRef, WorkflowRef,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) enum ReplayRecord {
    User(String),
    Event(AgentEvent),
    Error(String),
    Cancelled,
}

pub(crate) struct RestoredSession {
    pub(super) checkpoint: Checkpoint,
    pub(super) journal: SessionJournal,
    pub(super) replay: Vec<ReplayRecord>,
}

impl RestoredSession {
    pub(crate) fn cwd(&self) -> &Path {
        &self.checkpoint.cwd
    }
    pub(crate) fn edit_mode(&self) -> EditMode {
        self.checkpoint.edit_mode
    }
    pub(crate) fn work_mode(&self) -> WorkMode {
        self.checkpoint.workflow.mode
    }
}

pub(super) struct Checkpoint {
    version: u32,
    id: String,
    created_at_ms: u64,
    pub(super) cwd: PathBuf,
    pub(super) model_reference: String,
    pub(super) think_level: ThinkLevel,
    pub(super) edit_mode: EditMode,
    pub(super) tool_limits: ToolLimits,
    pub(super) base_system: String,
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
    fn len(self) -> usize {
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
    pub(super) cwd: &'a Path,
    pub(super) model_reference: &'a str,
    pub(super) think_level: ThinkLevel,
    pub(super) edit_mode: EditMode,
    pub(super) tool_limits: ToolLimits,
    pub(super) base_system: &'a str,
}

struct Active {
    id: String,
    created_at_ms: u64,
    file: File,
    truncate_to: Option<u64>,
    header_written: bool,
    tail: Option<String>,
    used_ids: HashSet<String>,
}

pub(super) struct SessionJournal {
    dir: PathBuf,
    active: Option<Active>,
    committed: usize,
    buffer: Vec<u8>,
    first_error: Option<String>,
    notice: Option<String>,
    model_reference: String,
    thinking: String,
    capture_thinking: bool,
    pending_checkpoint: bool,
    pending_stats: Option<String>,
}

impl SessionJournal {
    pub(super) fn stats_context(
        &self,
        metadata: &Metadata<'_>,
        purpose: &'static str,
    ) -> crate::stats::RequestContext {
        crate::stats::RequestContext {
            model_reference: metadata.model_reference.to_owned(),
            cwd: metadata.cwd.to_owned(),
            purpose,
            session_file: self.id().map(|id| self.dir.join(format!("{id}.jsonl"))),
        }
    }
    pub(super) fn track_stats(&mut self, key: Option<String>, success: bool) {
        if !success {
            self.pending_stats = None;
        }
        if let Some(key) = key {
            if let Some(id) = self.id() {
                crate::stats::link(&key, &self.dir.join(format!("{id}.jsonl")), &[]);
            }
            if success {
                self.pending_stats = Some(key);
            }
        }
    }

    pub(super) fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            active: None,
            committed: 0,
            buffer: Vec::new(),
            first_error: None,
            notice: None,
            model_reference: String::new(),
            thinking: String::new(),
            capture_thinking: false,
            pending_checkpoint: false,
            pending_stats: None,
        }
    }
    pub(super) fn id(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.id.as_str())
    }
    pub(super) fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }
    pub(super) fn reset(&mut self) {
        *self = Self::new(std::mem::take(&mut self.dir));
    }
    pub(super) fn history_replaced(&mut self) {
        // 仅恢复时不改变日志和排序；下一次请求发送后再保存新的模型与原生历史。
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
            let active = self.active.as_ref().expect("active journal checked");
            let id = loop {
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
                    break id;
                }
            };
            self.buffer.clear();
            let serialized = serde_json::to_writer(
                &mut self.buffer,
                &EntryRef {
                    id: &id,
                    parent_id: active.tail.as_deref(),
                    timestamp: &timestamp,
                    payload,
                },
            );
            self.append_serialized(serialized, sync)?;
            let active = self.active.as_mut().expect("active journal checked");
            active.used_ids.insert(id.clone());
            active.tail = Some(id);
            Ok(())
        })();
        self.latch(result)
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
    fn append_serialized(&mut self, serialized: serde_json::Result<()>, sync: bool) -> Result<()> {
        let active = self
            .active
            .as_mut()
            .expect("active journal checked before serialization");
        let result = (|| -> Result<()> {
            serialized.context("serializing session record")?;
            self.buffer.push(b'\n');
            if let Some(offset) = active.truncate_to {
                active.file.set_len(offset)?;
                active.file.seek(SeekFrom::End(0))?;
                active.truncate_to = None;
            }
            active.file.write_all(&self.buffer)?;
            if sync {
                active.file.flush()?;
                active.file.sync_data()?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let message = format!("persisting session {}: {error:#}", active.id);
            self.first_error = Some(message.clone());
            bail!("{message}");
        }
        Ok(())
    }
    pub(super) fn checkpoint(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
    ) -> Result<()> {
        self.check()?;
        let Some(active) = &self.active else {
            return Ok(());
        };
        let first = !active.header_written;
        if first {
            let result = (|| -> Result<()> {
                let active = self.active.as_ref().expect("active journal checked");
                let timestamp = timestamp(active.created_at_ms)?;
                self.buffer.clear();
                let serialized = serde_json::to_writer(
                    &mut self.buffer,
                    &HeaderRef {
                        kind: "session",
                        version: 3,
                        id: &active.id,
                        timestamp: &timestamp,
                        cwd: metadata.cwd,
                    },
                );
                self.append_serialized(serialized, false)?;
                self.active
                    .as_mut()
                    .expect("active journal checked")
                    .header_written = true;
                Ok(())
            })();
            self.latch(result)?;
        } else {
            self.record(
                PayloadRef::ModelChange {
                    model: metadata.model_reference,
                    role: "default",
                },
                false,
            )?;
        }
        self.write_state(metadata, session, workflow)?;
        if first {
            self.record(
                PayloadRef::ModelChange {
                    model: metadata.model_reference,
                    role: "default",
                },
                false,
            )?;
            self.think(metadata.think_level)?;
        }
        Ok(())
    }
    /// Commit a complete native history without inventing a model-switch audit.
    /// The new state record is the logical commit point; old records remain.
    pub(super) fn replace_history(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
    ) -> Result<()> {
        self.check()?;
        ensure!(
            self.active
                .as_ref()
                .is_some_and(|active| active.header_written),
            "persisting session: missing session checkpoint"
        );
        self.write_state(metadata, session, workflow)
    }

    fn write_state(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
    ) -> Result<()> {
        self.write_tree_state(metadata, session, workflow, None)
    }

    pub(super) fn write_tree_state(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
        tree: Option<&tree::Selection>,
    ) -> Result<()> {
        let mut state = StateRef::new(metadata, session, workflow);
        state.tree = tree;
        // 导航失败时保留此前完整提交，避免半条或未同步的 state 在恢复时生效。
        self.synced_custom(CustomRef::State(state))?;
        self.committed = session.len();
        self.model_reference.clear();
        self.model_reference.push_str(metadata.model_reference);
        self.capture_thinking = matches!(session, SessionRef::Completion(_));
        self.thinking.clear();
        self.pending_checkpoint = false;
        Ok(())
    }

    /// Offset to truncate back to if a synced write group fails mid-way.
    fn rollback_offset(&self) -> Result<Option<u64>> {
        self.active
            .as_ref()
            .map(|active| {
                active
                    .file
                    .metadata()
                    .map(|m| active.truncate_to.unwrap_or(m.len()))
            })
            .transpose()
            .context("reading session rollback boundary")
    }

    fn rollback_group(&mut self, offset: Option<u64>, error: anyhow::Error) -> anyhow::Error {
        if let Some(offset) = offset
            && let Some(active) = &mut self.active
            && let Err(rollback) = active
                .file
                .set_len(offset)
                .and_then(|()| active.file.sync_data())
        {
            // 回滚失败不改变已登记的持久化错误：重复失败必须返回同一信息。
            eprintln!("Failed to roll back failed session state write: {rollback}");
        }
        error
    }

    /// Synced custom record commit: on failure truncate back to the previous
    /// length and sync so no half-written or unsynced state can restore.
    fn synced_custom(&mut self, custom: CustomRef<'_>) -> Result<()> {
        let rollback = self.rollback_offset()?;
        if let Err(error) = self.custom(custom, true) {
            return Err(self.rollback_group(rollback, error));
        }
        Ok(())
    }

    /// Persist a workflow-only update (mode changes, approvals, revisions).
    /// This is the transactional boundary for every permission grant.
    pub(super) fn write_workflow(&mut self, workflow: &WorkflowState) -> Result<()> {
        self.check()?;
        if self.active.is_none() {
            return Ok(());
        }
        self.synced_custom(CustomRef::Workflow(WorkflowRef {
            version: 1,
            workflow,
        }))
    }

    /// A blank journal for a fresh-context execution; only the sessions
    /// directory is shared with the planning session.
    pub(super) fn fork_empty(&self) -> Self {
        Self::new(self.dir.clone())
    }

    /// Test-only fault injection: swap the active descriptor for a read-only
    /// one so subsequent synced writes fail deterministically, even as root.
    #[cfg(test)]
    pub(super) fn use_read_only_file(&mut self, path: &Path) {
        self.active.as_mut().expect("active journal").file = File::open(path).unwrap();
    }

    /// Commit the approved plan execution handoff: the user projection and the
    /// native state (which already contains the user message) form one atomic
    /// group. Failure rolls both back and never reports a running task.
    pub(super) fn commit_plan_execution(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
        prompt: &str,
    ) -> Result<()> {
        self.check()?;
        self.ensure_active()?;
        let rollback = self.rollback_offset()?;
        let result = (|| -> Result<()> {
            let first = !self
                .active
                .as_ref()
                .expect("active journal after ensure_active")
                .header_written;
            if first {
                let active = self.active.as_ref().expect("active journal checked");
                let timestamp = timestamp(active.created_at_ms)?;
                self.buffer.clear();
                let serialized = serde_json::to_writer(
                    &mut self.buffer,
                    &HeaderRef {
                        kind: "session",
                        version: 3,
                        id: &active.id,
                        timestamp: &timestamp,
                        cwd: metadata.cwd,
                    },
                );
                self.append_serialized(serialized, false)?;
                self.active
                    .as_mut()
                    .expect("active journal checked")
                    .header_written = true;
            }
            let timestamp = now_ms()?;
            self.record(
                PayloadRef::Message {
                    message: &Message::User {
                        content: prompt,
                        timestamp,
                    },
                },
                true,
            )?;
            self.write_state(metadata, session, workflow)?;
            if first {
                self.record(
                    PayloadRef::ModelChange {
                        model: metadata.model_reference,
                        role: "default",
                    },
                    false,
                )?;
                self.think(metadata.think_level)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            return Err(self.rollback_group(rollback, error));
        }
        Ok(())
    }

    fn ensure_active(&mut self) -> Result<()> {
        if self.active.is_some() {
            return Ok(());
        }
        let created = (|| {
            let created_at_ms = now_ms()?;
            DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(&self.dir)
                .context("creating session directory")?;
            create(&self.dir, created_at_ms, || {
                let mut random = [0; 16];
                getrandom::fill(&mut random)
                    .map_err(|_| anyhow::anyhow!("session OS random source failed"))?;
                Ok(random)
            })
        })();
        self.active = Some(match created {
            Ok(active) => active,
            Err(error) => {
                let message = format!("persisting session: {error:#}");
                self.first_error = Some(message.clone());
                bail!("{message}");
            }
        });
        self.notice = self.id().map(str::to_owned);
        Ok(())
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

    pub(super) fn prepare_tree(&mut self, id: &str) -> Result<tree::Prepared> {
        let tree = self.tree()?;
        let active = self.active.as_ref().context("empty session tree")?;
        tree.prepare(&active.file, id)
    }

    pub(super) fn sent(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        workflow: &WorkflowState,
        user: Option<&str>,
        instructions: Option<&str>,
    ) -> Result<()> {
        self.check()?;
        self.ensure_active()?;
        let first = !self
            .active
            .as_ref()
            .expect("active journal after ensure_active")
            .header_written;
        if self.pending_checkpoint || first {
            self.checkpoint(metadata, session, workflow)?;
        } else {
            if let Some(system) = instructions {
                self.custom(CustomRef::Instructions { system }, false)?;
            }
            self.commit(session)?;
        }
        if let Some(text) = user {
            let result = now_ms().and_then(|timestamp| {
                self.record(
                    PayloadRef::Message {
                        message: &Message::User {
                            content: text,
                            timestamp,
                        },
                    },
                    true,
                )
            });
            self.latch(result)?;
        }
        Ok(())
    }
    pub(super) fn commit(&mut self, session: SessionRef<'_>) -> Result<()> {
        self.check()?;
        if self.active.is_none() {
            return Ok(());
        }
        let end = session.len();
        ensure!(
            self.committed <= end,
            "persisting session: invalid history cursor"
        );
        if end > self.committed {
            // Move only these small display buffers, never the native request/history.
            let model_reference = std::mem::take(&mut self.model_reference);
            let thinking = std::mem::take(&mut self.thinking);
            let result = (|| -> Result<()> {
                let timestamp = now_ms()?;
                let provider = model_reference
                    .split_once('/')
                    .map_or(model_reference.as_str(), |(provider, _)| provider);
                let (messages, append) = match session {
                    SessionRef::Completion(request) => {
                        let slice = &request.messages[self.committed..];
                        (
                            format::completion(
                                slice,
                                provider,
                                &request.model,
                                &thinking,
                                timestamp,
                            ),
                            NativeAppendRef::Completion { messages: slice },
                        )
                    }
                    SessionRef::Response(request) => {
                        let InputParam::Items(items) = &request.input else {
                            unreachable!()
                        };
                        let slice = &items[self.committed..];
                        (
                            format::response(
                                slice,
                                provider,
                                request.model.as_deref().unwrap_or_default(),
                                timestamp,
                            ),
                            NativeAppendRef::Response { items: slice },
                        )
                    }
                };
                let mut entries = Vec::new();
                for message in &messages {
                    self.record(PayloadRef::Message { message }, false)?;
                    if self.pending_stats.is_some()
                        && matches!(message, Message::Assistant { .. })
                        && let Ok(entry) = serde_json::from_slice::<serde_json::Value>(&self.buffer)
                    {
                        entries.push(entry);
                    }
                }
                self.custom(CustomRef::NativeAppend(append), true)?;
                self.committed = end;
                if let Some(key) = self.pending_stats.take()
                    && let Some(id) = self.id()
                {
                    crate::stats::link(&key, &self.dir.join(format!("{id}.jsonl")), &entries);
                }
                Ok(())
            })();
            self.model_reference = model_reference;
            self.thinking = thinking;
            self.thinking.clear();
            self.latch(result)?;
        }
        Ok(())
    }
    pub(super) fn event(&mut self, event: &AgentEvent) {
        if matches!(
            event,
            AgentEvent::ContextUpdated { .. }
                | AgentEvent::PlanProgressUpdated { .. }
                | AgentEvent::CompactionStarted { .. }
                | AgentEvent::CompactionFinished { .. }
                | AgentEvent::UserInputAccepted { .. }
                | AgentEvent::ModelRequestStarted
                | AgentEvent::ModelOutputProgress { .. }
                | AgentEvent::ModelRequestFinished { .. }
        ) {
            return;
        }
        if self.capture_thinking && self.active.is_some() && self.first_error.is_none() {
            match event {
                AgentEvent::AssistantStarted => self.thinking.clear(),
                AgentEvent::ThinkingDelta { text } => self.thinking.push_str(text),
                _ => {}
            }
        }
        let _ = self.custom(CustomRef::Event { event }, false);
    }
    pub(super) fn cancelled(&mut self) -> Result<()> {
        self.pending_stats = None;
        self.custom(CustomRef::Cancelled {}, true)
    }
    pub(super) fn error(&mut self, message: &str) {
        self.pending_stats = None;
        let _ = self.custom(CustomRef::Error { message }, true);
    }
    pub(super) fn think(&mut self, level: ThinkLevel) -> Result<()> {
        self.record(
            PayloadRef::ThinkingLevelChange {
                thinking_level: level,
            },
            true,
        )
    }
}

fn now_ms() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("session clock is before Unix epoch")?
        .as_millis()
        .try_into()
        .context("session clock overflow")
}

fn timestamp(milliseconds: u64) -> Result<String> {
    let datetime = i64::try_from(milliseconds)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .context("session clock overflow")?;
    Ok(datetime.to_rfc3339_opts(SecondsFormat::Millis, true))
}

fn parse_timestamp(timestamp: &str) -> Result<u64> {
    DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .and_then(|time| u64::try_from(time.timestamp_millis()).ok())
        .context("invalid session record: invalid timestamp")
}

const ID_TIME_FORMAT: &str = "%Y-%m-%dT%H-%M-%S";

pub(crate) fn validate_id(id: &str) -> Result<()> {
    let valid = id.rsplit_once('-').is_some_and(|(time, random)| {
        time.len() == 19
            && time.bytes().enumerate().all(|(index, byte)| match index {
                4 | 7 | 13 | 16 => byte == b'-',
                10 => byte == b'T',
                _ => byte.is_ascii_digit(),
            })
            && NaiveDateTime::parse_from_str(time, ID_TIME_FORMAT).is_ok()
            && random.len() == 32
            && random
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    ensure!(valid, "invalid session ID");
    Ok(())
}

fn lock(file: &File, id: &str) -> Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(|error| {
        if error == rustix::io::Errno::WOULDBLOCK {
            anyhow::anyhow!("session is already in use: {id}")
        } else {
            anyhow::anyhow!("locking session {id}: {error}")
        }
    })
}

fn create(
    dir: &Path,
    created_at_ms: u64,
    mut random: impl FnMut() -> Result<[u8; 16]>,
) -> Result<Active> {
    use std::fmt::Write as _;
    let created_at = i64::try_from(created_at_ms)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .context("session clock overflow")?
        .with_timezone(&Local);
    ensure!(
        (0..=9999).contains(&created_at.year()),
        "session clock year is out of range"
    );
    let time = created_at.format(ID_TIME_FORMAT);
    loop {
        let random = random()?;
        let mut id = String::with_capacity(52);
        write!(&mut id, "{time}-").expect("writing to String");
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for byte in random {
            id.push(char::from(HEX[usize::from(byte >> 4)]));
            id.push(char::from(HEX[usize::from(byte & 15)]));
        }
        match OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(format!("{id}.jsonl")))
        {
            Ok(file) => {
                lock(&file, &id)?;
                return Ok(Active {
                    id,
                    created_at_ms,
                    file,
                    truncate_to: None,
                    header_written: false,
                    tail: None,
                    used_ids: HashSet::new(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("persisting session file"),
        }
    }
}

fn latest(dir: &Path) -> Result<String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("no saved sessions found")
        }
        Err(error) => return Err(error).context("reading saved sessions"),
    };
    let mut latest = None;
    for entry in entries {
        let entry = entry.context("reading saved session entry")?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".jsonl")) else {
            continue;
        };
        if validate_id(id).is_err() {
            continue;
        }
        let candidate = (entry.metadata()?.modified()?, id.to_owned());
        if latest.as_ref().is_none_or(|old| &candidate > old) {
            latest = Some(candidate);
        }
    }
    latest.map(|(_, id)| id).context("no saved sessions found")
}

pub(crate) struct SessionSummary {
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) updated_at: SystemTime,
    pub(crate) summary: String,
    pub(crate) preview: String,
    pub(crate) bytes: u64,
    pub(crate) done: bool,
}

pub(crate) struct SessionListing {
    pub(crate) sessions: Vec<SessionSummary>,
    pub(crate) diagnostics: Vec<String>,
}

pub(crate) fn list(dir: &Path) -> Result<SessionListing> {
    let mut listing = SessionListing {
        sessions: Vec::new(),
        diagnostics: Vec::new(),
    };
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(listing),
        Err(error) => return Err(error).context("reading saved sessions"),
    };
    for entry in entries {
        let entry = entry.context("reading saved session entry")?;
        let name = entry.file_name();
        let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".jsonl")) else {
            continue;
        };
        if validate_id(id).is_err() {
            continue;
        }
        let result = (|| -> Result<Option<SessionSummary>> {
            if !entry.file_type()?.is_file() {
                return Ok(None);
            }
            read_summary(&entry.path(), id)
        })();
        match result {
            Ok(Some(summary)) => listing.sessions.push(summary),
            Ok(None) => {}
            Err(error) => listing
                .diagnostics
                .push(format!("Skipping session {id}: {error:#}")),
        }
    }
    listing
        .sessions
        .sort_unstable_by(|a, b| (b.updated_at, &b.id).cmp(&(a.updated_at, &a.id)));
    Ok(listing)
}

fn read_summary(path: &Path, id: &str) -> Result<Option<SessionSummary>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(path)
        .context("opening session")?;
    let metadata = file.metadata().context("reading session metadata")?;
    ensure!(metadata.is_file(), "session is not a regular file");
    let updated_at = metadata
        .modified()
        .context("reading session modification time")?;
    let mut reader = BufReader::new(&file);
    let mut line = Vec::new();
    reader
        .read_until(b'\n', &mut line)
        .context("reading session header")?;
    ensure!(line.ends_with(b"\n"), "incomplete session header");
    let header: Header =
        serde_json::from_slice(&line).map_err(|_| anyhow::anyhow!("invalid session header"))?;
    ensure!(
        header.kind == "session",
        "first record must be a session header"
    );
    ensure!(header.version == 3, "unsupported session version");
    ensure!(header.id == id, "session ID does not match filename");
    ensure!(
        header.cwd.is_absolute(),
        "session working directory must be absolute"
    );
    parse_timestamp(&header.timestamp)?;
    #[derive(Deserialize)]
    struct SummaryEntry {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        message: Option<SummaryMessage>,
        #[serde(default, rename = "customType")]
        custom_type: String,
        #[serde(default)]
        data: SummaryData,
    }
    #[derive(Default, Deserialize)]
    struct SummaryData {
        #[serde(default)]
        tree: Option<serde::de::IgnoredAny>,
    }
    #[derive(Deserialize)]
    #[serde(tag = "role")]
    enum SummaryMessage {
        #[serde(rename = "user")]
        User { content: String },
        #[serde(rename = "assistant")]
        Assistant {
            #[serde(default)]
            content: Vec<SummaryContent>,
            #[serde(default, rename = "stopReason")]
            stop_reason: String,
        },
        #[serde(other)]
        Other,
    }
    #[derive(Deserialize)]
    struct SummaryContent {
        #[serde(default, rename = "type")]
        kind: String,
        #[serde(default)]
        text: String,
    }
    let mut branched = false;
    let mut summary = None;
    let mut preview = String::new();
    let mut done = false;
    loop {
        line.clear();
        reader
            .read_until(b'\n', &mut line)
            .context("reading session record")?;
        if !line.ends_with(b"\n") {
            if !line.is_empty() {
                done = false;
            }
            break;
        }
        let entry: SummaryEntry = match serde_json::from_slice(&line) {
            Ok(entry) => entry,
            // 列表可展示已有摘要；损坏的尾部仍由完整恢复流程校验。
            Err(_) if summary.is_some() => {
                done = false;
                break;
            }
            Err(_) => bail!("invalid session record before first user message"),
        };
        branched |= entry.data.tree.is_some();
        if matches!(entry.custom_type.as_str(), "pp.error" | "pp.cancelled") {
            done = false;
        }
        if entry.kind == "message" {
            match entry.message {
                Some(SummaryMessage::User { content }) => {
                    summary.get_or_insert_with(|| compact_summary(&content, 80));
                    done = false;
                }
                Some(SummaryMessage::Assistant {
                    content,
                    stop_reason,
                }) => {
                    if preview.is_empty() {
                        preview = compact_summary(
                            &content
                                .iter()
                                .filter(|part| part.kind == "text")
                                .map(|part| part.text.as_str())
                                .collect::<Vec<_>>()
                                .join(" "),
                            200,
                        );
                    }
                    done = stop_reason == "stop";
                }
                _ => {}
            }
        }
    }
    if branched {
        let tree = tree::Tree::read(&file)?;
        let path = tree.path(tree.leaf);
        summary = Some(
            path.iter()
                .map(|i| &tree.nodes[*i])
                .find(|n| n.kind == tree::Kind::User)
                .map_or_else(
                    || "Untitled session".into(),
                    |n| compact_summary(&n.text, 80),
                ),
        );
        preview = path
            .iter()
            .map(|i| &tree.nodes[*i])
            .find(|n| n.kind == tree::Kind::Assistant)
            .map_or_else(String::new, |n| compact_summary(&n.text, 200));
        done = tree.leaf.is_some_and(|i| tree.nodes[i].done);
    }
    Ok(summary.map(|summary| SessionSummary {
        id: id.to_owned(),
        cwd: header.cwd,
        updated_at,
        summary,
        preview,
        bytes: metadata.len(),
        done,
    }))
}

fn compact_summary(text: &str, limit: usize) -> String {
    let mut result = String::new();
    let mut count = 0;
    let mut whitespace = false;
    for character in text.chars() {
        if character.is_whitespace() || character.is_control() {
            whitespace = !result.is_empty();
            continue;
        }
        if whitespace {
            if count == limit {
                result.push('…');
                break;
            }
            result.push(' ');
            count += 1;
            whitespace = false;
        }
        if count == limit {
            result.push('…');
            break;
        }
        result.push(character);
        count += 1;
    }
    result
}

pub(crate) fn delete(dir: &Path, id: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    validate_id(id)?;
    let path = dir.join(format!("{id}.jsonl"));
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(&path)
        .context("opening session for deletion")?;
    let metadata = file.metadata().context("reading session metadata")?;
    ensure!(metadata.is_file(), "session is not a regular file");
    // 当前会话及其他进程持有的会话都不可删除。
    lock(&file, id)?;
    let current = fs::symlink_metadata(&path).context("checking session before deletion")?;
    ensure!(
        current.is_file() && current.dev() == metadata.dev() && current.ino() == metadata.ino(),
        "session changed before deletion"
    );
    fs::remove_file(path).context("deleting session")
}

pub(crate) fn load(dir: &Path, id: Option<&str>, include_replay: bool) -> Result<RestoredSession> {
    let id = match id {
        Some(id) => {
            validate_id(id)?;
            id.to_owned()
        }
        None => latest(dir)?,
    };
    let path = dir.join(format!("{id}.jsonl"));
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(&path)
        .with_context(|| format!("opening saved session {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "saved session is not a regular file: {}",
        path.display()
    );
    lock(&file, &id)?;
    let mut reader = BufReader::new(&file);
    let mut line = Vec::new();
    let mut line_number = 0;
    let mut offset = 0;
    let mut truncated = false;
    let mut header: Option<Header> = None;
    let mut checkpoint: Option<Checkpoint> = None;
    let mut tail: Option<String> = None;
    let mut used_ids = HashSet::new();
    let mut replay = Vec::new();
    let mut has_tree = false;
    loop {
        line.clear();
        let count = reader.read_until(b'\n', &mut line).with_context(|| {
            format!(
                "reading session {} line {}",
                path.display(),
                line_number + 1
            )
        })?;
        if count == 0 {
            break;
        }
        line_number += 1;
        if !line.ends_with(b"\n") {
            truncated = true;
            break;
        }
        let result = (|| -> Result<()> {
            if header.is_none() {
                #[derive(Deserialize)]
                struct Kind<'a> {
                    #[serde(borrow, rename = "type")]
                    kind: &'a str,
                }
                let kind: Kind<'_> = serde_json::from_slice(&line)
                    .map_err(|_| anyhow::anyhow!("first record must be a session header"))?;
                ensure!(
                    kind.kind != "checkpoint",
                    "unsupported legacy session format"
                );
                ensure!(
                    kind.kind == "session",
                    "first record must be a session header"
                );
                let saved: Header = serde_json::from_slice(&line)
                    .map_err(|_| anyhow::anyhow!("invalid session record"))?;
                ensure!(
                    saved.kind == "session",
                    "first record must be a session header"
                );
                ensure!(saved.version == 3, "unsupported session version");
                ensure!(saved.id == id, "session ID does not match filename");
                ensure!(
                    saved.cwd.is_absolute(),
                    "session working directory must be absolute"
                );
                parse_timestamp(&saved.timestamp)?;
                header = Some(saved);
                return Ok(());
            }
            let entry: Entry = serde_json::from_slice(&line)
                .map_err(|_| anyhow::anyhow!("invalid session record"))?;
            ensure!(
                !entry.id.is_empty() && !used_ids.contains(&entry.id) && entry.parent_id == tail,
                "invalid session entry chain"
            );
            parse_timestamp(&entry.timestamp)?;
            match entry.payload {
                Payload::Custom {
                    custom: Custom::State(state),
                } => {
                    has_tree |= state.tree.is_some();
                    let header = header.as_ref().expect("header checked");
                    let saved = Checkpoint {
                        version: state.version,
                        id: header.id.clone(),
                        created_at_ms: parse_timestamp(&header.timestamp)?,
                        cwd: header.cwd.clone(),
                        model_reference: state.model_reference,
                        think_level: state.think_level,
                        edit_mode: state.edit_mode,
                        tool_limits: state.tool_limits,
                        base_system: state.base_system,
                        session: state.session,
                        workflow: state.workflow,
                    };
                    validate_checkpoint(&saved, &id)?;
                    checkpoint = Some(saved);
                }
                Payload::Custom {
                    custom: Custom::Workflow(record),
                } => {
                    ensure!(record.version == 1, "unsupported saved workflow version");
                    record.workflow.validate()?;
                    let state = checkpoint
                        .as_mut()
                        .context("state change before pp session state")?;
                    state.workflow = record.workflow;
                }
                Payload::Custom {
                    custom: Custom::Instructions { system },
                } => {
                    let saved = checkpoint
                        .as_mut()
                        .context("state change before pp session state")?;
                    match &mut saved.session {
                        Session::Completion(request) => {
                            let Some(ChatCompletionRequestMessage::System(message)) =
                                request.messages.first_mut()
                            else {
                                bail!("missing system message");
                            };
                            message.content = system.into();
                        }
                        Session::Response(request) => request.instructions = Some(system),
                    }
                }
                Payload::ThinkingLevelChange { thinking_level } => {
                    let saved = checkpoint
                        .as_mut()
                        .context("state change before pp session state")?;
                    saved.think_level = thinking_level;
                    saved.session.set_think_level(thinking_level);
                }
                Payload::Custom {
                    custom: Custom::NativeAppend(append),
                } => {
                    let saved = checkpoint
                        .as_mut()
                        .context("state change before pp session state")?;
                    match (&mut saved.session, append) {
                        (Session::Completion(request), NativeAppend::Completion { messages }) => {
                            request.messages.extend(messages)
                        }
                        (Session::Response(request), NativeAppend::Response { items }) => {
                            let InputParam::Items(input) = &mut request.input else {
                                bail!("Responses input must be items");
                            };
                            input.extend(items);
                        }
                        _ => bail!("append protocol mismatch"),
                    }
                }
                Payload::ModelChange { model, role } => {
                    let format::DefaultRole::Default = role;
                    ensure!(
                        model.split_once('/').is_some_and(
                            |(provider, model)| !provider.is_empty() && !model.is_empty()
                        ),
                        "invalid saved model reference"
                    );
                    // Auditing only: a subsequent native state is the model switch commit point.
                }
                Payload::Message {
                    message: SavedMessage::User { content, .. },
                } => {
                    if include_replay {
                        replay.push(ReplayRecord::User(content));
                    }
                }
                Payload::Message {
                    message: SavedMessage::Assistant { .. } | SavedMessage::ToolResult { .. },
                } => {}
                Payload::Custom {
                    custom: Custom::Event { event },
                } => {
                    if include_replay {
                        replay.push(ReplayRecord::Event(event));
                    }
                }
                Payload::Custom {
                    custom: Custom::Error { message },
                } => {
                    if include_replay {
                        replay.push(ReplayRecord::Error(message));
                    }
                }
                Payload::Custom {
                    custom: Custom::Cancelled {},
                } => {
                    if include_replay {
                        replay.push(ReplayRecord::Cancelled);
                    }
                }
            }
            used_ids.insert(entry.id.clone());
            tail = Some(entry.id);
            Ok(())
        })();
        result.with_context(|| format!("loading session {} line {line_number}", path.display()))?;
        offset += count as u64;
    }
    ensure!(
        header.is_some(),
        "loading session {} line 1: missing session header",
        path.display()
    );
    let checkpoint = checkpoint.with_context(|| {
        format!(
            "loading session {} line {line_number}: missing pp session state",
            path.display()
        )
    })?;
    if truncated {
        eprintln!("Ignoring incomplete final record in {}", path.display());
    }
    let committed = SessionRef::from(&checkpoint.session).len();
    let created_at_ms = checkpoint.created_at_ms;
    let model_reference = checkpoint.model_reference.clone();
    let capture_thinking = matches!(checkpoint.session, Session::Completion(_));
    if has_tree {
        let tree = tree::Tree::read(&file)?;
        if include_replay {
            replay = tree.replay(&file, tree.leaf, true)?;
        }
    }
    Ok(RestoredSession {
        checkpoint,
        journal: SessionJournal {
            dir: dir.to_owned(),
            active: Some(Active {
                id,
                created_at_ms,
                file,
                truncate_to: truncated.then_some(offset),
                header_written: true,
                tail,
                used_ids,
            }),
            committed,
            buffer: Vec::new(),
            first_error: None,
            notice: None,
            model_reference,
            thinking: String::new(),
            capture_thinking,
            pending_checkpoint: false,
            pending_stats: None,
        },
        replay,
    })
}

fn validate_checkpoint(saved: &Checkpoint, id: &str) -> Result<()> {
    ensure!(saved.version == 1, "unsupported pp session version");
    ensure!(saved.id == id, "session ID does not match filename");
    ensure!(
        saved.cwd.is_absolute(),
        "session working directory must be absolute"
    );
    ensure!(
        saved
            .model_reference
            .split_once('/')
            .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty()),
        "invalid saved model reference"
    );
    saved.tool_limits.validate()?;
    saved.workflow.validate()?;
    match &saved.session {
        Session::Completion(request) => {
            ensure!(!request.model.is_empty(), "missing native model");
            ensure!(
                matches!(
                    request.messages.first(),
                    Some(ChatCompletionRequestMessage::System(_))
                ),
                "Completion first message must be system"
            );
        }
        Session::Response(request) => {
            ensure!(
                request
                    .model
                    .as_ref()
                    .is_some_and(|model| !model.is_empty()),
                "missing native model"
            );
            ensure!(
                matches!(request.input, InputParam::Items(_)),
                "Responses input must be items"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Protocol;

    #[test]
    fn session_id_requires_calendar_time_and_lowercase_random_suffix() {
        for time in ["2026-10-17T01-02-03", "2024-02-29T23-59-59"] {
            assert!(validate_id(&format!("{time}-{}", "0123456789abcdef".repeat(2))).is_ok());
        }
        for time in [
            "1700000000000",
            "2026-02-29T01-02-03",
            "2026-04-31T01-02-03",
            "2026-13-17T01-02-03",
            "2026-10-17T24-02-03",
            "2026-10-17T01-60-03",
            "2026-10-17T01:02:03",
            "2026-10-17t01-02-03",
            "2026-1-17T01-02-03",
            "../2026-10-17T01-02-03",
        ] {
            assert!(
                validate_id(&format!("{time}-{}", "00".repeat(16))).is_err(),
                "{time}"
            );
        }
        for random in [
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdeg",
            "0123456789abcdef0123456789abcdef/../escape",
        ] {
            assert!(validate_id(&format!("2026-10-17T01-02-03-{random}")).is_err());
        }
    }

    #[test]
    fn listing_reads_preview_size_and_latest_turn_status_without_native_content() {
        let dir = tempfile::tempdir().unwrap();
        let id = format!("2026-10-17T01-02-03-{}", "a".repeat(32));
        let path = dir.path().join(format!("{id}.jsonl"));
        let header = serde_json::json!({"type":"session", "version":3, "id":id,
            "timestamp":"2026-10-17T01:02:03Z", "cwd":"/project"});
        let mut data = format!(
            "{header}\n{}\n{}\n",
            serde_json::json!({"type":"message",
            "message":{"role":"user", "content":"  修复\n测试 "}}),
            serde_json::json!({"type":"message",
            "message":{"role":"assistant", "content":[{"type":"thinking", "thinking":"private thought"},
            {"type":"text", "text":"已经\n修复"}], "stopReason":"stop"}})
        );
        fs::write(&path, &data).unwrap();
        let listing = list(dir.path()).unwrap();
        let summary = &listing.sessions[0];
        assert_eq!(summary.summary, "修复 测试");
        assert_eq!(summary.preview, "已经 修复");
        assert_eq!(summary.bytes, data.len() as u64);
        assert!(summary.done);
        for tail in [
            "{\"type\":\"custom\",\"customType\":\"pp.error\",\"data\":{\"message\":\"safe error\"}}\n",
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"next turn\"}}\n",
            "{\"type\":\"custom\",\"customType\":\"pp.cancelled\",\"data\":{}}\n",
            "{partial",
        ] {
            let original = data.clone();
            data.push_str(tail);
            fs::write(&path, &data).unwrap();
            assert!(!list(dir.path()).unwrap().sessions[0].done);
            data = original;
        }
    }

    #[test]
    fn deletion_rejects_locked_sessions_symlinks_and_invalid_ids() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let id = format!("2026-10-17T01-02-03-{}", "a".repeat(32));
        let link_id = format!("2026-10-17T01-02-03-{}", "b".repeat(32));
        let path = dir.path().join(format!("{id}.jsonl"));
        fs::write(&path, "original bytes").unwrap();
        let guard = File::open(&path).unwrap();
        lock(&guard, &id).unwrap();
        assert!(
            delete(dir.path(), &id)
                .unwrap_err()
                .to_string()
                .contains("already in use")
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "original bytes");
        drop(guard);
        let link = dir.path().join(format!("{link_id}.jsonl"));
        symlink(&path, &link).unwrap();
        assert!(delete(dir.path(), &link_id).is_err());
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(delete(dir.path(), "../escape").is_err());
        delete(dir.path(), &id).unwrap();
        assert!(!path.exists());
        assert!(delete(dir.path(), &id).is_err());
    }

    #[test]
    fn listing_is_read_only_sorted_and_isolates_invalid_sessions() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        assert!(
            list(&dir.path().join("missing"))
                .unwrap()
                .sessions
                .is_empty()
        );
        let mut paths = Vec::new();
        for n in 0..5 {
            let id = format!("2026-10-17T01-02-03-{n:032x}");
            let path = dir.path().join(format!("{id}.jsonl"));
            let header = serde_json::json!({"type":"session", "version":3, "id":id, "timestamp":"2026-10-17T01:02:03Z", "cwd":format!("/project/{n}")});
            let user = if n == 0 {
                "中文".repeat(50)
            } else {
                "  first\n\t user\u{0001} message  ".into()
            };
            let mut data = format!("{header}\n");
            if n != 4 {
                data.push_str(&format!(
                    "{}\n",
                    serde_json::json!({"type":"custom","data":{"huge":"ignored"}})
                ));
                data.push_str(&format!("{}\n", serde_json::json!({"type":"message","message":{"role":"assistant","content":[]}})));
                if n == 3 {
                    data.push_str("broken before user\n");
                }
                data.push_str(&format!(
                    "{}\n",
                    serde_json::json!({"type":"message","message":{"role":"user","content":user}})
                ));
                data.push_str("broken after user\n{partial");
            }
            fs::write(&path, data).unwrap();
            File::open(&path)
                .unwrap()
                .set_times(fs::FileTimes::new().set_modified(
                    UNIX_EPOCH + std::time::Duration::from_secs(if n == 0 { 20 } else { 10 }),
                ))
                .unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
            paths.push(path);
        }
        fs::write(dir.path().join("invalid.jsonl"), "broken").unwrap();
        let bad = dir
            .path()
            .join(format!("2026-10-17T01-02-03-{}.jsonl", "f".repeat(32)));
        fs::write(&bad, "bad header\n").unwrap();
        let link = dir
            .path()
            .join(format!("2026-10-17T01-02-03-{}.jsonl", "e".repeat(32)));
        symlink(&paths[0], link).unwrap();
        let before: Vec<_> = paths
            .iter()
            .map(|p| {
                (
                    fs::read(p).unwrap(),
                    fs::metadata(p).unwrap().modified().unwrap(),
                )
            })
            .collect();
        let locked = File::open(&paths[0]).unwrap();
        lock(&locked, "listing-fixture").unwrap();
        let listing = list(dir.path()).unwrap();
        assert_eq!(
            listing
                .sessions
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            [0, 2, 1].map(|n| format!("2026-10-17T01-02-03-{n:032x}"))
        );
        assert_eq!(
            listing.sessions[0].summary,
            format!("{}…", "中文".repeat(40))
        );
        assert_eq!(listing.sessions[1].summary, "first user message");
        assert_eq!(listing.sessions[1].cwd, Path::new("/project/2"));
        assert_eq!(listing.diagnostics.len(), 2);
        assert!(
            listing
                .diagnostics
                .iter()
                .all(|d| !d.contains("broken before") && !d.contains("bad header"))
        );
        for (path, (bytes, modified)) in paths.iter().zip(before) {
            assert_eq!(fs::read(path).unwrap(), bytes);
            assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
        }
    }

    #[test]
    fn listing_rejects_invalid_headers_and_ignores_unfinished_user_records() {
        let dir = tempfile::tempdir().unwrap();
        let id = format!("2026-10-17T01-02-03-{}", "0".repeat(32));
        let path = dir.path().join(format!("{id}.jsonl"));
        let header = serde_json::json!({"type":"session", "version":3, "id":id, "timestamp":"2026-10-17T01:02:03Z", "cwd":"/project"});
        let user =
            serde_json::json!({"type":"message", "message":{"role":"user", "content":"first"}});
        for (key, value) in [
            ("type", serde_json::json!("message")),
            ("version", serde_json::json!(2)),
            ("id", serde_json::json!("mismatch")),
            ("timestamp", serde_json::json!("invalid")),
            ("cwd", serde_json::json!("relative")),
        ] {
            let mut invalid = header.clone();
            invalid[key] = value;
            fs::write(&path, format!("{invalid}\n{user}\n")).unwrap();
            let listing = list(dir.path()).unwrap();
            assert!(listing.sessions.is_empty(), "{key}");
            assert_eq!(listing.diagnostics.len(), 1, "{key}");
        }
        fs::write(&path, format!("{header}\n{user}")).unwrap();
        let listing = list(dir.path()).unwrap();
        assert!(listing.sessions.is_empty());
        assert!(listing.diagnostics.is_empty());
        fs::write(&path, format!("{header}\n{user}\n{{partial")).unwrap();
        let listing = list(dir.path()).unwrap();
        assert_eq!(listing.sessions[0].summary, "first");
        assert!(listing.diagnostics.is_empty());
        fs::write(&path, format!("{header}\n{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":[]}}}}\n")).unwrap();
        let listing = list(dir.path()).unwrap();
        assert!(listing.sessions.is_empty());
        assert_eq!(listing.diagnostics.len(), 1);
    }

    #[test]
    fn collision_preserves_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let time = DateTime::from_timestamp_millis(42)
            .unwrap()
            .with_timezone(&Local)
            .format(ID_TIME_FORMAT)
            .to_string();
        let path = dir.path().join(format!("{time}-{}.jsonl", "00".repeat(16)));
        fs::write(&path, "original").unwrap();
        let mut candidate = 0;
        let active = create(dir.path(), 42, || {
            let bytes = [candidate; 16];
            candidate += 1;
            Ok(bytes)
        })
        .unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "original");
        assert_eq!(active.id, format!("{time}-{}", "01".repeat(16)));
    }

    #[test]
    fn replay_native_history_and_repair_only_on_append() {
        for protocol in [Protocol::Completion, Protocol::Response] {
            let dir = tempfile::tempdir().unwrap();
            let mut session = Session::new(
                "fixture".into(),
                protocol,
                "system".into(),
                EditMode::Replace,
            );
            session.set_think_level(ThinkLevel::None);
            let metadata = Metadata {
                cwd: dir.path(),
                model_reference: "fixture/primary",
                think_level: ThinkLevel::None,
                edit_mode: EditMode::Replace,
                tool_limits: ToolLimits::default(),
                base_system: "system",
            };
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
            drop(journal);
            let complete = fs::read(&path).unwrap();
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"{unfinished")
                .unwrap();
            let mut restored = load(dir.path(), Some(&id), true).unwrap();
            assert_eq!(
                serde_json::to_value(&restored.checkpoint.session).unwrap(),
                serde_json::to_value(&session).unwrap()
            );
            assert!(
                matches!(restored.replay.as_slice(), [ReplayRecord::User(text)] if text == "user")
            );
            assert_eq!(fs::read(&path).unwrap().len(), complete.len() + 11);
            restored.journal.think(ThinkLevel::High).unwrap();
            drop(restored);
            let restored = load(dir.path(), Some(&id), false).unwrap();
            assert_eq!(restored.checkpoint.think_level, ThinkLevel::High);
            assert!(restored.replay.is_empty());
        }
    }

    #[test]
    fn incomplete_native_commit_never_restores_projections_or_model_audit() {
        for protocol in [Protocol::Completion, Protocol::Response] {
            let dir = tempfile::tempdir().unwrap();
            let mut session = Session::new(
                "fixture".into(),
                protocol,
                "system".into(),
                EditMode::Replace,
            );
            session.set_think_level(ThinkLevel::None);
            let metadata = Metadata {
                cwd: dir.path(),
                model_reference: "fixture/primary",
                think_level: ThinkLevel::None,
                edit_mode: EditMode::Replace,
                tool_limits: ToolLimits::default(),
                base_system: "system",
            };
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
            journal
                .record(
                    PayloadRef::Message {
                        message: &Message::Assistant {
                            api: "openai-completions",
                            provider: "fixture",
                            model: "fixture",
                            content: vec![format::Content::Text {
                                text: "not native committed".into(),
                            }],
                            stop_reason: "stop",
                            timestamp: 42,
                        },
                    },
                    false,
                )
                .unwrap();
            journal
                .record(
                    PayloadRef::ModelChange {
                        model: "fixture/not-committed",
                        role: "default",
                    },
                    false,
                )
                .unwrap();
            let previous_tail = journal.active.as_ref().unwrap().tail.clone();
            let id = journal.id().unwrap().to_owned();
            let path = dir.path().join(format!("{id}.jsonl"));
            drop(journal);
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(b"{\"type\":\"custom\",\"customType\":\"pp.native_append\"")
                .unwrap();
            let mut restored = load(dir.path(), Some(&id), true).unwrap();
            assert_eq!(restored.checkpoint.model_reference, "fixture/primary");
            assert_eq!(
                serde_json::to_value(&restored.checkpoint.session).unwrap(),
                serde_json::to_value(&session).unwrap()
            );
            assert!(
                matches!(restored.replay.as_slice(), [ReplayRecord::User(text)] if text == "user")
            );
            assert_eq!(
                restored.journal.active.as_ref().unwrap().tail,
                previous_tail
            );
            restored.journal.think(ThinkLevel::High).unwrap();
            drop(restored);
            let records: Vec<serde_json::Value> = fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(records.last().unwrap()["parentId"], previous_tail.unwrap());
            assert_eq!(
                records
                    .iter()
                    .filter(|record| record["type"] == "session")
                    .count(),
                1
            );
            assert_eq!(
                load(dir.path(), Some(&id), false)
                    .unwrap()
                    .checkpoint
                    .think_level,
                ThinkLevel::High
            );
        }
    }

    #[test]
    fn context_failed_history_replacement_restores_native_memory_and_disk() {
        use crate::{
            agent::Agent, cancellation::CancellationToken, config::ModelCatalog, tools::Tools,
        };
        use serde_json::json;
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let models = dir.path().join("models.yml");
            fs::write(&models, json!({"providers":[{"id":"fixture","protocol":protocol,"api_key":"fixture-key","base_url":format!("http://{}/v1",server.server_addr()),"models":[{"id":"primary","model":"fixture-model"}]}]}).to_string()).unwrap();
            let catalog =
                ModelCatalog::from_files(&dir.path().join("settings.yml"), &models).unwrap();
            let mut agent = Agent::new(
                catalog,
                Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
                dir.path().join("sessions"),
            )
            .unwrap();
            for index in 0..3 {
                let text = format!("old-{index} {}", "retained-data ".repeat(5000));
                match &mut agent.session {
                    Session::Completion(request) => {
                        request.messages.push(
                            serde_json::from_value(json!({"role":"user","content":text})).unwrap(),
                        );
                        request.messages.push(
                            serde_json::from_value(
                                json!({"role":"assistant","content":format!("answer-{index}")}),
                            )
                            .unwrap(),
                        );
                    }
                    Session::Response(request) => {
                        let InputParam::Items(input) = &mut request.input else {
                            panic!()
                        };
                        input.push(
                            serde_json::from_value(json!({"role":"user","content":text})).unwrap(),
                        );
                        input.push(serde_json::from_value(json!({"type":"message","id":format!("msg-{index}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":format!("answer-{index}"),"annotations":[],"logprobs":null}]})).unwrap());
                    }
                }
            }
            let metadata = Metadata {
                cwd: agent.tools.cwd(),
                model_reference: &agent.current_model.reference,
                think_level: agent.think_level,
                edit_mode: agent.tools.edit_mode(),
                tool_limits: agent.tools.limits,
                base_system: &agent.base_system,
            };
            agent
                .journal
                .sent(
                    &metadata,
                    (&agent.session).into(),
                    &WorkflowState::default(),
                    None,
                    None,
                )
                .unwrap();
            let id = agent.session_id().unwrap().to_owned();
            let path = agent.journal.dir.join(format!("{id}.jsonl"));
            let before = fs::read(&path).unwrap();
            let native = serde_json::to_value(&agent.session).unwrap();
            agent.journal.active.as_mut().unwrap().file = File::open(&path).unwrap();
            let responder = std::thread::spawn(move || {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .expect("summary request");
                let body: serde_json::Value = serde_json::from_reader(request.as_reader()).unwrap();
                assert!(body.get("tools").is_none_or(serde_json::Value::is_null));
                assert!(
                    !body.to_string().contains("old-2"),
                    "latest exchange must not enter summary request"
                );
                let sse = if protocol == "completion" {
                    format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        json!({"id":"summary","object":"chat.completion.chunk","created":1,"model":"fixture-model","choices":[{"index":0,"delta":{"role":"assistant","content":"short factual summary"},"finish_reason":"stop"}]})
                    )
                } else {
                    format!(
                        "data: {}\n\n",
                        json!({"type":"response.completed","sequence_number":1,"response":{"id":"summary","object":"response","created_at":1,"model":"fixture-model","status":"completed","output":[{"type":"message","id":"summary-message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"short factual summary","annotations":[],"logprobs":null}]}]}})
                    )
                };
                request
                    .respond(tiny_http::Response::from_string(sse).with_header(
                        tiny_http::Header::from_bytes("Content-Type", "text/event-stream").unwrap(),
                    ))
                    .unwrap();
                assert!(
                    server
                        .recv_timeout(std::time::Duration::from_millis(100))
                        .unwrap()
                        .is_none()
                );
            });
            let mut finished = false;
            let error = agent
                .compact(&CancellationToken::new(), &mut |event| {
                    finished |= matches!(event, AgentEvent::CompactionFinished { .. });
                })
                .unwrap_err()
                .to_string();
            responder.join().unwrap();
            assert!(error.contains("persisting session"), "{error}");
            assert!(
                !finished,
                "failed checkpoint cannot announce compaction success"
            );
            assert_eq!(serde_json::to_value(&agent.session).unwrap(), native);
            assert_eq!(fs::read(&path).unwrap(), before);
            assert!(agent.journal.check().is_err());
            drop(agent);
            let restored = load(&dir.path().join("sessions"), Some(&id), false).unwrap();
            assert_eq!(
                serde_json::to_value(&restored.checkpoint.session).unwrap(),
                native
            );
        }
    }

    #[test]
    fn write_failure_preserves_settings_and_blocks_requests_until_reset() {
        use crate::{agent::Agent, config::ModelCatalog, tools::Tools};
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models.yml");
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        fs::write(&models, serde_json::json!({"providers":[
            {"id":"a","protocol":"completion","api_key":"fixture", "base_url":format!("http://{}/v1", server.server_addr()),
             "models":[{"id":"one","model":"one"},{"id":"two","model":"two","thinking":{"defaultLevel":"max"}}]},
            {"id":"b","protocol":"response","api_key":"fixture", "models":[{"id":"one","model":"other","thinking":{"defaultLevel":"high"}},{"id":"two","model":"two","thinking":{"defaultLevel":"max"}}]}
        ]}).to_string()).unwrap();
        for (source, target) in [
            ("a/one", "a/two"),
            ("a/one", "b/one"),
            ("b/one", "b/two"),
            ("b/one", "a/one"),
            ("a/one", "effort"),
        ] {
            fs::write(
                dir.path().join("settings.yml"),
                serde_json::json!({"default_model":source}).to_string(),
            )
            .unwrap();
            let catalog =
                ModelCatalog::from_files(&dir.path().join("settings.yml"), &models).unwrap();
            let mut agent = Agent::new(
                catalog,
                Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
                dir.path().join("sessions"),
            )
            .unwrap();
            let original_level = agent.think_level;
            if let Session::Response(request) = &mut agent.session {
                request.reasoning.as_mut().unwrap().summary =
                    Some(async_openai::types::responses::ReasoningSummary::Detailed);
            }
            let metadata = Metadata {
                cwd: agent.tools.cwd(),
                model_reference: &agent.current_model.reference,
                think_level: agent.think_level,
                edit_mode: agent.tools.edit_mode(),
                tool_limits: agent.tools.limits,
                base_system: &agent.base_system,
            };
            agent
                .journal
                .sent(
                    &metadata,
                    (&agent.session).into(),
                    &WorkflowState::default(),
                    None,
                    None,
                )
                .unwrap();
            let path = agent
                .journal
                .dir
                .join(format!("{}.jsonl", agent.session_id().unwrap()));
            let before = fs::read(&path).unwrap();
            let native = serde_json::to_value(&agent.session).unwrap();
            // A read-only descriptor deterministically rejects writes even as root.
            agent.journal.active.as_mut().unwrap().file = File::open(&path).unwrap();
            let error = if target == "effort" {
                agent
                    .set_think_level(ThinkLevel::High)
                    .unwrap_err()
                    .to_string()
            } else {
                agent.switch_model(target).unwrap_err().to_string()
            };
            assert!(error.contains("persisting session"));
            assert_eq!(agent.current_model.reference, source);
            assert_eq!(serde_json::to_value(&agent.session).unwrap(), native);
            assert_eq!(
                agent
                    .set_think_level(ThinkLevel::High)
                    .unwrap_err()
                    .to_string(),
                error
            );
            assert_eq!(agent.think_level, original_level);
            assert_eq!(serde_json::to_value(&agent.session).unwrap(), native);
            assert_eq!(
                agent
                    .continue_session(
                        &crate::cancellation::CancellationToken::new(),
                        None,
                        &mut |_| {},
                    )
                    .unwrap_err()
                    .to_string(),
                error
            );
            assert!(
                server
                    .recv_timeout(std::time::Duration::from_millis(30))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(fs::read(&path).unwrap(), before);
            agent.reset();
            assert!(agent.session_id().is_none());
            agent.set_think_level(ThinkLevel::High).unwrap();
            assert_eq!(agent.think_level, ThinkLevel::High);
        }
    }

    #[test]
    fn initial_creation_failure_is_latched_without_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("sessions");
        fs::write(&blocked, "not a directory").unwrap();
        let session = Session::new(
            "fixture".into(),
            Protocol::Response,
            "system".into(),
            EditMode::Replace,
        );
        let metadata = Metadata {
            cwd: dir.path(),
            model_reference: "fixture/primary",
            think_level: ThinkLevel::None,
            edit_mode: EditMode::Replace,
            tool_limits: ToolLimits::default(),
            base_system: "system",
        };
        let mut journal = SessionJournal::new(blocked.clone());
        let error = journal
            .sent(
                &metadata,
                (&session).into(),
                &WorkflowState::default(),
                Some("task"),
                None,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("persisting session"));
        assert_eq!(journal.check().unwrap_err().to_string(), error);
        assert!(journal.id().is_none());
        assert_eq!(fs::read_to_string(blocked).unwrap(), "not a directory");
    }

    #[test]
    fn rejects_complete_corruption_and_locked_files() {
        let dir = tempfile::tempdir().unwrap();
        let session = Session::new(
            "fixture".into(),
            Protocol::Completion,
            "system".into(),
            EditMode::Replace,
        );
        let metadata = Metadata {
            cwd: dir.path(),
            model_reference: "fixture/primary",
            think_level: ThinkLevel::None,
            edit_mode: EditMode::Replace,
            tool_limits: ToolLimits::default(),
            base_system: "system",
        };
        let mut journal = SessionJournal::new(dir.path().to_owned());
        journal
            .sent(
                &metadata,
                (&session).into(),
                &WorkflowState::default(),
                None,
                None,
            )
            .unwrap();
        let id = journal.id().unwrap().to_owned();
        assert!(
            load(dir.path(), Some(&id), false)
                .err()
                .unwrap()
                .to_string()
                .contains("already in use")
        );
        drop(journal);
        let path = dir.path().join(format!("{id}.jsonl"));
        let next_line = fs::read(&path)
            .unwrap()
            .iter()
            .filter(|byte| **byte == b'\n')
            .count()
            + 1;
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"invalid\n")
            .unwrap();
        let error = load(dir.path(), Some(&id), false).err().unwrap();
        assert!(format!("{error:#}").contains(&format!("line {next_line}")));
    }

    fn plan_workflow() -> WorkflowState {
        WorkflowState {
            mode: WorkMode::Plan,
            plan: Some(crate::plan::PlanDocument {
                id: "0123456789abcdef0123456789abcdef".into(),
                revision: 1,
                title: "Create file".into(),
                content: "- [ ] Create hello.txt; done when bytes match.\n".into(),
                status: crate::plan::PlanStatus::Ready,
                origin_prompt: "Create hello.txt".into(),
            }),
            review: None,
        }
    }

    fn test_metadata(dir: &std::path::Path) -> Metadata<'_> {
        Metadata {
            cwd: dir,
            model_reference: "fixture/primary",
            think_level: ThinkLevel::None,
            edit_mode: EditMode::Replace,
            tool_limits: ToolLimits::default(),
            base_system: "system",
        }
    }

    fn rewrite_records(path: &Path, mutate: impl Fn(&mut serde_json::Value)) {
        let mut out = String::new();
        for line in fs::read_to_string(path).unwrap().lines() {
            let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
            mutate(&mut record);
            if record.is_null() {
                continue;
            }
            out.push_str(&record.to_string());
            out.push('\n');
        }
        fs::write(path, out).unwrap();
    }

    fn read_only(path: &Path, journal: &mut SessionJournal) {
        journal.use_read_only_file(path);
    }

    #[test]
    fn workflow_state_survives_restore_without_moving_native_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = test_metadata(dir.path());
        let mut session = Session::new(
            "fixture".into(),
            Protocol::Response,
            "system".into(),
            EditMode::Replace,
        );
        session.set_think_level(ThinkLevel::None);
        super::super::response_input(match &mut session {
            Session::Response(request) => request,
            _ => unreachable!(),
        })
        .push(
            serde_json::from_value(serde_json::json!({"role":"user","content":"plan task"}))
                .unwrap(),
        );
        let workflow = plan_workflow();
        let mut journal = SessionJournal::new(dir.path().to_owned());
        journal
            .sent(
                &metadata,
                (&session).into(),
                &workflow,
                Some("plan task"),
                None,
            )
            .unwrap();
        let id = journal.id().unwrap().to_owned();
        let path = dir.path().join(format!("{id}.jsonl"));
        let before = fs::read(&path).unwrap();
        let nodes_before = journal.tree().unwrap().nodes.len();

        // Workflow-only updates append their own strict versioned record and
        // never touch native history or the logical tree.
        let mut updated = workflow.clone();
        updated.plan.as_mut().unwrap().revision = 2;
        updated.plan.as_mut().unwrap().content = "- [x] Create hello.txt\n".into();
        journal.write_workflow(&updated).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""customType":"pp.workflow""#));
        assert!(text.contains(r#""version":1"#));
        assert!(fs::read(&path).unwrap().starts_with(&before));
        assert_eq!(journal.tree().unwrap().nodes.len(), nodes_before);
        drop(journal);

        let restored = load(dir.path(), Some(&id), true).unwrap();
        assert_eq!(restored.work_mode(), WorkMode::Plan);
        assert_eq!(restored.checkpoint.workflow, updated);
        assert!(
            matches!(restored.replay.as_slice(), [ReplayRecord::User(text)] if text == "plan task")
        );
        // The workflow record is not a replay action and does not move the
        // native cursor or the restore checkpoint's native history.
        assert_eq!(
            serde_json::to_value(&restored.checkpoint.session).unwrap(),
            serde_json::to_value(&session).unwrap()
        );
        assert_eq!(
            restored.journal.committed,
            SessionRef::from(&restored.checkpoint.session).len()
        );

        drop(restored);
        // A state record without a workflow field restores as Execute/None.
        rewrite_records(&path, |record| {
            if record["customType"] == "pp.session_state" {
                record["data"]
                    .as_object_mut()
                    .unwrap()
                    .remove("workflow")
                    .expect("state embeds workflow");
            }
            if record["customType"] == "pp.workflow" {
                *record = serde_json::Value::Null;
            }
        });
        let restored = load(dir.path(), Some(&id), false).unwrap();
        assert_eq!(restored.checkpoint.workflow, WorkflowState::default());
        assert_eq!(restored.work_mode(), WorkMode::Execute);
    }

    #[test]
    fn workflow_records_fail_closed_on_version_unknown_fields_and_privilege() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = test_metadata(dir.path());
        let session = Session::new(
            "fixture".into(),
            Protocol::Completion,
            "system".into(),
            EditMode::Replace,
        );
        let mut journal = SessionJournal::new(dir.path().to_owned());
        journal
            .sent(
                &metadata,
                (&session).into(),
                &WorkflowState::default(),
                None,
                None,
            )
            .unwrap();
        let id = journal.id().unwrap().to_owned();
        let path = dir.path().join(format!("{id}.jsonl"));
        drop(journal);

        let mut ready = plan_workflow();
        ready.plan.as_mut().unwrap().content = "sensitive-plan-body".into();
        let ready_json = serde_json::to_value(&ready).unwrap();
        let base = fs::read_to_string(&path).unwrap();
        let records = [
            (
                serde_json::json!({"type":"custom", "customType":"pp.workflow",
                    "data":{"version":2, "workflow":{"mode":"execute", "plan":null, "review":null}}}),
                ("unsupported saved workflow version", None),
            ),
            (
                serde_json::json!({"type":"custom", "customType":"pp.workflow",
                    "data":{"version":1, "workflow":{"mode":"plan", "plan":null, "review":null}, "extra":1}}),
                ("invalid session record", None),
            ),
            (
                serde_json::json!({"type":"custom", "customType":"pp.workflow",
                    "data":{"version":1, "workflow":{"mode":"admin", "plan":null, "review":null}}}),
                ("invalid session record", None),
            ),
            (
                serde_json::json!({"type":"custom", "customType":"pp.workflow",
                    "data":{"version":1, "workflow":{"mode":"execute", "plan":ready_json["plan"], "review":null}}}),
                ("Invalid saved workflow state.", Some("sensitive-plan-body")),
            ),
        ];
        for (mut record, (expected, secret)) in records {
            let entries: Vec<serde_json::Value> = base
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            record["id"] = serde_json::json!("workflow-corruption");
            record["parentId"] = entries.last().unwrap()["id"].clone();
            record["timestamp"] = entries[0]["timestamp"].clone();
            fs::write(&path, format!("{base}{record}\n")).unwrap();
            let error = format!("{:#}", load(dir.path(), Some(&id), false).err().unwrap());
            assert!(error.contains(expected), "{expected}: {error}");
            if let Some(secret) = secret {
                assert!(!error.contains(secret), "leaked saved content: {error}");
            }
        }

        // Corrupted embedded workflow inside pp.session_state also fails closed.
        rewrite_records(&path, |record| {
            if record["customType"] == "pp.session_state" {
                record["data"]["workflow"] = serde_json::json!({"mode":"admin"});
            }
            if record["customType"] == "pp.workflow" {
                *record = serde_json::Value::Null;
            }
        });
        let error = format!("{:#}", load(dir.path(), Some(&id), false).err().unwrap());
        assert!(error.contains("invalid session record"), "{error}");
    }

    #[test]
    fn failed_workflow_sync_rolls_back_and_blocks_until_reset() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = test_metadata(dir.path());
        let session = Session::new(
            "fixture".into(),
            Protocol::Completion,
            "system".into(),
            EditMode::Replace,
        );
        let mut journal = SessionJournal::new(dir.path().to_owned());
        journal
            .sent(
                &metadata,
                (&session).into(),
                &WorkflowState::default(),
                None,
                None,
            )
            .unwrap();
        let path = dir.path().join(format!("{}.jsonl", journal.id().unwrap()));
        let id = journal.id().unwrap().to_owned();
        let mut updated = plan_workflow();
        updated.plan.as_mut().unwrap().revision = 3;
        journal.write_workflow(&updated).unwrap();
        let mut next = plan_workflow();
        next.plan.as_mut().unwrap().revision = 4;
        let before = fs::read(&path).unwrap();

        // A read-only descriptor deterministically rejects writes even as root;
        // the failed sync must neither grant the newer revision nor corrupt the
        // file, and the last successfully synced state still restores.
        read_only(&path, &mut journal);
        let error = journal.write_workflow(&next).unwrap_err().to_string();
        assert!(error.contains("persisting session"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(journal.check().is_err());
        drop(journal);
        let restored = load(dir.path(), Some(&id), false).unwrap();
        assert_eq!(restored.work_mode(), WorkMode::Plan);
        assert_eq!(
            restored.checkpoint.workflow.plan.as_ref().unwrap().revision,
            3
        );
    }

    #[test]
    fn commit_plan_execution_commits_projection_and_state_atomically() {
        for protocol in [Protocol::Completion, Protocol::Response] {
            let dir = tempfile::tempdir().unwrap();
            let metadata = test_metadata(dir.path());
            let workflow = plan_workflow();
            let mut session = Session::new(
                "fixture".into(),
                protocol,
                "system".into(),
                EditMode::Replace,
            );
            match &mut session {
                Session::Completion(request) => request.messages.push(
                    serde_json::from_value(serde_json::json!(
                        {"role":"user", "content":"implement the approved plan"}
                    ))
                    .unwrap(),
                ),
                Session::Response(request) => {
                    let input = super::super::response_input(request);
                    input.push(
                        serde_json::from_value(serde_json::json!(
                            {"role":"user", "content":"implement the approved plan"}
                        ))
                        .unwrap(),
                    );
                }
            }
            session.set_think_level(ThinkLevel::None);
            let mut journal = SessionJournal::new(dir.path().to_owned());
            journal
                .commit_plan_execution(
                    &metadata,
                    (&session).into(),
                    &workflow,
                    "implement the approved plan",
                )
                .unwrap();
            let id = journal.id().unwrap().to_owned();
            assert_eq!(journal.take_notice().as_deref(), Some(id.as_str()));
            let path = dir.path().join(format!("{id}.jsonl"));
            let text = fs::read_to_string(&path).unwrap();
            let first: serde_json::Value =
                serde_json::from_str(text.lines().next().unwrap()).unwrap();
            assert_eq!(first["type"], "session");
            drop(journal);
            let restored = load(dir.path(), Some(&id), true).unwrap();
            assert_eq!(restored.work_mode(), WorkMode::Plan);
            assert_eq!(restored.checkpoint.workflow, workflow);
            // The user projection appears exactly once; the native history
            // already contains the same message at the sync commit point.
            assert!(
                matches!(restored.replay.as_slice(), [ReplayRecord::User(text)] if text == "implement the approved plan")
            );
            assert_eq!(
                serde_json::to_value(&restored.checkpoint.session).unwrap(),
                serde_json::to_value(&session).unwrap()
            );
            assert_eq!(
                restored.journal.committed,
                SessionRef::from(&restored.checkpoint.session).len()
            );

            // fork_empty shares only the sessions directory.
            let mut planner = SessionJournal::new(dir.path().to_owned());
            planner
                .sent(
                    &metadata,
                    (&session).into(),
                    &WorkflowState::default(),
                    None,
                    None,
                )
                .unwrap();
            let planned_id = planner.id().unwrap().to_owned();
            let planned_path = dir.path().join(format!("{planned_id}.jsonl"));
            let planned_before = fs::read(&planned_path).unwrap();
            let mut forked = planner.fork_empty();
            assert!(forked.id().is_none());
            forked
                .commit_plan_execution(
                    &metadata,
                    (&session).into(),
                    &workflow,
                    "implement the approved plan",
                )
                .unwrap();
            let forked_id = forked.id().unwrap().to_owned();
            assert_ne!(forked_id, planned_id);
            drop(forked);
            // The planning journal and its file are untouched by the fork.
            assert_eq!(fs::read(&planned_path).unwrap(), planned_before);
        }
    }

    #[test]
    fn commit_plan_execution_failure_rolls_back_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = test_metadata(dir.path());
        let mut session = Session::new(
            "fixture".into(),
            Protocol::Response,
            "system".into(),
            EditMode::Replace,
        );
        let mut journal = SessionJournal::new(dir.path().to_owned());
        journal
            .sent(
                &metadata,
                (&session).into(),
                &WorkflowState::default(),
                Some("first"),
                None,
            )
            .unwrap();
        let path = dir.path().join(format!("{}.jsonl", journal.id().unwrap()));
        let id = journal.id().unwrap().to_owned();
        if let Session::Response(request) = &mut session {
            super::super::response_input(request).push(
                serde_json::from_value(
                    serde_json::json!({"role":"user", "content":"implement the approved plan"}),
                )
                .unwrap(),
            );
        }
        let before = fs::read(&path).unwrap();
        read_only(&path, &mut journal);
        let error = journal
            .commit_plan_execution(
                &metadata,
                (&session).into(),
                &plan_workflow(),
                "implement the approved plan",
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("persisting session"), "{error}");
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(journal.check().is_err());
        drop(journal);
        let restored = load(dir.path(), Some(&id), true).unwrap();
        // Neither the execution projection nor the workflow grant survived.
        assert!(
            matches!(restored.replay.as_slice(), [ReplayRecord::User(text)] if text == "first")
        );
        assert_eq!(restored.work_mode(), WorkMode::Execute);
        assert!(restored.checkpoint.workflow.plan.is_none());
    }

    #[test]
    fn commit_plan_execution_creation_failure_is_latched_without_a_session() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("sessions");
        fs::write(&blocked, "not a directory").unwrap();
        let session = Session::new(
            "fixture".into(),
            Protocol::Response,
            "system".into(),
            EditMode::Replace,
        );
        let mut journal = SessionJournal::new(blocked.clone());
        let error = journal
            .commit_plan_execution(
                &test_metadata(dir.path()),
                (&session).into(),
                &plan_workflow(),
                "implement the approved plan",
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("persisting session"), "{error}");
        assert!(journal.id().is_none());
        assert_eq!(fs::read_to_string(blocked).unwrap(), "not a directory");
    }
}
