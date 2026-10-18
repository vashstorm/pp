mod format;

use super::Session;
use crate::{
    config::{EditMode, ThinkLevel, ToolLimits},
    events::AgentEvent,
};
use anyhow::{Context, Result, bail, ensure};
use async_openai::types::{
    chat::{ChatCompletionRequestMessage, CreateChatCompletionRequest},
    responses::{CreateResponse, InputParam},
};
use chrono::{DateTime, Datelike, Local, NaiveDateTime, SecondsFormat};
use format::{
    Custom, CustomRef, Entry, EntryRef, Header, HeaderRef, Message, NativeAppend, NativeAppendRef,
    Payload, PayloadRef, SavedMessage, StateRef,
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
}

impl SessionJournal {
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
        self.custom(CustomRef::State(StateRef::new(metadata, session)), true)?;
        self.committed = session.len();
        self.model_reference.clear();
        self.model_reference.push_str(metadata.model_reference);
        self.capture_thinking = matches!(session, SessionRef::Completion(_));
        self.thinking.clear();
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
    pub(super) fn sent(
        &mut self,
        metadata: &Metadata<'_>,
        session: SessionRef<'_>,
        user: Option<&str>,
        instructions: Option<&str>,
    ) -> Result<()> {
        self.check()?;
        if self.active.is_none() {
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
            self.checkpoint(metadata, session)?;
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
                for message in &messages {
                    self.record(PayloadRef::Message { message }, false)?;
                }
                self.custom(CustomRef::NativeAppend(append), true)?;
                self.committed = end;
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
        if self.capture_thinking && self.active.is_some() && self.first_error.is_none() {
            match event {
                AgentEvent::AssistantStarted => self.thinking.clear(),
                AgentEvent::ThinkingDelta { text } => self.thinking.push_str(text),
                _ => {}
            }
        }
        let _ = self.custom(CustomRef::Event { event }, false);
    }
    pub(super) fn error(&mut self, message: &str) {
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
                    };
                    validate_checkpoint(&saved, &id)?;
                    checkpoint = Some(saved);
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
                .sent(&metadata, (&session).into(), Some("user"), None)
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
                .sent(&metadata, (&session).into(), Some("user"), None)
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
    fn write_failure_preserves_settings_and_blocks_requests_until_reset() {
        use crate::{agent::Agent, config::ModelCatalog, tools::Tools};
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models.jsonc");
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
                dir.path().join("settings.jsonc"),
                serde_json::json!({"default_model":source}).to_string(),
            )
            .unwrap();
            let catalog =
                ModelCatalog::from_files(&dir.path().join("settings.jsonc"), &models).unwrap();
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
                .sent(&metadata, (&agent.session).into(), None, None)
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
                agent.continue_session(&mut |_| {}).unwrap_err().to_string(),
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
            .sent(&metadata, (&session).into(), Some("task"), None)
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
            .sent(&metadata, (&session).into(), None, None)
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
}
