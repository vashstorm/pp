use super::Session;
use crate::{
    config::{EditMode, ThinkLevel, ToolLimits},
    events::AgentEvent,
};
use anyhow::{Context, Result, bail, ensure};
use async_openai::types::{
    chat::{ChatCompletionRequestMessage, CreateChatCompletionRequest},
    responses::{CreateResponse, InputItem, InputParam},
};
use chrono::{DateTime, Datelike, Local, NaiveDateTime};
use serde::{Deserialize, Serialize};
use std::{
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

#[derive(Deserialize)]
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

#[derive(Serialize)]
struct CheckpointRef<'a> {
    version: u32,
    id: &'a str,
    created_at_ms: u64,
    #[serde(flatten)]
    metadata: &'a Metadata<'a>,
    session: SessionRef<'a>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RecordRef<'a> {
    Checkpoint(CheckpointRef<'a>),
    Instructions {
        system: &'a str,
    },
    ThinkLevel {
        level: ThinkLevel,
    },
    AppendCompletion {
        messages: &'a [ChatCompletionRequestMessage],
    },
    AppendResponse {
        items: &'a [InputItem],
    },
    User {
        text: &'a str,
    },
    Event {
        event: &'a AgentEvent,
    },
    Error {
        message: &'a str,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Record {
    Checkpoint(Box<Checkpoint>),
    Instructions {
        system: String,
    },
    ThinkLevel {
        level: ThinkLevel,
    },
    AppendCompletion {
        messages: Vec<ChatCompletionRequestMessage>,
    },
    AppendResponse {
        items: Vec<InputItem>,
    },
    User {
        text: String,
    },
    Event {
        event: AgentEvent,
    },
    Error {
        message: String,
    },
}

struct Active {
    id: String,
    created_at_ms: u64,
    file: File,
    truncate_to: Option<u64>,
}

pub(super) struct SessionJournal {
    dir: PathBuf,
    active: Option<Active>,
    committed: usize,
    buffer: Vec<u8>,
    first_error: Option<String>,
    notice: Option<String>,
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
    fn record(&mut self, record: &RecordRef<'_>, sync: bool) -> Result<()> {
        self.check()?;
        if self.active.is_none() {
            return Ok(());
        }
        self.buffer.clear();
        let serialized = serde_json::to_writer(&mut self.buffer, record);
        self.append_serialized(serialized, sync)
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
        self.buffer.clear();
        let serialized = serde_json::to_writer(
            &mut self.buffer,
            &RecordRef::Checkpoint(CheckpointRef {
                version: 1,
                id: &active.id,
                created_at_ms: active.created_at_ms,
                metadata,
                session,
            }),
        );
        self.append_serialized(serialized, true)?;
        self.committed = session.len();
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
                self.record(&RecordRef::Instructions { system }, false)?;
            }
            self.commit(session)?;
        }
        if let Some(text) = user {
            self.record(&RecordRef::User { text }, true)?;
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
            match session {
                SessionRef::Completion(request) => self.record(
                    &RecordRef::AppendCompletion {
                        messages: &request.messages[self.committed..],
                    },
                    true,
                )?,
                SessionRef::Response(request) => {
                    let InputParam::Items(items) = &request.input else {
                        unreachable!()
                    };
                    self.record(
                        &RecordRef::AppendResponse {
                            items: &items[self.committed..],
                        },
                        true,
                    )?;
                }
            }
            self.committed = end;
        }
        Ok(())
    }
    pub(super) fn event(&mut self, event: &AgentEvent) {
        let _ = self.record(&RecordRef::Event { event }, false);
    }
    pub(super) fn error(&mut self, message: &str) {
        let _ = self.record(&RecordRef::Error { message }, true);
    }
    pub(super) fn think(&mut self, level: ThinkLevel) -> Result<()> {
        self.record(&RecordRef::ThinkLevel { level }, true)
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
    let mut checkpoint: Option<Checkpoint> = None;
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
            let record: Record = serde_json::from_slice(&line)
                .map_err(|_| anyhow::anyhow!("invalid session record"))?;
            if checkpoint.is_none() {
                ensure!(
                    matches!(record, Record::Checkpoint(_)),
                    "first record must be a checkpoint"
                );
            }
            match record {
                Record::Checkpoint(saved) => {
                    validate_checkpoint(&saved, &id)?;
                    if let Some(previous) = &checkpoint {
                        ensure!(
                            saved.created_at_ms == previous.created_at_ms
                                && saved.cwd == previous.cwd,
                            "checkpoint identity changed"
                        );
                    }
                    checkpoint = Some(*saved);
                }
                Record::Instructions { system } => {
                    match &mut checkpoint.as_mut().expect("checkpoint checked").session {
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
                Record::ThinkLevel { level } => {
                    let saved = checkpoint.as_mut().expect("checkpoint checked");
                    saved.think_level = level;
                    saved.session.set_think_level(level);
                }
                Record::AppendCompletion { messages } => {
                    let Session::Completion(request) =
                        &mut checkpoint.as_mut().expect("checkpoint checked").session
                    else {
                        bail!("append protocol mismatch");
                    };
                    request.messages.extend(messages);
                }
                Record::AppendResponse { items } => {
                    let Session::Response(request) =
                        &mut checkpoint.as_mut().expect("checkpoint checked").session
                    else {
                        bail!("append protocol mismatch");
                    };
                    let InputParam::Items(input) = &mut request.input else {
                        bail!("Responses input must be items");
                    };
                    input.extend(items);
                }
                Record::User { text } => {
                    if include_replay {
                        replay.push(ReplayRecord::User(text));
                    }
                }
                Record::Event { event } => {
                    if include_replay {
                        replay.push(ReplayRecord::Event(event));
                    }
                }
                Record::Error { message } => {
                    if include_replay {
                        replay.push(ReplayRecord::Error(message));
                    }
                }
            }
            Ok(())
        })();
        result.with_context(|| format!("loading session {} line {line_number}", path.display()))?;
        offset += count as u64;
    }
    let checkpoint = checkpoint.with_context(|| {
        format!(
            "loading session {} line 1: missing checkpoint",
            path.display()
        )
    })?;
    if truncated {
        eprintln!("Ignoring incomplete final record in {}", path.display());
    }
    let committed = SessionRef::from(&checkpoint.session).len();
    let created_at_ms = checkpoint.created_at_ms;
    Ok(RestoredSession {
        checkpoint,
        journal: SessionJournal {
            dir: dir.to_owned(),
            active: Some(Active {
                id,
                created_at_ms,
                file,
                truncate_to: truncated.then_some(offset),
            }),
            committed,
            buffer: Vec::new(),
            first_error: None,
            notice: None,
        },
        replay,
    })
}

fn validate_checkpoint(saved: &Checkpoint, id: &str) -> Result<()> {
    ensure!(saved.version == 1, "unsupported session version");
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
            let session = Session::new(
                "fixture".into(),
                protocol,
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
    fn write_failure_preserves_settings_and_blocks_requests_until_reset() {
        use crate::{agent::Agent, config::ModelCatalog, tools::Tools};
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models.jsonc");
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        fs::write(&models, serde_json::json!({"providers":[
            {"id":"a","protocol":"completion","api_key":"fixture", "base_url":format!("http://{}/v1", server.server_addr()),
             "models":[{"id":"one","model":"one"},{"id":"two","model":"two"}]},
            {"id":"b","protocol":"response","api_key":"fixture", "models":[{"id":"one","model":"other"}]}
        ]}).to_string()).unwrap();
        for target in ["a/two", "b/one", "effort"] {
            let catalog =
                ModelCatalog::from_files(&dir.path().join("settings.jsonc"), &models).unwrap();
            let mut agent = Agent::new(
                catalog,
                Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
                dir.path().join("sessions"),
            )
            .unwrap();
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
            assert_eq!(agent.current_model.reference, "a/one");
            assert_eq!(serde_json::to_value(&agent.session).unwrap(), native);
            assert_eq!(
                agent
                    .set_think_level(ThinkLevel::High)
                    .unwrap_err()
                    .to_string(),
                error
            );
            assert_eq!(agent.think_level, ThinkLevel::None);
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
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"invalid\n")
            .unwrap();
        let error = load(dir.path(), Some(&id), false).err().unwrap();
        assert!(format!("{error:#}").contains("line 2"));
    }
}
