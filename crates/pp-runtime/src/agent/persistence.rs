mod format;
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
pub use listing::{SessionListing, SessionSummary, delete, validate_id};
pub(in crate::agent::persistence) use listing::{
    compact_summary, create, latest, now_ms, parse_timestamp, timestamp,
};
pub use load::load;
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
    /// 统计库路径；由 `Agent` 构造方从 `pp_stats::database_path` 或临时目录传入。
    stats_db: PathBuf,
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
    /// 最近一次成功写入统计库的请求键；工具执行结果按它关联请求。
    last_stats: Option<String>,
    /// 工具调用 ID → (工具名, 开始时刻)，用于在 ToolFinished 时计算耗时。
    tool_started: HashMap<String, (String, Instant)>,
    /// 已冻结状态与耗时、等待最终回填模型文本的工具统计。
    pending_tool_stats: HashMap<String, PendingToolStat>,
    unlinked_stats: Arc<Mutex<Vec<String>>>,
    first_request: Arc<AtomicBool>,
}

impl SessionJournal {
    pub(super) fn directory(&self) -> &Path {
        &self.dir
    }

    #[cfg(test)]
    pub(super) fn new(dir: PathBuf) -> Self {
        let lock_dir = dir.join(".locks");
        let stats_db = dir.join(".stats/stats.db");
        Self::with_locks(dir, lock_dir, stats_db)
    }

    pub(super) fn with_locks(dir: PathBuf, lock_dir: PathBuf, stats_db: PathBuf) -> Self {
        Self {
            dir,
            lock_dir,
            stats_db,
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
            std::mem::take(&mut self.stats_db),
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
            let mut physical = if active.header_written {
                active
                    .storage
                    .encode(&serde_json::from_slice(&self.buffer)?)?
            } else {
                self.buffer.clone()
            };
            if !physical.ends_with(b"\n") {
                physical.push(b'\n');
            }
            active.file.write_all(&physical)?;
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

    /// 新上下文复用会话目录与锁目录，独立持有新会话的锁。
    pub(super) fn fork_empty(&self) -> Self {
        Self::with_locks(
            self.dir.clone(),
            self.lock_dir.clone(),
            self.stats_db.clone(),
        )
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

    pub(super) fn prepare_tree(&mut self, id: &str) -> Result<tree::Prepared> {
        let tree = self.tree()?;
        let active = self.active.as_ref().context("empty session tree")?;
        tree.prepare(&active.file, id)
    }
}

#[cfg(test)]
pub(crate) fn indexed_listing(dir: &Path) -> Result<SessionListing> {
    // 每次验收显式使用独立临时库，不能从会话目录推导共享 /tmp/history.db。
    let database = tempfile::tempdir()?;
    let mut store = crate::history::Store::open(&database.path().join("history.db"))?;
    let diagnostics = store.sync_sessions(dir, &pp_core::cancellation::CancellationToken::new())?;
    Ok(SessionListing {
        sessions: store.search_sessions(None, "")?,
        diagnostics,
    })
}

/// 测试专用：统计库随会话目录落在临时工作区，避免写真实 `~/.pp/stats`。
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

#[cfg(test)]
pub(crate) mod tests;

#[cfg(test)]
mod slimming_tests;

/// 冻结的展示格式 fixture 通过与生产读取相同的投影器进入 UI 测试。
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
