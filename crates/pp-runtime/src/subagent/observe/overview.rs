//! Hub 总览／Activity 专用轻量事件读取：流式跳过与合并高频 delta，避免把大量逐 token 碎片送往 UI。

use super::{Binding, EventCursor, ObservedEntry, ObservedRecord, lock};
use anyhow::{Context, Result, ensure};
use pp_core::events::AgentEvent;
use pp_core::model::{ModelSource, ThinkLevel};
use serde::Deserialize;
use serde::de::IgnoredAny;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::os::unix::fs::{FileExt, MetadataExt};

/// 单页总览软读取上限（物理字节）。
const OVERVIEW_PAGE_BYTES: u64 = 4 * 1024 * 1024;
/// 单页保留的关键条目上限；超出后交由下一页继续扫描。
const OVERVIEW_PAGE_ENTRIES: usize = 2048;
/// 预读探测长度：用于判断末尾未消费片段是否包含完整行。
const PROBE_BYTES: u64 = 4096;
/// 与 `projection::SUMMARY_SCALARS` 对齐的摘要标量上限。
const SUMMARY_SCALARS: usize = 512;

#[derive(Debug, Default)]
pub struct OverviewPage {
    pub entries: Vec<ObservedEntry>,
    pub last_activity_ms: Option<u64>,
    pub cursor: Option<EventCursor>,
    pub reset: bool,
    pub has_more: bool,
    pub warnings: Vec<String>,
}

#[derive(Deserialize)]
struct OverviewLine {
    #[serde(default)]
    root_id: Option<String>,
    agent_id: String,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    generation: u64,
    #[serde(default)]
    sequence: Option<u64>,
    #[serde(default)]
    timestamp_ms: Option<u64>,
    #[serde(default)]
    event: Option<OverviewEvent>,
    #[serde(default)]
    hub: Option<super::HubEvent>,
}

/// 仅解码 Hub 总览／Activity 关心的事件字段，其余高频／大体积变体零分配跳过。
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum OverviewEvent {
    TodoStateUpdated(IgnoredAny),
    TodoReminder(IgnoredAny),
    TodoSnapshot(IgnoredAny),
    PlanProgressUpdated(IgnoredAny),
    ContextUpdated {
        usage: pp_core::context::ContextUsage,
    },
    CompactionStarted(IgnoredAny),
    CompactionFinished(IgnoredAny),
    UserInputAccepted {
        id: u64,
        text: String,
    },
    UserInputRejected(IgnoredAny),
    HookDiagnostic(IgnoredAny),
    ModelRequestStarted,
    RequestRetrying {
        reason: pp_core::events::RequestRetryReason,
        attempt: u32,
        wait_ms: u64,
    },
    ModelOutputProgress(IgnoredAny),
    ModelRequestFinished {
        elapsed_ms: u64,
        usage: Option<pp_core::context::TokenUsage>,
        success: bool,
    },
    ModelAttemptFailed {
        attempt: u32,
        #[serde(default)]
        model: String,
        #[serde(default)]
        error: String,
    },
    ModelRouteChanged {
        model: String,
        effort: ThinkLevel,
        #[serde(default)]
        source: ModelSource,
        temporary: bool,
    },
    AssistantStarted,
    ThinkingDelta {
        text: String,
    },
    ReasoningSummaryDelta {
        text: String,
    },
    AssistantDelta {
        text: String,
        #[allow(dead_code)]
        refusal: bool,
    },
    AssistantFinished {
        text: String,
    },
    ToolArgumentsDelta(IgnoredAny),
    ToolStarted {
        id: String,
        name: String,
        arguments: String,
    },
    ProjectInstructionsLoaded(IgnoredAny),
    FileChanged(IgnoredAny),
    AutoReviewStarted(IgnoredAny),
    AutoReviewFinished(IgnoredAny),
    ToolOutput(IgnoredAny),
    ToolOutputSnapshot(IgnoredAny),
    SubagentResultsInjected(IgnoredAny),
    ToolShellSnapshot(IgnoredAny),
    ToolPipelineSnapshot(IgnoredAny),
    ToolFinished {
        id: String,
        result: serde_json::Value,
    },
    LspDiagnostics {
        #[serde(default)]
        id: Option<String>,
        report: pp_core::lsp_report::DiagnosticReport,
    },
}

struct PendingMeta {
    root_id: Option<String>,
    agent_id: String,
    run_id: Option<String>,
    generation: u64,
    sequence: Option<u64>,
    timestamp_ms: Option<u64>,
    start_offset: u64,
}

impl PendingMeta {
    fn from_line(line: &OverviewLine, start_offset: u64) -> Self {
        Self {
            root_id: line.root_id.clone(),
            agent_id: line.agent_id.clone(),
            run_id: line.run_id.clone(),
            generation: line.generation,
            sequence: line.sequence,
            timestamp_ms: line.timestamp_ms,
            start_offset,
        }
    }

    fn into_entry(self, event: AgentEvent) -> ObservedEntry {
        ObservedEntry {
            start_offset: self.start_offset,
            record: ObservedRecord {
                root_id: self.root_id,
                agent_id: self.agent_id,
                run_id: self.run_id,
                generation: self.generation,
                sequence: self.sequence,
                timestamp_ms: self.timestamp_ms,
                event: Some(event),
                hub: None,
            },
        }
    }
}

#[derive(Default)]
struct RunCoalesce {
    /// 同一轮连续 `ReasoningSummaryDelta` 合并前缀（用于填充 `work.draft`，保留首条锚点）。
    reasoning_prefix: Option<(PendingMeta, String)>,
    /// 同一轮最后一条 `ReasoningSummaryDelta`（独立保留以匹配逐条覆盖的摘要语义）。
    reasoning_last: Option<(PendingMeta, String)>,
    /// 同一轮最后一条非空或尚未被覆盖的 `AssistantDelta` / `ThinkingDelta`。
    delta: Option<(PendingMeta, String, bool)>,
}

impl RunCoalesce {
    fn push_reasoning(&mut self, meta: PendingMeta, text: String) {
        if let Some((prev_meta, prev_text)) = self.reasoning_last.take() {
            match &mut self.reasoning_prefix {
                Some((_, prefix)) => {
                    let room = SUMMARY_SCALARS.saturating_sub(prefix.chars().count());
                    prefix.extend(prev_text.chars().take(room));
                }
                None => {
                    let prefix = take_scalars(&prev_text, SUMMARY_SCALARS);
                    self.reasoning_prefix = Some((prev_meta, prefix));
                }
            }
        }
        self.reasoning_last = Some((meta, text));
        self.delta = None;
    }

    fn push_delta(&mut self, meta: PendingMeta, text: String, thinking: bool) {
        if text.trim().is_empty() && self.delta.is_some() {
            return;
        }
        self.delta = Some((meta, text, thinking));
    }

    fn flush_reasoning(&mut self, out: &mut Vec<ObservedEntry>) {
        if let Some((meta, text)) = self.reasoning_prefix.take() {
            out.push(meta.into_entry(AgentEvent::ReasoningSummaryDelta { text }));
        }
        if let Some((meta, text)) = self.reasoning_last.take() {
            out.push(meta.into_entry(AgentEvent::ReasoningSummaryDelta { text }));
        }
    }

    fn flush_all(&mut self, out: &mut Vec<ObservedEntry>) {
        self.flush_reasoning(out);
        if let Some((meta, text, thinking)) = self.delta.take() {
            let event = if thinking {
                AgentEvent::ThinkingDelta { text }
            } else {
                AgentEvent::AssistantDelta {
                    text,
                    refusal: false,
                }
            };
            out.push(meta.into_entry(event));
        }
    }
}

fn take_scalars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn flush_run(
    runs: &mut BTreeMap<String, RunCoalesce>,
    run_key: &str,
    out: &mut Vec<ObservedEntry>,
) {
    if let Some(state) = runs.get_mut(run_key) {
        state.flush_all(out);
    }
}

impl Binding {
    /// 读取 Hub 总览／Activity 所需的增量关键记录，跳过高频流式输出并合并连续 delta。
    pub fn read_overview_events(
        &self,
        agent_id: &str,
        cursor: Option<&EventCursor>,
    ) -> Result<OverviewPage> {
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
                return Ok(OverviewPage::default());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("opening {}", path.display()));
            }
        };
        let metadata = file
            .metadata()
            .with_context(|| format!("inspecting {}", path.display()))?;
        let (device, inode, length) = (metadata.dev(), metadata.ino(), metadata.len());
        let mut reset = false;
        let mut start = 0u64;
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
        let mut entries = Vec::new();
        let mut warnings = Vec::new();
        let mut last_activity_ms: Option<u64> = None;
        let mut runs: BTreeMap<String, RunCoalesce> = BTreeMap::new();
        let mut consumed = 0u64;
        let mut capped = false;
        if start < length {
            file.seek(SeekFrom::Start(start))?;
            let mut reader = BufReader::with_capacity(64 * 1024, &file);
            let mut line_buf = Vec::with_capacity(4096);
            loop {
                if consumed >= OVERVIEW_PAGE_BYTES || entries.len() >= OVERVIEW_PAGE_ENTRIES {
                    capped = true;
                    break;
                }
                line_buf.clear();
                let read = reader.read_until(b'\n', &mut line_buf)?;
                if read == 0 || !line_buf.ends_with(b"\n") {
                    break;
                }
                let position = start + consumed;
                consumed += read as u64;
                let parsed = serde_json::from_slice::<OverviewLine>(&line_buf[..read - 1]);
                let line = match parsed {
                    Ok(line) if line.event.is_some() || line.hub.is_some() => line,
                    Ok(_) => {
                        warnings.push(format!("record without payload at byte {position}"));
                        continue;
                    }
                    Err(_) => {
                        warnings.push(format!("unreadable record at byte {position}"));
                        continue;
                    }
                };
                if let Some(ts) = line.timestamp_ms {
                    last_activity_ms = Some(last_activity_ms.map_or(ts, |old| old.max(ts)));
                }
                let run_key = line.run_id.as_deref().unwrap_or("-").to_owned();
                let mut line = line;
                match line.event.take() {
                    None => {
                        entries.push(ObservedEntry {
                            start_offset: position,
                            record: ObservedRecord {
                                root_id: line.root_id,
                                agent_id: line.agent_id,
                                run_id: line.run_id,
                                generation: line.generation,
                                sequence: line.sequence,
                                timestamp_ms: line.timestamp_ms,
                                event: None,
                                hub: line.hub,
                            },
                        });
                    }
                    Some(OverviewEvent::ReasoningSummaryDelta { text }) => {
                        let meta = PendingMeta::from_line(&line, position);
                        runs.entry(run_key).or_default().push_reasoning(meta, text);
                    }
                    Some(OverviewEvent::AssistantDelta { text, .. }) => {
                        let meta = PendingMeta::from_line(&line, position);
                        runs.entry(run_key)
                            .or_default()
                            .push_delta(meta, text, false);
                    }
                    Some(OverviewEvent::ThinkingDelta { text }) => {
                        let meta = PendingMeta::from_line(&line, position);
                        runs.entry(run_key)
                            .or_default()
                            .push_delta(meta, text, true);
                    }
                    Some(OverviewEvent::AssistantStarted) => {
                        flush_run(&mut runs, &run_key, &mut entries);
                        entries.push(
                            PendingMeta::from_line(&line, position)
                                .into_entry(AgentEvent::AssistantStarted),
                        );
                    }
                    Some(OverviewEvent::AssistantFinished { text }) => {
                        if let Some(state) = runs.get_mut(&run_key) {
                            state.flush_reasoning(&mut entries);
                            if text.trim().is_empty() {
                                state.flush_all(&mut entries);
                            } else {
                                // 非空 AssistantFinished 会直接填充/覆盖 Response 行。
                                state.delta = None;
                            }
                        }
                        entries.push(
                            PendingMeta::from_line(&line, position)
                                .into_entry(AgentEvent::AssistantFinished { text }),
                        );
                    }
                    Some(OverviewEvent::ToolStarted {
                        id,
                        name,
                        arguments,
                    }) => {
                        flush_run(&mut runs, &run_key, &mut entries);
                        entries.push(PendingMeta::from_line(&line, position).into_entry(
                            AgentEvent::ToolStarted {
                                id,
                                name,
                                arguments,
                            },
                        ));
                    }
                    Some(OverviewEvent::ModelAttemptFailed {
                        attempt,
                        model,
                        error,
                    }) => {
                        flush_run(&mut runs, &run_key, &mut entries);
                        entries.push(PendingMeta::from_line(&line, position).into_entry(
                            AgentEvent::ModelAttemptFailed {
                                attempt,
                                model,
                                error,
                            },
                        ));
                    }
                    Some(
                        OverviewEvent::ToolOutput(_)
                        | OverviewEvent::ModelRequestStarted
                        | OverviewEvent::ModelOutputProgress(_)
                        | OverviewEvent::ToolArgumentsDelta(_)
                        | OverviewEvent::ToolOutputSnapshot(_)
                        | OverviewEvent::ToolPipelineSnapshot(_)
                        | OverviewEvent::ToolShellSnapshot(_)
                        | OverviewEvent::TodoStateUpdated(_)
                        | OverviewEvent::TodoReminder(_)
                        | OverviewEvent::TodoSnapshot(_)
                        | OverviewEvent::PlanProgressUpdated(_)
                        | OverviewEvent::CompactionStarted(_)
                        | OverviewEvent::CompactionFinished(_)
                        | OverviewEvent::SubagentResultsInjected(_)
                        | OverviewEvent::ProjectInstructionsLoaded(_)
                        | OverviewEvent::HookDiagnostic(_)
                        | OverviewEvent::UserInputRejected(_)
                        | OverviewEvent::FileChanged(_)
                        | OverviewEvent::AutoReviewStarted(_)
                        | OverviewEvent::AutoReviewFinished(_),
                    ) => {}
                    Some(other) => {
                        let event = match other {
                            OverviewEvent::ContextUpdated { usage } => {
                                AgentEvent::ContextUpdated { usage }
                            }
                            OverviewEvent::UserInputAccepted { id, text } => {
                                AgentEvent::UserInputAccepted { id, text }
                            }
                            OverviewEvent::RequestRetrying {
                                reason,
                                attempt,
                                wait_ms,
                            } => AgentEvent::RequestRetrying {
                                reason,
                                attempt,
                                wait_ms,
                            },
                            OverviewEvent::ModelRequestFinished {
                                elapsed_ms,
                                usage,
                                success,
                            } => AgentEvent::ModelRequestFinished {
                                elapsed_ms,
                                usage,
                                success,
                            },
                            OverviewEvent::ModelRouteChanged {
                                model,
                                effort,
                                source,
                                temporary,
                            } => AgentEvent::ModelRouteChanged {
                                model,
                                effort,
                                source,
                                temporary,
                            },
                            OverviewEvent::ToolFinished { id, result } => {
                                AgentEvent::ToolFinished { id, result }
                            }
                            OverviewEvent::LspDiagnostics { id, report } => {
                                AgentEvent::LspDiagnostics { id, report }
                            }
                            _ => unreachable!("handled above"),
                        };
                        entries.push(PendingMeta::from_line(&line, position).into_entry(event));
                    }
                }
            }
        }
        for (_, mut state) in runs {
            state.flush_all(&mut entries);
        }
        entries.sort_by_key(|entry| entry.start_offset);
        let offset = start + consumed;
        let mut has_more = capped && offset < length;
        if !has_more && offset < length {
            let mut probe = vec![0u8; PROBE_BYTES.min(length - offset) as usize];
            if let Ok(read) = file.read_at(&mut probe, offset) {
                has_more = probe[..read].contains(&b'\n');
            }
        }
        Ok(OverviewPage {
            entries,
            last_activity_ms,
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
