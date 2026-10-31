mod format;
pub(crate) mod index;
pub(crate) mod locks;
mod summary;
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
use locks::SessionLock;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
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
    pub(super) role_route: Option<crate::config::RoleRoute>,
    version: u32,
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
    pub(super) role_route: Option<&'a crate::config::RoleRoute>,
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
}

pub(super) struct SessionJournal {
    dir: PathBuf,
    lock_dir: PathBuf,
    active: Option<Active>,
    committed: usize,
    buffer: Vec<u8>,
    first_error: Option<String>,
    notice: Option<String>,
    model_reference: String,
    thinking: String,
    capture_thinking: bool,
    pending_checkpoint: bool,
    pending_tools: bool,
    pending_stats: Option<String>,
    first_request: Arc<AtomicBool>,
}

impl SessionJournal {
    pub(super) fn directory(&self) -> &Path {
        &self.dir
    }
    pub(super) fn stats_context(
        &self,
        metadata: &Metadata<'_>,
        purpose: &'static str,
        skills: &[crate::skills::SkillHeader],
    ) -> crate::stats::RequestContext {
        crate::stats::RequestContext {
            model_reference: metadata.model_reference.to_owned(),
            cwd: metadata.cwd.to_owned(),
            purpose,
            session_file: self.id().map(|id| self.dir.join(format!("{id}.jsonl"))),
            first_request: self.first_request.clone(),
            skills: if self.first_request.load(Ordering::Relaxed) {
                skills.to_vec()
            } else {
                Vec::new()
            },
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

    #[cfg(test)]
    pub(super) fn new(dir: PathBuf) -> Self {
        let lock_dir = dir.join(".locks");
        Self::with_locks(dir, lock_dir)
    }

    pub(super) fn with_locks(dir: PathBuf, lock_dir: PathBuf) -> Self {
        Self {
            dir,
            lock_dir,
            active: None,
            committed: 0,
            buffer: Vec::new(),
            first_error: None,
            notice: None,
            model_reference: String::new(),
            thinking: String::new(),
            capture_thinking: false,
            pending_checkpoint: false,
            pending_tools: false,
            pending_stats: None,
            first_request: Arc::new(AtomicBool::new(true)),
        }
    }
    pub(super) fn id(&self) -> Option<&str> {
        self.active.as_ref().map(|active| active.id.as_str())
    }
    pub(super) fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }
    pub(super) fn reset(&mut self) {
        *self = Self::with_locks(
            std::mem::take(&mut self.dir),
            std::mem::take(&mut self.lock_dir),
        );
    }
    pub(super) fn lock_dir(&self) -> &Path {
        &self.lock_dir
    }
    pub(super) fn tools_changed(&mut self) {
        self.pending_tools = true;
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
            let result = self.write_header(metadata);
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
            self.write_first_records(metadata)?;
        }
        Ok(())
    }

    /// Write the session header exactly once; callers decide how to latch a
    /// failure (`checkpoint` latches, plan execution rolls the group back).
    fn write_header(&mut self, metadata: &Metadata<'_>) -> Result<()> {
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
    }

    /// The first-record audit pair that follows a freshly written header.
    fn write_first_records(&mut self, metadata: &Metadata<'_>) -> Result<()> {
        self.record(
            PayloadRef::ModelChange {
                model: metadata.model_reference,
                role: "default",
            },
            false,
        )?;
        self.think(metadata.think_level)
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
        self.pending_tools = false;
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

    /// 新上下文复用会话目录与锁目录，独立持有新会话的锁。
    pub(super) fn fork_empty(&self) -> Self {
        Self::with_locks(self.dir.clone(), self.lock_dir.clone())
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
    pub(super) fn commit_plan_handoff(
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
                self.write_header(metadata)?;
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
                self.write_first_records(metadata)?;
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
            create(&self.dir, &self.lock_dir, created_at_ms, || {
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
            if self.pending_tools {
                let tools = match session {
                    SessionRef::Completion(request) => serde_json::to_value(&request.tools)?,
                    SessionRef::Response(request) => serde_json::to_value(&request.tools)?,
                };
                self.custom(
                    CustomRef::ToolDefinitions {
                        version: 1,
                        tools: &tools,
                    },
                    false,
                )?;
                self.pending_tools = false;
            }
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
                | AgentEvent::TodoStateUpdated { .. }
                | AgentEvent::PlanProgressUpdated { .. }
                | AgentEvent::CompactionStarted { .. }
                | AgentEvent::CompactionFinished { .. }
                | AgentEvent::UserInputAccepted { .. }
                | AgentEvent::ModelRequestStarted
                | AgentEvent::ModelOutputProgress { .. }
                | AgentEvent::ModelRequestFinished { .. }
                | AgentEvent::RequestRetrying { .. }
        ) {
            return;
        }
        if self.capture_thinking && self.active.is_some() && self.first_error.is_none() {
            match event {
                AgentEvent::AssistantStarted | AgentEvent::ModelAttemptFailed { .. } => {
                    self.thinking.clear()
                }
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

fn create(
    dir: &Path,
    lock_dir: &Path,
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
        let Some(session_lock) = SessionLock::try_acquire(lock_dir, &id)? else {
            continue;
        };
        match OpenOptions::new()
            .read(true)
            .append(true)
            .create_new(true)
            .mode(0o600)
            .open(dir.join(format!("{id}.jsonl")))
        {
            Ok(file) => {
                return Ok(Active {
                    id,
                    created_at_ms,
                    file,
                    _lock: session_lock,
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

#[cfg(test)]
pub(crate) fn indexed_listing(dir: &Path) -> Result<SessionListing> {
    // 每次验收显式使用独立临时库，不能从会话目录推导共享 /tmp/history.db。
    let database = tempfile::tempdir()?;
    let mut store = crate::history::Store::open(&database.path().join("history.db"))?;
    let diagnostics = store.sync_sessions(dir, &crate::cancellation::CancellationToken::new())?;
    Ok(SessionListing {
        sessions: store.search_sessions(None, "")?,
        diagnostics,
    })
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

pub(crate) fn delete(dir: &Path, lock_dir: &Path, id: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    validate_id(id)?;
    // 当前会话及其他进程持有的会话都不可删除。
    let _lock = SessionLock::acquire(lock_dir, id)?;
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
    let current = fs::symlink_metadata(&path).context("checking session before deletion")?;
    ensure!(
        current.is_file() && current.dev() == metadata.dev() && current.ino() == metadata.ino(),
        "session changed before deletion"
    );
    fs::remove_file(path).context("deleting session")
}

pub(crate) fn load(
    dir: &Path,
    lock_dir: &Path,
    id: Option<&str>,
    include_replay: bool,
) -> Result<RestoredSession> {
    let id = match id {
        Some(id) => {
            validate_id(id)?;
            id.to_owned()
        }
        None => latest(dir)?,
    };
    let session_lock = SessionLock::acquire(lock_dir, &id)?;
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
                        role_route: state.role_route,
                        version: state.version,
                        id: header.id.clone(),
                        created_at_ms: parse_timestamp(&header.timestamp)?,
                        cwd: header.cwd.clone(),
                        model_reference: state.model_reference,
                        think_level: state.think_level,
                        edit_mode: state.edit_mode,
                        tool_limits: state.tool_limits,
                        base_system: state.base_system,
                        prompt_source: state.prompt_source,
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
                    custom: Custom::ToolDefinitions { version, tools },
                } => {
                    ensure!(version == 1, "unsupported saved tool definitions version");
                    checkpoint
                        .as_mut()
                        .context("tool definitions before pp session state")?
                        .session
                        .restore_tools(tools)?;
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
            lock_dir: lock_dir.to_owned(),
            active: Some(Active {
                id,
                created_at_ms,
                file,
                _lock: session_lock,
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
            pending_tools: false,
            pending_stats: None,
            first_request: Arc::new(AtomicBool::new(false)),
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
    if let Some(route) = &saved.role_route {
        crate::config::RoleAssignment {
            enabled: true,
            model: route.primary.model.clone(),
            effort: route.primary.effort,
            fallback: route.fallback.clone(),
        }
        .validate()?;
        ensure!(
            saved.model_reference == route.primary.model
                || route
                    .fallback
                    .as_ref()
                    .is_some_and(|f| f.model == saved.model_reference),
            "saved model does not match its role route"
        );
    }
    saved.tool_limits.validate()?;
    saved.workflow.validate()?;
    if let Some(source) = &saved.prompt_source {
        source.validate()?;
    }
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
mod tests;
