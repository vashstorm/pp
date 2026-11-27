//! 子 Agent 的只读观察契约：类型化快照、修订号与按字节分页的事件读取。
//!
//! 观察层不改变任务所有权：它只复用 registry 与 `<agent>.events.jsonl`，
//! 不建立第二份 token 日志、数据库或模型工具。`history://` 的 64 KiB 模型侧
//! 上限保持原职责，Hub 使用 [`Binding::read_events`] 分页读取完整历史。

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
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

/// 单页最多返回的完整记录条数。
const PAGE_ENTRIES: usize = 128;
/// 单页软读取预算；单条超大记录会继续读到行尾，绝不截断。
const PAGE_BYTES: u64 = 256 * 1024;
/// 单条记录异常巨大时的防御上限；只是内存保护，不是截断点。
const MAX_RECORD_BYTES: usize = 32 * 1024 * 1024;
/// 判断“offset 之后是否还有完整行”时每次读取的窗口。
const LINE_SCAN_CHUNK: usize = 64 * 1024;
/// 单个待合并缓冲的文本上限：达到后立即落盘，避免缓冲无界增长。
const COALESCE_BYTES: usize = 8 * 1024;
/// 缓冲时限：观察轮询在读取前冲刷过期缓冲，流式尾部不等待下一轮模型事件。
const COALESCE_DELAY: Duration = Duration::from_millis(250);

/// 快照中的单个 Agent；record 用 flatten 保持既有扁平的 JSON 形状。
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
    /// 每个子 Agent 最近的管道阶段预览；与主输出预览分开有界保留。
    #[serde(skip)]
    pub pipeline_previews: Vec<ObservedRecord>,
    #[serde(skip)]
    pub shell_previews: Vec<ObservedRecord>,
    pub agents: Vec<HubAgent>,
    pub runs: Vec<super::Run>,
    /// 观察日志健康提示；不进入旧 snapshots wire。
    #[serde(skip)]
    pub warnings: Vec<String>,
}

/// 进程内事件游标；文件身份与行起始位置共同决定可续读位置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventCursor {
    pub device: u64,
    pub inode: u64,
    pub offset: u64,
}

/// Hub 私有生命周期记录；不扩展全局 [`AgentEvent`] 枚举。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HubEvent {
    RunState { status: String },
    AgentState { status: String },
}

/// 一行 `.events.jsonl`。旧记录缺 timestamp/hub/可选元数据仍可读取。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObservedRecord {
    #[serde(default)]
    pub root_id: Option<String>,
    /// 瘦身新行缺省为空串；读取端按请求 agent id 补齐。
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
    /// 累计输出字节和首个输出的单调时间。
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

/// 可合并的流式 delta 种类；`AssistantDelta` 的 refusal 参与边界匹配。
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

/// 待合并的连续 delta 缓冲：保留首片锚点，只追加文本。
struct PendingDelta {
    run_id: Option<String>,
    generation: u64,
    kind: DeltaKind,
    sequence: Option<u64>,
    timestamp_ms: u64,
    text: String,
    started: Instant,
}

/// 每个 agent 的追加状态：整行写锁加可选待合并缓冲。
#[derive(Default)]
pub(super) struct AppendState {
    pending: Option<PendingDelta>,
}

/// 判定可合并的 delta，返回种类与文本借用；其余事件返回 `None`。
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

/// 流式写出的 envelope：不构造中间 `Value`，避免复制整段输出。
/// 新行不再序列化 `root_id`/`agent_id`；读取端按请求 agent 与当前目录名补齐。
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

/// 一条观察信封的可选元数据；与 payload 分开传递，避免超长参数表。
struct RecordMeta<'a> {
    run_id: Option<&'a str>,
    generation: u64,
    sequence: Option<u64>,
    timestamp_ms: u64,
}

impl Inner {
    /// 每个 agent 一条整行追加锁 + 待合并缓冲；worker 持有自己的 `Arc`，控制线程共用同一把。
    pub(super) fn append_gate(&self, id: &str) -> std::sync::Arc<std::sync::Mutex<AppendState>> {
        lock(&self.appends)
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    fn append_lock(&self, agent_id: &str) -> std::sync::Arc<std::sync::Mutex<AppendState>> {
        self.append_gate(agent_id)
    }

    /// 把一条观察信封写入 `<agent>.events.jsonl`；调用方必须已持有该 agent 的 gate。
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

    /// 把缓冲中的一条 delta 落盘；调用方已取走该缓冲，失败即丢弃，不再累积。
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

    /// 冲掉并清空该 agent 的待合并缓冲；无缓冲时为空操作。
    fn flush_pending(&self, agent_id: &str, state: &mut AppendState) -> Result<()> {
        match state.pending.take() {
            Some(pending) => self.write_pending(agent_id, pending),
            None => Ok(()),
        }
    }

    /// 记录一次追加结果：成功清除该 agent 的健康提示，失败保留脱敏摘要；两者都推进修订号。
    fn record_append_outcome(&self, agent_id: &str, result: Result<()>) {
        let mut health = lock(&self.observe_errors);
        match result {
            Ok(()) => {
                health.remove(agent_id);
            }
            Err(error) => {
                // 只保留脱敏摘要，避免把路径细节或正文写进 UI。
                health.insert(
                    agent_id.to_owned(),
                    format!("event log is possibly incomplete: {error}"),
                );
            }
        }
        drop(health);
        self.revision.fetch_add(1, Ordering::Relaxed);
    }

    /// 观察轮询读取前冲刷超过时限的缓冲；无动作时不推进修订号。
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

    /// 追加一条观察记录；失败只记录脱敏健康提示，绝不取消真实任务。
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
        // 输出进度只保留内存采样，诊断与拒绝只用于根会话展示。
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
            event @ (AgentEvent::ToolOutputSnapshot { .. }
            | AgentEvent::ToolPipelineSnapshot { .. }
            | AgentEvent::ToolShellSnapshot { .. }),
        ) = event
        {
            let previews = match event {
                AgentEvent::ToolPipelineSnapshot { .. } => &self.pipeline_previews,
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
            lock(&self.pipeline_previews).remove(agent_id);
            lock(&self.shell_previews).remove(agent_id);
        }
        let gate = self.append_lock(agent_id);
        // `None` 表示本步只更新内存缓冲、未发生写盘，不能据此清除已有的写失败提示。
        let result = (|| -> Option<Result<()>> {
            let mut state = lock(&gate);
            // 连续同变体 delta 有界合并：先留在内存，等容量、时限或边界冲刷。
            if hub.is_none()
                && let Some(event) = event
                && let Some((kind, text)) = coalescible(event)
            {
                let timestamp = now();
                // 单个片段已达容量上限：先冲刷旧缓冲，再完整写出，不拆分也不并入缓冲。
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
                    // 时限内继续拼接，不加分隔符；达到容量后立即落盘。
                    if pending.started.elapsed() < COALESCE_DELAY {
                        pending.text.push_str(text);
                        if pending.text.len() < COALESCE_BYTES {
                            return None;
                        }
                        let pending = state.pending.take().expect("merged pending");
                        return Some(self.write_pending(agent_id, pending));
                    }
                }
                // 超时限，或 run/generation/变体/refusal 边界：先冲刷旧缓冲再开新缓冲。
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
            // 其它记录先冲刷已有缓冲，保证工具、回复终态与生命周期顺序；
            // 冲刷失败也照常尝试写当前记录，错误优先保留冲刷一侧。
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

    /// 请求统计更新后让 UI 感知指标变化。
    pub(super) fn bump_revision(&self) {
        self.revision.fetch_add(1, Ordering::Relaxed);
    }
}

impl Binding {
    /// 根实例身份比较；不能比较会从 pending 绑定到真实 session 的目录名。
    pub fn same_root(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// 进程内观察修订号，只用于增量刷新判断。
    pub fn hub_revision(&self) -> u64 {
        self.inner.revision.load(Ordering::Relaxed)
    }

    /// 读取匹配请求的累计输出字节及首个输出时间，只访问进程内存。
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

    /// 类型化快照：state 锁内只克隆记录，释放后再查询模型控制注册表。
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
            pipeline_previews: lock(&self.inner.pipeline_previews)
                .values()
                .cloned()
                .collect(),
            shell_previews: lock(&self.inner.shell_previews).values().cloned().collect(),
            agents,
            runs,
            warnings,
        }
    }

    /// 从登记 id 构造日志路径并分页读取；不接受 UI 提供的任意文件路径。
    pub fn read_events(&self, agent_id: &str, cursor: Option<&EventCursor>) -> Result<EventPage> {
        ensure!(
            lock(&self.inner.state).disk.agents.contains_key(agent_id),
            "unknown agent"
        );
        // 观察轮询推进流式尾部：过期缓冲先落盘，再打开文件与计算游标。
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
            // 单条记录可以超过软预算；继续读到行尾，绝不截断记录。
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
                    // 瘦身新行缺省身份按请求 agent 与目录名补齐；旧行显式值保持原样。
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
        // 单行超过 MAX_RECORD_BYTES 且含换行时跳过该行，避免游标永久卡死。
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
        // 末尾半行留到下次，不推进 offset，也不让 has_more 引起忙循环。
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

    /// 导出（`/dump all`）前强制冲刷全部 agent 的待合并缓冲。
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

/// `offset` 之后是否还有完整行。
///
/// 合并后的单条记录可能远超 4 KiB，固定窗口探测会把“还有记录”误判成文件结束；
/// 这里按窗口逐步扫到下一个换行，扫描总量以 `MAX_RECORD_BYTES` 为上限，
/// 因此末尾半行仍不会被当成完整记录，也不会引起忙循环。
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

impl Binding {
    /// 当前会话是否登记过任何子 Agent（只读内存检查）。
    pub fn has_agents(&self) -> bool {
        !lock(&self.inner.state).disk.agents.is_empty()
    }

    /// 当前正在运行的子代理数；排队和已结束的会话不计入。
    pub fn running_subagent_count(&self) -> usize {
        lock(&self.inner.state)
            .disk
            .agents
            .values()
            .filter(|agent| agent.status == "running")
            .count()
    }

    pub fn summary(&self) -> String {
        let state = lock(&self.inner.state);
        if state.disk.agents.is_empty() {
            return String::new();
        }
        let active = state
            .disk
            .runs
            .values()
            .filter(|r| matches!(r.status.as_str(), "queued" | "running"))
            .count();
        format!(
            "Subagents: {active} running/queued · {} sessions · use /agents",
            state.disk.agents.len()
        )
    }
    /// 行模式 CLI 与既有消费者读取的 JSON 形状：agents 保持扁平的 Record 字段。
    pub fn snapshots(&self) -> Value {
        serde_json::to_value(self.hub_snapshot()).unwrap_or_else(|_| json!({"agents":[],"runs":[]}))
    }
}
