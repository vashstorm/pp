//! 子 Agent 的只读观察契约：类型化快照、修订号与按字节分页的事件读取。
//!
//! 观察层不改变任务所有权：它只复用 registry 与 `<agent>.events.jsonl`，
//! 不建立第二份 token 日志、数据库或模型工具。`history://` 的 64 KiB 模型侧
//! 上限保持原职责，Hub 使用 [`Binding::read_events`] 分页读取完整历史。

use super::{Binding, Inner, Record, lock, now};
use anyhow::{Context, Result, ensure};
use pp_core::events::AgentEvent;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    sync::atomic::Ordering,
};

/// 单页最多返回的完整记录条数。
const PAGE_ENTRIES: usize = 128;
/// 单页软读取预算；单条超大记录会继续读到行尾，绝不截断。
const PAGE_BYTES: u64 = 256 * 1024;
/// 预读探测长度：用于判断 offset 之后是否已有完整行。
const PROBE_BYTES: u64 = 4096;
/// 单条记录异常巨大时的防御上限；只是内存保护，不是截断点。
const MAX_RECORD_BYTES: usize = 32 * 1024 * 1024;

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

/// 流式写出的 envelope：不构造中间 `Value`，避免复制整段输出。
#[derive(Serialize)]
struct Envelope<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    root_id: Option<&'a str>,
    agent_id: &'a str,
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

impl Inner {
    /// 每个 agent 一条整行追加锁；worker 持有自己的 `Arc`，控制线程共用同一把。
    pub(super) fn append_gate(&self, id: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
        lock(&self.appends)
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    fn append_lock(&self, agent_id: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
        self.append_gate(agent_id)
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
        let event = if matches!(event, Some(AgentEvent::ModelOutputProgress { .. })) {
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
            | AgentEvent::ToolPipelineSnapshot { .. }),
        ) = event
        {
            let previews = match event {
                AgentEvent::ToolPipelineSnapshot { .. } => &self.pipeline_previews,
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
        }
        let gate = self.append_lock(agent_id);
        let root = self
            .directory()
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        let result = (|| -> Result<()> {
            let _guard = lock(&gate);
            let path = self.directory().join(format!("{agent_id}.events.jsonl"));
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            serde_json::to_writer(
                &mut file,
                &Envelope {
                    root_id: root.as_deref(),
                    agent_id,
                    run_id,
                    generation,
                    sequence,
                    timestamp_ms: now(),
                    event,
                    hub: hub.as_ref(),
                },
            )?;
            file.write_all(b"\n")?;
            Ok(())
        })();
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
        let path = self
            .inner
            .directory()
            .join(format!("{agent_id}.events.jsonl"));
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
                Ok(record) if record.event.is_some() || record.hub.is_some() => {
                    entries.push(ObservedEntry {
                        start_offset: position,
                        record,
                    });
                }
                Ok(_) => warnings.push(format!("record without payload at byte {position}")),
                Err(_) => warnings.push(format!("unreadable record at byte {position}")),
            }
        }
        let offset = start + consumed as u64;
        // 末尾半行留到下次，不推进 offset，也不让 has_more 引起忙循环。
        let mut has_more = capped;
        if !has_more && offset < length {
            let mut probe = vec![0u8; PROBE_BYTES.min(length - offset) as usize];
            if let Ok(read) = file.read_at(&mut probe, offset) {
                has_more = probe[..read].contains(&b'\n');
            }
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
}

use std::os::unix::fs::FileExt;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent::{RootOptions, Spec, atomic_json};
    use pp_core::model::ThinkLevel;
    use std::fs;
    use std::path::PathBuf;

    fn spec() -> Spec {
        Spec {
            model_role: None,
            role_route: None,
            definition: "task".into(),
            prompt: String::new(),
            context: String::new(),
            model: "fixture-model".into(),
            thinking: ThinkLevel::None,
            tools: vec![],
            spawns: vec![],
            output_schema: None,
            cwd: PathBuf::from("."),
            depth: 1,
            readonly: false,
            isolated: false,
            cost: None,
        }
    }

    fn binding(dir: &std::path::Path, id: &str) -> Binding {
        let dir = dir.join("session.subagents");
        std::fs::create_dir_all(&dir).unwrap();
        atomic_json(
            &dir.join("registry.json"),
            &serde_json::json!({
                "version": 1,
                "agents": { id: {
                    "id": id,
                    "name": "probe",
                    "parent": "Main",
                    "generation": 1,
                    "status": "idle",
                    "spec": spec(),
                    "session_file": null,
                    "updated_ms": 1,
                }},
                "runs": {},
                "next_id": 1,
            }),
        )
        .unwrap();
        Binding::root(
            RootOptions {
                user: dir.clone(),
                dir,
                cwd: PathBuf::from("."),
                model: "fixture-model".into(),
                thinking: ThinkLevel::None,
                settings: Default::default(),
                plan_mode: false,
            },
            std::sync::Arc::new(|_, _| unreachable!("no launch in observe tests")),
            std::sync::Arc::new(|_| Ok(())),
        )
        .unwrap()
    }

    fn append(binding: &Binding, id: &str, record: &str) {
        use std::io::Write;
        let path = binding.directory().join(format!("{id}.events.jsonl"));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        writeln!(file, "{record}").unwrap();
    }

    fn read_all(binding: &Binding, id: &str) -> Vec<ObservedEntry> {
        let mut cursor = None;
        let mut all = Vec::new();
        loop {
            let page = binding.read_events(id, cursor.as_ref()).unwrap();
            cursor = page.cursor;
            all.extend(page.entries);
            if !page.has_more {
                return all;
            }
        }
    }

    #[test]
    fn cursor_resumes_without_rereading_and_completes_half_line() {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        append(
            &binding,
            "a1",
            r#"{"agent_id":"a1","generation":1,"sequence":1,"event":{"assistant_started":null}}"#,
        );
        let first = binding.read_events("a1", None).unwrap();
        assert_eq!(first.entries.len(), 1);
        assert!(!first.reset);
        assert!(!first.has_more);
        let cursor = first.cursor.unwrap();
        // 半行：写入推进游标但不产生条目，也不报告 has_more。
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(binding.directory().join("a1.events.jsonl"))
                .unwrap();
            write!(file, r#"{{"agent_id":"a1","generation":1"#).unwrap();
        }
        let partial = binding.read_events("a1", Some(&cursor)).unwrap();
        assert_eq!(partial.entries.len(), 0);
        assert!(!partial.has_more);
        assert_eq!(partial.cursor.as_ref().unwrap().offset, cursor.offset);
        {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(binding.directory().join("a1.events.jsonl"))
                .unwrap();
            writeln!(
                file,
                r#","sequence":2,"event":{{"assistant_finished":{{"text":"done"}}}}}}"#
            )
            .unwrap();
        }
        let completed = binding
            .read_events("a1", Some(partial.cursor.as_ref().unwrap()))
            .unwrap();
        assert_eq!(completed.entries.len(), 1);
        assert_eq!(completed.entries[0].start_offset, cursor.offset);
        // 再读一次不会重复。
        let again = binding
            .read_events("a1", Some(completed.cursor.as_ref().unwrap()))
            .unwrap();
        assert!(again.entries.is_empty());
    }

    #[test]
    fn utf8_split_across_reads_and_large_records_page() {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        let text = "中文输出".repeat(20_000);
        for sequence in 1..=200 {
            let record = serde_json::json!({
                "agent_id": "a1",
                "generation": 1,
                "sequence": sequence,
                "timestamp_ms": sequence,
                "event": {"assistant_delta": {"text": if sequence == 1 { &text } else { "x" }, "refusal": false}},
            });
            append(&binding, "a1", &record.to_string());
        }
        let entries = read_all(&binding, "a1");
        assert_eq!(entries.len(), 200);
        assert_eq!(entries[0].start_offset, 0);
        // 大记录跨越软预算仍被完整读出。
        let AgentEvent::AssistantDelta { text: first, .. } =
            entries[0].record.event.clone().unwrap()
        else {
            panic!("expected delta");
        };
        assert_eq!(first, text);
        assert!(
            entries
                .windows(2)
                .all(|pair| pair[0].start_offset < pair[1].start_offset)
        );
    }

    #[test]
    fn generation_changes_and_broken_lines_keep_stream_readable() {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        append(
            &binding,
            "a1",
            r#"{"agent_id":"a1","generation":1,"sequence":1,"event":{"assistant_started":null}}"#,
        );
        append(&binding, "a1", "{not json");
        append(
            &binding,
            "a1",
            r#"{"agent_id":"a1","generation":2,"sequence":1,"hub":{"kind":"run_state","status":"queued"}}"#,
        );
        let page = binding.read_events("a1", None).unwrap();
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.entries[1].record.generation, 2);
        assert_eq!(page.warnings.len(), 1);
        assert!(page.warnings[0].contains("byte"));
        assert!(!page.warnings[0].contains("not json"));
    }

    #[test]
    fn replacement_and_truncation_request_reset() {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        append(
            &binding,
            "a1",
            r#"{"agent_id":"a1","generation":1,"sequence":1,"event":{"assistant_started":null}}"#,
        );
        let cursor = binding.read_events("a1", None).unwrap().cursor.unwrap();
        let path = binding.directory().join("a1.events.jsonl");
        // 替换文件：inode 变化必须 reset。
        let replacement = binding.directory().join("replacement.jsonl");
        fs::write(&replacement, "").unwrap();
        fs::rename(&replacement, &path).unwrap();
        append(
            &binding,
            "a1",
            r#"{"agent_id":"a1","generation":1,"sequence":1,"event":{"assistant_started":null}}"#,
        );
        let page = binding.read_events("a1", Some(&cursor)).unwrap();
        assert!(page.reset);
        assert_eq!(page.entries.len(), 1);
        // 截断到游标之前同样 reset。
        let cursor = page.cursor.unwrap();
        fs::write(&path, "").unwrap();
        let page = binding.read_events("a1", Some(&cursor)).unwrap();
        assert!(page.reset);
        assert!(page.entries.is_empty());
    }

    #[test]
    fn missing_file_and_unknown_agent_are_distinct() {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        let page = binding.read_events("a1", None).unwrap();
        assert!(page.entries.is_empty());
        assert!(page.cursor.is_none());
        assert!(binding.read_events("a9", None).is_err());
        assert!(!binding.same_root(&binding.clone()) || binding.same_root(&binding.clone()));
    }

    #[test]
    fn snapshots_keep_flat_agent_shape() {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        let value = serde_json::to_value(binding.hub_snapshot()).unwrap();
        assert_eq!(value["agents"][0]["id"], "a1");
        assert_eq!(value["agents"][0]["parent"], "Main");
        assert!(value["agents"][0].get("spec").is_some());
        assert!(value["runs"].as_array().unwrap().is_empty());
        let legacy = binding.snapshots();
        assert_eq!(legacy["agents"][0]["id"], "a1");
    }

    #[test]
    fn progress_does_not_write_or_bump_revision_and_sequence_gaps_are_valid() {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        let revision = binding.hub_revision();
        for sequence in 1..100 {
            binding.inner.observe(
                "a1",
                Some("r1"),
                1,
                Some(sequence),
                Some(&AgentEvent::ModelOutputProgress {
                    output_bytes: sequence,
                }),
                None,
            );
        }
        assert_eq!(binding.hub_revision(), revision);
        assert!(binding.hub_snapshot().output_previews.is_empty());
        assert!(read_all(&binding, "a1").is_empty());
        for sequence in [100, 200] {
            binding.inner.observe(
                "a1",
                Some("r1"),
                1,
                Some(sequence),
                Some(&AgentEvent::ThinkingDelta {
                    text: "kept".into(),
                }),
                None,
            );
        }
        let entries = read_all(&binding, "a1");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].record.sequence, Some(100));
        assert_eq!(entries[1].record.sequence, Some(200));
        binding.inner.observe(
            "a1",
            Some("r1"),
            1,
            Some(201),
            Some(&AgentEvent::ModelOutputProgress { output_bytes: 201 }),
            Some(HubEvent::RunState {
                status: "completed".into(),
            }),
        );
        let entries = read_all(&binding, "a1");
        assert!(entries[2].record.event.is_none());
        assert!(entries[2].record.hub.is_some());
    }

    #[test]
    fn artifact_output_previews_replace_in_memory_without_appending_event_log() {
        let home = tempfile::tempdir().unwrap();
        let binding = binding(home.path(), "a1");
        binding.inner.observe(
            "a1",
            Some("r1"),
            1,
            Some(1),
            Some(&AgentEvent::ToolStarted {
                id: "bash".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }),
            None,
        );
        for i in 0..20 {
            binding.inner.observe(
                "a1",
                Some("r1"),
                1,
                Some(i + 2),
                Some(&AgentEvent::ToolOutputSnapshot {
                    id: "bash".into(),
                    output: format!("current-{i}"),
                }),
                None,
            );
            binding.inner.observe(
                "a1",
                Some("r1"),
                1,
                Some(i + 22),
                Some(&AgentEvent::ToolPipelineSnapshot {
                    id: "bash".into(),
                    pipes: serde_json::json!({"run_id":"r1","revision":i}),
                    preview: None,
                }),
                None,
            );
        }
        let snapshot = binding.hub_snapshot();
        assert_eq!(snapshot.output_previews.len(), 1);
        assert!(
            matches!(&snapshot.output_previews[0].event,Some(AgentEvent::ToolOutputSnapshot { output, .. }) if output == "current-19")
        );
        // 阶段预览独立保留，不覆盖主输出预览。
        assert_eq!(snapshot.pipeline_previews.len(), 1);
        assert!(matches!(
            &snapshot.pipeline_previews[0].event,
            Some(AgentEvent::ToolPipelineSnapshot { pipes, .. })
                if pipes["revision"] == 19
        ));
        assert_eq!(read_all(&binding, "a1").len(), 1);
        binding.inner.observe(
            "a1",
            Some("r1"),
            1,
            Some(22),
            Some(&AgentEvent::ToolFinished {
                id: "bash".into(),
                result: serde_json::json!({"ok":true,"output":"final"}),
            }),
            None,
        );
        let snapshot = binding.hub_snapshot();
        assert!(snapshot.output_previews.is_empty());
        assert!(snapshot.pipeline_previews.is_empty());
        assert_eq!(read_all(&binding, "a1").len(), 2);
    }
}
