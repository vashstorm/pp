//! Hub 的纯数据投影：只从类型化 registry 与观察记录计算列表、inspector 和
//! Activity，不调用模型、不读取任意 artifact 正文、不改变 result consumed。

use crate::context::ContextUsage;
use crate::events::AgentEvent;
use crate::subagent::{EventPage, HubEvent, HubSnapshot, ObservedEntry, Run};
use std::collections::{BTreeMap, BTreeSet};

/// Activity 摘要保留的 Unicode 标量上限；完整输出只在详情里。
const SUMMARY_SCALARS: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum AgentStatus {
    Running,
    Idle,
    Parked,
    Aborted,
}

impl AgentStatus {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Idle => "idle",
            Self::Parked => "parked",
            Self::Aborted => "aborted",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Response,
    Tool,
    Lifecycle,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RowStatus {
    Pending,
    Success,
    Error,
    Aborted,
}

impl RowStatus {
    pub(super) fn symbol(self) -> &'static str {
        match self {
            Self::Pending => "●",
            Self::Success => "✔",
            Self::Error => "✖",
            Self::Aborted => "■",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Category {
    All,
    Errors,
    Responses,
    Tools,
}

impl Category {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Errors => "Errors",
            Self::Responses => "Responses",
            Self::Tools => "Tools",
        }
    }

    pub(super) fn next(self) -> Self {
        match self {
            Self::All => Self::Errors,
            Self::Errors => Self::Responses,
            Self::Responses => Self::Tools,
            Self::Tools => Self::All,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Scope {
    All,
    Selected,
    Subtree,
}

impl Scope {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::All => "All agents",
            Self::Selected => "Selected agent",
            Self::Subtree => "Subtree",
        }
    }

    pub(super) fn next(self) -> Self {
        match self {
            Self::All => Self::Selected,
            Self::Selected => Self::Subtree,
            Self::Subtree => Self::All,
        }
    }
}

/// 一条 Activity 行；`offset` 是详情定位用的源行起始偏移。
#[derive(Clone, Debug)]
pub(super) struct ActivityRow {
    /// 稳定行键；跨重排、暂停 follow 与详情锚点都用它。
    key: String,
    pub agent: String,
    pub run: Option<String>,
    pub tool: Option<String>,
    pub offset: u64,
    pub timestamp: Option<u64>,
    pub kind: Kind,
    pub status: RowStatus,
    pub title: String,
    pub summary: String,
    /// 旧日志合成行的排序阶段：task=0、事件=1、终态=2。
    phase: u8,
}

impl ActivityRow {
    pub(super) fn id(&self) -> String {
        self.key.clone()
    }
}

#[derive(Clone, Debug)]
pub(super) struct AgentView {
    pub id: String,
    pub name: String,
    pub definition: String,
    pub parent: String,
    pub depth: usize,
    pub model: String,
    pub thinking: String,
    pub model_change: Option<String>,
    pub status: AgentStatus,
    /// 真实 queued/running Run 状态；用于副行说明。
    pub run_status: Option<String>,
    pub task: Option<String>,
    pub current: Option<String>,
    pub requests: usize,
    pub tokens: Option<u64>,
    pub cost: Option<f64>,
    pub cost_measured: usize,
    pub runs: usize,
    pub tools: usize,
    pub time_ms: Option<u64>,
    /// 全历史扫描未完成时指标标 loading。
    pub loading: bool,
    pub context: Option<ContextUsage>,
    pub readonly: bool,
    pub isolated: bool,
    pub artifacts: Vec<String>,
    pub last_error: Option<String>,
    pub last_activity: Option<u64>,
}

/// 树形或扁平列表中的一行；`prefix` 已包含分支符。
#[derive(Clone, Debug)]
pub(super) struct TreeRow {
    pub agent: usize,
    pub prefix: String,
    /// 只为 lineage 保留的祖先行。
    pub muted: bool,
}

#[derive(Default)]
struct Stream {
    /// `agent|run` -> 当前 assistant turn 计数。
    turns: BTreeMap<String, u64>,
    /// 该 run 是否出现过 RunState 标记。
    markers: BTreeSet<String>,
    legacy_task: BTreeSet<String>,
    legacy_terminal: BTreeSet<String>,
    /// run -> 真实 running 状态的时间戳（毫秒）。
    started: BTreeMap<String, u64>,
    usage_in: u64,
    usage_out: u64,
    usage_seen: bool,
    context: Option<ContextUsage>,
    current: Option<String>,
    current_running: bool,
    last_error: Option<String>,
    last_activity: Option<u64>,
}

#[derive(Default)]
pub(super) struct Projection {
    snapshot: Option<HubSnapshot>,
    agents: Vec<AgentView>,
    index: BTreeMap<String, usize>,
    /// (id, generation) -> 冻结排序权重，越大越靠前。
    ranks: BTreeMap<(String, u64), u64>,
    next_rank: u64,
    rows: Vec<ActivityRow>,
    keys: BTreeMap<String, usize>,
    streams: BTreeMap<String, Stream>,
    /// 全历史扫描完成的 agent。
    scanned: BTreeSet<String>,
    sorted: bool,
    needs_finalize: bool,
}

impl Projection {
    pub(super) fn snapshot(&self) -> Option<&HubSnapshot> {
        self.snapshot.as_ref()
    }

    pub(super) fn agents(&self) -> &[AgentView] {
        &self.agents
    }

    pub(super) fn agent(&self, id: &str) -> Option<&AgentView> {
        self.index.get(id).map(|index| &self.agents[*index])
    }

    pub(super) fn rows(&self) -> &[ActivityRow] {
        &self.rows
    }

    /// 首次快照前视为加载中，不能先闪空状态。
    pub(super) fn loading(&self) -> bool {
        self.snapshot.is_none()
    }

    /// 该 agent 的旧投影与详情锚点失效：只清空这一路。
    pub(super) fn reset(&mut self, agent: &str) {
        self.rows.retain(|row| row.agent != agent);
        self.streams.remove(agent);
        self.scanned.remove(agent);
        self.reindex();
        self.sorted = false;
    }

    /// 每秒时钟刷新：重新计算 age 与活动 Run 的 elapsed。
    pub(super) fn refresh(&mut self, now_ms: u64) {
        if self.snapshot.is_some() {
            self.rebuild(now_ms);
        }
    }

    pub(super) fn apply_snapshot(&mut self, snapshot: HubSnapshot, now_ms: u64) {
        self.snapshot = Some(snapshot);
        self.needs_finalize = true;
        self.rebuild(now_ms);
    }

    /// 摄入一页观察记录；`page.has_more == false` 表示该 agent 历史已读完。
    pub(super) fn ingest(&mut self, agent: &str, page: &EventPage, now_ms: u64) {
        for entry in &page.entries {
            self.consume(agent, entry);
        }
        if !page.has_more {
            self.scanned.insert(agent.to_owned());
            self.needs_finalize = true;
        }
        self.rebuild(now_ms);
    }

    fn consume(&mut self, agent: &str, entry: &ObservedEntry) {
        let record = &entry.record;
        let offset = entry.start_offset;
        let timestamp = record.timestamp_ms;
        let run = record.run_id.clone();
        {
            let stream = self.streams.entry(agent.to_owned()).or_default();
            if let Some(timestamp) = timestamp {
                stream.last_activity = Some(
                    stream
                        .last_activity
                        .map_or(timestamp, |old| old.max(timestamp)),
                );
            }
        }
        match (&record.event, &record.hub) {
            (Some(event), _) => {
                self.consume_event(agent, run.as_deref(), offset, timestamp, event);
            }
            (None, Some(HubEvent::RunState { status })) => {
                let stream = self.streams.entry(agent.to_owned()).or_default();
                if let Some(run) = &run {
                    stream.markers.insert(run.clone());
                    if status == "running"
                        && let Some(timestamp) = timestamp
                    {
                        stream.started.insert(run.clone(), timestamp);
                    }
                }
                let row_status = match status.as_str() {
                    "queued" | "running" => RowStatus::Pending,
                    "completed" => RowStatus::Success,
                    "cancelled" => RowStatus::Aborted,
                    _ => RowStatus::Error,
                };
                if row_status == RowStatus::Error {
                    let message = format!("run {status}");
                    self.streams.entry(agent.to_owned()).or_default().last_error =
                        Some(message.clone());
                }
                self.insert_lifecycle(
                    agent,
                    run.as_deref(),
                    offset,
                    timestamp,
                    "Run",
                    status.clone(),
                    row_status,
                    1,
                );
            }
            (None, Some(HubEvent::AgentState { status })) => {
                let row_status = match status.as_str() {
                    "running" => RowStatus::Pending,
                    "aborted" => RowStatus::Aborted,
                    _ => RowStatus::Success,
                };
                self.insert_lifecycle(
                    agent,
                    None,
                    offset,
                    timestamp,
                    "Agent",
                    status.clone(),
                    row_status,
                    1,
                );
            }
            _ => {}
        }
    }

    fn consume_event(
        &mut self,
        agent: &str,
        run: Option<&str>,
        offset: u64,
        timestamp: Option<u64>,
        event: &AgentEvent,
    ) {
        let run_key = format!("{agent}|{}", run.unwrap_or("-"));
        match event {
            AgentEvent::AssistantStarted => {
                let turn = {
                    let stream = self.streams.entry(agent.to_owned()).or_default();
                    let turn = stream.turns.entry(run_key.clone()).or_insert(0);
                    *turn += 1;
                    *turn
                };
                self.set_current(agent, "Requesting model");
                self.response_row(
                    agent,
                    run,
                    turn,
                    offset,
                    timestamp,
                    String::new(),
                    RowStatus::Pending,
                );
            }
            AgentEvent::AssistantDelta { text, .. } | AgentEvent::ThinkingDelta { text } => {
                let turn = {
                    let stream = self.streams.entry(agent.to_owned()).or_default();
                    let turn = stream.turns.entry(run_key.clone()).or_insert(1);
                    *turn
                };
                self.response_row(
                    agent,
                    run,
                    turn,
                    offset,
                    timestamp,
                    truncate(text),
                    RowStatus::Pending,
                );
                let phase = if matches!(event, AgentEvent::ThinkingDelta { .. }) {
                    "Thinking"
                } else {
                    "Responding"
                };
                self.set_current(agent, phase);
            }
            AgentEvent::AssistantFinished { text } => {
                let turn = {
                    let stream = self.streams.entry(agent.to_owned()).or_default();
                    let turn = stream.turns.entry(run_key).or_insert(1);
                    if *turn == 0 {
                        *turn = 1;
                    }
                    *turn
                };
                self.set_current(agent, "Processing response");
                // 纯工具调用的 turn 没有正文，不保留空 response 行。
                let key = format!("{agent}|{}|response|{turn}", run.unwrap_or("-"));
                if text.trim().is_empty()
                    && self
                        .keys
                        .get(&key)
                        .is_some_and(|index| self.rows[*index].summary.trim().is_empty())
                {
                    self.drop_row(&key);
                    return;
                }
                self.response_row(
                    agent,
                    run,
                    turn,
                    offset,
                    timestamp,
                    truncate(text),
                    RowStatus::Success,
                );
            }
            AgentEvent::ToolStarted {
                id,
                name,
                arguments,
            } => {
                {
                    let stream = self.streams.entry(agent.to_owned()).or_default();
                    stream.current =
                        Some(format!("Running {name} {}", summarize_arguments(arguments)));
                    stream.current_running = true;
                }
                self.upsert(
                    ActivityRow {
                        key: String::new(),
                        agent: agent.to_owned(),
                        run: run.map(str::to_owned),
                        tool: Some(id.clone()),
                        offset,
                        timestamp,
                        kind: Kind::Tool,
                        status: RowStatus::Pending,
                        title: name.clone(),
                        summary: summarize_arguments(arguments),
                        phase: 1,
                    },
                    tool_key(agent, run, id),
                );
            }
            AgentEvent::ToolFinished { id, result } => {
                self.set_current(agent, "Processing tool result");
                let ok = result["ok"] != serde_json::Value::Bool(false);
                let message = result["error"].as_str().unwrap_or("tool failed").to_owned();
                if !ok {
                    self.streams.entry(agent.to_owned()).or_default().last_error =
                        Some(message.clone());
                }
                let key = tool_key(agent, run, id);
                if self.keys.contains_key(&key) {
                    self.upsert(
                        ActivityRow {
                            key: String::new(),
                            agent: agent.to_owned(),
                            run: run.map(str::to_owned),
                            tool: Some(id.clone()),
                            offset,
                            timestamp,
                            kind: Kind::Tool,
                            status: if ok {
                                RowStatus::Success
                            } else {
                                RowStatus::Error
                            },
                            title: String::new(),
                            summary: if ok {
                                String::new()
                            } else {
                                truncate(&message)
                            },
                            phase: 1,
                        },
                        key,
                    );
                } else {
                    // 旧日志缺 ToolStarted：补一条终态行，不伪造开始位置。
                    self.upsert(
                        ActivityRow {
                            key: String::new(),
                            agent: agent.to_owned(),
                            run: run.map(str::to_owned),
                            tool: Some(id.clone()),
                            offset,
                            timestamp,
                            kind: Kind::Tool,
                            status: if ok {
                                RowStatus::Success
                            } else {
                                RowStatus::Error
                            },
                            title: "tool".into(),
                            summary: if ok {
                                String::new()
                            } else {
                                truncate(&message)
                            },
                            phase: 1,
                        },
                        key,
                    );
                }
            }
            AgentEvent::ToolOutput { .. } => {}
            AgentEvent::UserInputAccepted { text, .. } => self.insert_lifecycle(
                agent,
                run,
                offset,
                timestamp,
                "Input accepted",
                truncate(text),
                RowStatus::Success,
                1,
            ),
            AgentEvent::ModelRequestFinished { success, usage, .. } => {
                if let Some(usage) = usage {
                    let stream = self.streams.entry(agent.to_owned()).or_default();
                    stream.usage_in = stream.usage_in.saturating_add(usage.input_tokens);
                    stream.usage_out = stream.usage_out.saturating_add(usage.output_tokens);
                    stream.usage_seen = true;
                }
                if !success {
                    self.streams.entry(agent.to_owned()).or_default().last_error =
                        Some("model request failed".into());
                    self.insert_lifecycle(
                        agent,
                        run,
                        offset,
                        timestamp,
                        "Request failed",
                        "model request failed".into(),
                        RowStatus::Error,
                        1,
                    );
                }
            }
            AgentEvent::ModelAttemptFailed { attempt } => {
                self.set_current(agent, "Model attempt failed");
                self.insert_lifecycle(
                    agent,
                    run,
                    offset,
                    timestamp,
                    "Failed attempt",
                    format!("Attempt {attempt} failed; partial output is excluded from history"),
                    RowStatus::Error,
                    1,
                );
            }
            AgentEvent::ModelRouteChanged { model, .. } => {
                self.set_current(agent, &format!("Fallback: {model}"));
                self.insert_lifecycle(
                    agent,
                    run,
                    offset,
                    timestamp,
                    "Fallback",
                    model.clone(),
                    RowStatus::Pending,
                    1,
                );
            }
            AgentEvent::RequestRetrying {
                reason,
                attempt,
                wait_ms,
            } => {
                let summary = format!(
                    "retrying {reason}, attempt {attempt}, wait {}",
                    crate::tui::format_wait(*wait_ms)
                );
                self.set_current(agent, &summary);
                self.insert_lifecycle(
                    agent,
                    run,
                    offset,
                    timestamp,
                    "Retry",
                    summary,
                    RowStatus::Pending,
                    1,
                );
            }
            AgentEvent::ContextUpdated { usage } => {
                self.streams.entry(agent.to_owned()).or_default().context = Some(*usage);
            }
            AgentEvent::ModelRequestStarted => self.set_current(agent, "Requesting model"),
            AgentEvent::TodoStateUpdated { .. }
            | AgentEvent::TodoReminder { .. }
            | AgentEvent::TodoSnapshot { .. }
            | AgentEvent::PlanProgressUpdated { .. }
            | AgentEvent::CompactionStarted { .. }
            | AgentEvent::CompactionFinished { .. }
            | AgentEvent::ModelOutputProgress { .. }
            | AgentEvent::ProjectInstructionsLoaded { .. }
            | AgentEvent::FileChanged { .. } => {}
        }
    }

    fn set_current(&mut self, agent: &str, text: &str) {
        let stream = self.streams.entry(agent.to_owned()).or_default();
        stream.current = Some(text.to_owned());
        stream.current_running = true;
    }

    /// 一个 assistant turn 一条 response；delta 原位更新同一短摘要。
    #[allow(clippy::too_many_arguments)]
    fn response_row(
        &mut self,
        agent: &str,
        run: Option<&str>,
        turn: u64,
        offset: u64,
        timestamp: Option<u64>,
        summary: String,
        status: RowStatus,
    ) {
        let key = format!("{agent}|{}|response|{turn}", run.unwrap_or("-"));
        if let Some(index) = self.keys.get(&key).copied() {
            let existing = &mut self.rows[index];
            existing.status = status;
            if !summary.trim().is_empty() {
                existing.summary = summary;
            }
            return;
        }
        self.upsert(
            ActivityRow {
                key: String::new(),
                agent: agent.to_owned(),
                run: run.map(str::to_owned),
                tool: None,
                offset,
                timestamp,
                kind: Kind::Response,
                status,
                title: format!("Response {turn}"),
                summary,
                phase: 1,
            },
            key,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_lifecycle(
        &mut self,
        agent: &str,
        run: Option<&str>,
        offset: u64,
        timestamp: Option<u64>,
        title: &str,
        summary: String,
        status: RowStatus,
        phase: u8,
    ) {
        self.upsert(
            ActivityRow {
                key: String::new(),
                agent: agent.to_owned(),
                run: run.map(str::to_owned),
                tool: None,
                offset,
                timestamp,
                kind: Kind::Lifecycle,
                status,
                title: title.to_owned(),
                summary,
                phase,
            },
            format!("{agent}|{}|lifecycle|{title}|{offset}", run.unwrap_or("-")),
        );
    }

    fn upsert(&mut self, mut row: ActivityRow, key: String) {
        if let Some(index) = self.keys.get(&key).copied() {
            let target = &mut self.rows[index];
            target.status = row.status;
            if !row.summary.is_empty() {
                target.summary = row.summary;
            }
            return;
        }
        row.key = key.clone();
        self.keys.insert(key, self.rows.len());
        self.rows.push(row);
        self.sorted = false;
    }

    fn drop_row(&mut self, key: &str) {
        if let Some(index) = self.keys.remove(key) {
            self.rows.remove(index);
            self.reindex();
            self.sorted = false;
        }
    }

    fn reindex(&mut self) {
        self.keys = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| (row.key.clone(), index))
            .collect();
    }

    /// 合成旧日志缺失的 task 关联与终态 lifecycle；新日志有标记不重复。
    fn finalize(&mut self) {
        if !self.needs_finalize {
            return;
        }
        self.needs_finalize = false;
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let runs = snapshot.runs.clone();
        for run in runs {
            let agent = run.agent_id.clone();
            if !self.scanned.contains(&agent) || !self.streams.contains_key(&agent) {
                continue;
            }
            if self
                .streams
                .get(&agent)
                .is_some_and(|stream| stream.markers.contains(&run.id))
            {
                continue;
            }
            let bounds = self
                .rows
                .iter()
                .filter(|row| row.agent == agent && row.run.as_deref() == Some(run.id.as_str()))
                .fold(None, |bounds: Option<(u64, u64)>, row| {
                    Some(match bounds {
                        None => (row.offset, row.offset),
                        Some((low, high)) => (low.min(row.offset), high.max(row.offset)),
                    })
                });
            let Some((first, last)) = bounds else {
                continue;
            };
            if !self
                .streams
                .get(&agent)
                .is_some_and(|stream| stream.legacy_task.contains(&run.id))
            {
                self.insert_lifecycle(
                    &agent,
                    Some(&run.id),
                    first,
                    None,
                    "Task",
                    truncate(&run.task),
                    RowStatus::Success,
                    0,
                );
                self.streams
                    .entry(agent.clone())
                    .or_default()
                    .legacy_task
                    .insert(run.id.clone());
            }
            let terminal = match run.status.as_str() {
                "completed" => Some((RowStatus::Success, "completed")),
                "failed" => Some((RowStatus::Error, "failed")),
                "interrupted" => Some((RowStatus::Error, "interrupted")),
                "cancelled" => Some((RowStatus::Aborted, "cancelled")),
                _ => None,
            };
            if let Some((status, label)) = terminal
                && !self
                    .streams
                    .get(&agent)
                    .is_some_and(|stream| stream.legacy_terminal.contains(&run.id))
            {
                self.insert_lifecycle(
                    &agent,
                    Some(&run.id),
                    last.saturating_add(1),
                    None,
                    "Run",
                    label.into(),
                    status,
                    2,
                );
                self.streams
                    .entry(agent.clone())
                    .or_default()
                    .legacy_terminal
                    .insert(run.id.clone());
            }
        }
        // 合成行插入后需要重排。
        self.sorted = false;
    }

    fn rebuild(&mut self, now_ms: u64) {
        self.finalize();
        if let Some(snapshot) = self.snapshot.clone() {
            self.freeze_ranks(&snapshot);
            self.rebuild_agents(&snapshot, now_ms);
        }
        if !self.sorted {
            self.sort_rows();
        }
    }

    /// 冻结首次出现的 (id, generation) 排序权重；新批次整体插到前面。
    fn freeze_ranks(&mut self, snapshot: &HubSnapshot) {
        let mut fresh = snapshot
            .agents
            .iter()
            .filter(|agent| {
                !self
                    .ranks
                    .contains_key(&(agent.record.id.clone(), agent.record.generation))
            })
            .collect::<Vec<_>>();
        if fresh.is_empty() {
            return;
        }
        fresh.sort_by(|left, right| {
            (
                status_of(snapshot, &left.record.id),
                std::cmp::Reverse(left.record.updated_ms),
                agent_number(&left.record.id),
            )
                .cmp(&(
                    status_of(snapshot, &right.record.id),
                    std::cmp::Reverse(right.record.updated_ms),
                    agent_number(&right.record.id),
                ))
        });
        let count = fresh.len() as u64;
        for (index, agent) in fresh.iter().enumerate() {
            let weight = self.next_rank + count - index as u64;
            self.ranks
                .insert((agent.record.id.clone(), agent.record.generation), weight);
        }
        self.next_rank += count;
    }

    fn rebuild_agents(&mut self, snapshot: &HubSnapshot, now_ms: u64) {
        let mut views = snapshot
            .agents
            .iter()
            .map(|agent| {
                let record = &agent.record;
                let runs = snapshot
                    .runs
                    .iter()
                    .filter(|run| run.agent_id == record.id)
                    .collect::<Vec<&Run>>();
                let stream = self.streams.get(&record.id);
                let status = status_of(snapshot, &record.id);
                let run_status = runs
                    .iter()
                    .find(|run| matches!(run.status.as_str(), "queued" | "running"))
                    .map(|run| run.status.clone());
                let task = runs
                    .iter()
                    .max_by_key(|run| run_number(&run.id))
                    .map(|run| run.task.clone());
                let requests = runs.iter().map(|run| run.requests).sum();
                let tokens_from_runs = runs.iter().map(|run| run.tokens).sum::<u64>();
                let usage = stream.filter(|stream| stream.usage_seen);
                let tokens = usage
                    .map(|stream| stream.usage_in.saturating_add(stream.usage_out))
                    .or_else(|| (tokens_from_runs > 0).then_some(tokens_from_runs));
                let cost_measured = runs.iter().filter(|run| run.cost.is_some()).count();
                let cost = (cost_measured > 0)
                    .then(|| runs.iter().filter_map(|run| run.cost).sum::<f64>());
                let mut time = Some(
                    runs.iter()
                        .filter(|run| !matches!(run.status.as_str(), "queued" | "running"))
                        .map(|run| run.duration_ms)
                        .sum::<u64>(),
                );
                if let Some(stream) = stream {
                    for run in runs
                        .iter()
                        .filter(|run| matches!(run.status.as_str(), "queued" | "running"))
                    {
                        match stream.started.get(&run.id) {
                            Some(started) => {
                                time = time.map(|total| {
                                    total.saturating_add(now_ms.saturating_sub(*started))
                                });
                            }
                            None => time = None,
                        }
                    }
                }
                let artifacts = runs
                    .iter()
                    .filter(|run| run.result.is_some())
                    .max_by_key(|run| run_number(&run.id))
                    .map(|run| {
                        let mut artifacts = vec![format!("agent://{}", record.id)];
                        if let Some(patches) = run
                            .result
                            .as_ref()
                            .and_then(|result| result["patches"].as_array())
                        {
                            for patch in patches {
                                if let Some(path) = patch.as_str() {
                                    artifacts.push(path.to_owned());
                                }
                            }
                        }
                        artifacts
                    })
                    .unwrap_or_default();
                AgentView {
                    id: record.id.clone(),
                    name: record.name.clone(),
                    definition: record.spec.definition.clone(),
                    parent: record.parent.clone(),
                    depth: record.spec.depth,
                    model: record.spec.model.clone(),
                    thinking: record.spec.thinking.as_str().to_owned(),
                    model_change: agent.model_change.clone(),
                    status,
                    run_status,
                    task,
                    current: stream
                        .filter(|stream| stream.current_running)
                        .and_then(|stream| stream.current.clone()),
                    requests,
                    tokens,
                    cost,
                    cost_measured,
                    runs: runs.len(),
                    tools: 0,
                    time_ms: time,
                    loading: !self.scanned.contains(&record.id),
                    context: stream.and_then(|stream| stream.context),
                    readonly: record.spec.readonly,
                    isolated: record.spec.isolated,
                    artifacts,
                    last_error: stream.and_then(|stream| stream.last_error.clone()),
                    last_activity: stream
                        .and_then(|stream| stream.last_activity)
                        .or(Some(record.updated_ms)),
                }
            })
            .collect::<Vec<_>>();
        for view in &mut views {
            view.tools = self
                .rows
                .iter()
                .filter(|row| row.agent == view.id && row.kind == Kind::Tool)
                .count();
        }
        views.sort_by(|left, right| {
            let weight = |view: &AgentView| {
                self.ranks
                    .get(&(view.id.clone(), generation_of(snapshot, &view.id)))
                    .copied()
                    .unwrap_or(0)
            };
            weight(right).cmp(&weight(left))
        });
        self.index = views
            .iter()
            .enumerate()
            .map(|(index, view)| (view.id.clone(), index))
            .collect();
        self.agents = views;
    }

    fn sort_rows(&mut self) {
        self.rows.sort_by_key(row_sort_key);
        self.reindex();
        self.sorted = true;
    }

    /// Agents 列表：扁平或 parent-first DFS；过滤只匹配 id/name（子序列）。
    pub(super) fn list(&self, tree: bool, filter: &str) -> Vec<TreeRow> {
        let needle = filter.trim().to_lowercase();
        let matches = |agent: &AgentView| {
            needle.is_empty()
                || fuzzy(&needle, &agent.id.to_lowercase())
                || fuzzy(&needle, &agent.name.to_lowercase())
        };
        if !tree {
            return self
                .agents
                .iter()
                .enumerate()
                .filter(|(_, agent)| matches(agent))
                .map(|(index, _)| TreeRow {
                    agent: index,
                    prefix: String::new(),
                    muted: false,
                })
                .collect();
        }
        let children = self.children();
        // 匹配项的祖先作为 lineage 保留。
        let mut keep = BTreeSet::new();
        for (index, agent) in self.agents.iter().enumerate() {
            if matches(agent) {
                keep.insert(index);
                let mut parent = agent.parent.clone();
                while let Some(parent_index) = self.index.get(&parent).copied() {
                    if !keep.insert(parent_index) {
                        break;
                    }
                    parent = self.agents[parent_index].parent.clone();
                }
            }
        }
        let matching = self.agents.iter().map(matches).collect::<Vec<bool>>();
        let mut rows = Vec::new();
        let mut visited = BTreeSet::new();
        for root in self.roots() {
            self.walk(
                root,
                &children,
                &keep,
                &matching,
                &mut visited,
                &mut rows,
                "",
            );
        }
        // 孤立 parent 或环防护：未访问项追加为根，不丢行。
        for index in 0..self.agents.len() {
            if !visited.contains(&index) {
                self.walk(
                    index,
                    &children,
                    &keep,
                    &matching,
                    &mut visited,
                    &mut rows,
                    "",
                );
            }
        }
        rows
    }

    fn children(&self) -> BTreeMap<usize, Vec<usize>> {
        let mut children: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (index, agent) in self.agents.iter().enumerate() {
            if let Some(parent) = self.index.get(&agent.parent) {
                children.entry(*parent).or_default().push(index);
            }
        }
        children
    }

    fn roots(&self) -> Vec<usize> {
        self.agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| !self.index.contains_key(&agent.parent))
            .map(|(index, _)| index)
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        index: usize,
        children: &BTreeMap<usize, Vec<usize>>,
        keep: &BTreeSet<usize>,
        matching: &[bool],
        visited: &mut BTreeSet<usize>,
        rows: &mut Vec<TreeRow>,
        prefix: &str,
    ) {
        if !visited.insert(index) {
            return;
        }
        if keep.contains(&index) {
            rows.push(TreeRow {
                agent: index,
                prefix: prefix.to_owned(),
                muted: !matching[index],
            });
        }
        let continuation = match prefix.strip_suffix("├─ ") {
            Some(head) => format!("{head}│  "),
            None => match prefix.strip_suffix("└─ ") {
                Some(head) => format!("{head}   "),
                None => prefix.to_owned(),
            },
        };
        let kids = children
            .get(&index)
            .into_iter()
            .flatten()
            .filter(|child| keep.contains(child))
            .copied()
            .collect::<Vec<_>>();
        let count = kids.len();
        for (slot, child) in kids.into_iter().enumerate() {
            let branch = format!(
                "{continuation}{}",
                if slot + 1 == count {
                    "└─ "
                } else {
                    "├─ "
                }
            );
            self.walk(child, children, keep, matching, visited, rows, &branch);
        }
    }

    /// 可见 Activity 行：类别 + scope + 搜索；scope 锚点不随活动行选择漂移。
    pub(super) fn activity(
        &self,
        category: Category,
        scope: Scope,
        anchor: Option<&str>,
        query: &str,
    ) -> Vec<usize> {
        let query = query.to_lowercase();
        let subtree = anchor.map(|anchor| self.subtree(anchor));
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| match category {
                Category::All => true,
                Category::Errors => row.status == RowStatus::Error,
                Category::Responses => row.kind == Kind::Response,
                Category::Tools => row.kind == Kind::Tool,
            })
            .filter(|(_, row)| match scope {
                Scope::All => true,
                Scope::Selected => anchor.is_some_and(|anchor| row.agent == anchor),
                Scope::Subtree => subtree
                    .as_ref()
                    .is_some_and(|subtree| subtree.contains(&row.agent)),
            })
            .filter(|(_, row)| {
                if query.is_empty() {
                    return true;
                }
                let name = self
                    .agent(&row.agent)
                    .map(|agent| agent.name.as_str())
                    .unwrap_or("");
                format!("{} {} {} {}", row.agent, name, row.title, row.summary)
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn subtree(&self, anchor: &str) -> BTreeSet<String> {
        let mut members = BTreeSet::new();
        members.insert(anchor.to_owned());
        loop {
            let mut added = false;
            for agent in &self.agents {
                if members.contains(&agent.parent) && members.insert(agent.id.clone()) {
                    added = true;
                }
            }
            if !added {
                return members;
            }
        }
    }

    /// 最新同 agent 活动行（inspector 的 Recent activity 使用）。
    pub(super) fn recent(&self, agent: &str, limit: usize) -> Vec<&ActivityRow> {
        let mut rows = self
            .rows
            .iter()
            .filter(|row| row.agent == agent)
            .collect::<Vec<_>>();
        let start = rows.len().saturating_sub(limit);
        rows.drain(..start);
        rows
    }
}

fn tool_key(agent: &str, run: Option<&str>, id: &str) -> String {
    format!("{agent}|{}|tool|{id}", run.unwrap_or("-"))
}

fn row_sort_key(row: &ActivityRow) -> (bool, u64, u64, String, u64, u8) {
    (
        row.timestamp.is_some(),
        row.timestamp.unwrap_or(0),
        agent_number(&row.agent),
        row.agent.clone(),
        row.offset,
        row.phase,
    )
}

fn generation_of(snapshot: &HubSnapshot, id: &str) -> u64 {
    snapshot
        .agents
        .iter()
        .find(|agent| agent.record.id == id)
        .map_or(0, |agent| agent.record.generation)
}

fn status_of(snapshot: &HubSnapshot, id: &str) -> AgentStatus {
    let record = snapshot
        .agents
        .iter()
        .find(|agent| agent.record.id == id)
        .map(|agent| &agent.record);
    // aborted 的 Agent 状态优先。
    if record.is_some_and(|record| record.status == "aborted") {
        return AgentStatus::Aborted;
    }
    if snapshot
        .runs
        .iter()
        .any(|run| run.agent_id == id && matches!(run.status.as_str(), "queued" | "running"))
    {
        return AgentStatus::Running;
    }
    match record.map(|record| record.status.as_str()) {
        Some("parked") => AgentStatus::Parked,
        Some("aborted") => AgentStatus::Aborted,
        _ => AgentStatus::Idle,
    }
}

fn agent_number(id: &str) -> u64 {
    id.strip_prefix('a')
        .and_then(|rest| rest.parse().ok())
        .unwrap_or(u64::MAX)
}

fn run_number(id: &str) -> u64 {
    id.strip_prefix('r')
        .and_then(|rest| rest.parse().ok())
        .unwrap_or(0)
}

/// 大小写不敏感的子序列匹配（两侧均已小写）。
pub(super) fn fuzzy(needle: &str, haystack: &str) -> bool {
    let mut characters = haystack.chars();
    needle
        .chars()
        .all(|wanted| characters.any(|found| found == wanted))
}

/// 单行摘要：折叠换行/Tab，并限制 Unicode 标量数量。
pub(super) fn truncate(text: &str) -> String {
    let mut out = String::new();
    for (count, character) in text.chars().enumerate() {
        if count >= SUMMARY_SCALARS {
            out.push('…');
            break;
        }
        out.push(match character {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        });
    }
    out.trim_end().to_owned()
}

fn summarize_arguments(arguments: &str) -> String {
    truncate(arguments)
}

pub(in crate::tui) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ThinkLevel;
    use crate::events::{AgentEvent, OutputStream};
    use crate::subagent::{HubAgent, ObservedRecord, Record, Spec};
    use std::path::PathBuf;

    fn spec(depth: usize) -> Spec {
        Spec {
            model_role: None,
            definition: "task".into(),
            prompt: String::new(),
            context: String::new(),
            model: "fixture-model".into(),
            thinking: ThinkLevel::None,
            tools: vec![],
            spawns: vec![],
            output_schema: None,
            cwd: PathBuf::from("."),
            depth,
            readonly: false,
            isolated: false,
            cost: None,
        }
    }

    fn record(id: &str, parent: &str, status: &str) -> Record {
        Record {
            id: id.into(),
            name: format!("{id}-name"),
            parent: parent.into(),
            generation: 1,
            status: status.into(),
            spec: spec(1),
            session_file: None,
            updated_ms: 100,
        }
    }

    fn run(id: &str, agent: &str, status: &str, task: &str) -> Run {
        Run {
            id: id.into(),
            agent_id: agent.into(),
            owner: "Main".into(),
            status: status.into(),
            task: task.into(),
            result: None,
            consumed: false,
            duration_ms: 0,
            requests: 0,
            tokens: 0,
            cost: None,
        }
    }

    fn snapshot(agents: Vec<Record>, runs: Vec<Run>) -> HubSnapshot {
        HubSnapshot {
            agents: agents
                .into_iter()
                .map(|record| HubAgent {
                    record,
                    model_change: None,
                })
                .collect(),
            runs,
            warnings: vec![],
        }
    }

    fn entry(offset: u64, timestamp: Option<u64>, run: &str, event: AgentEvent) -> ObservedEntry {
        ObservedEntry {
            start_offset: offset,
            record: ObservedRecord {
                root_id: None,
                agent_id: "a1".into(),
                run_id: Some(run.into()),
                generation: 1,
                sequence: None,
                timestamp_ms: timestamp,
                event: Some(event),
                hub: None,
            },
        }
    }

    fn page(entries: Vec<ObservedEntry>, has_more: bool) -> EventPage {
        EventPage {
            entries,
            cursor: None,
            reset: false,
            has_more,
            warnings: vec![],
        }
    }

    fn delta(text: &str) -> AgentEvent {
        AgentEvent::AssistantDelta {
            text: text.into(),
            refusal: false,
        }
    }

    #[test]
    fn response_and_tool_update_in_place_across_runs() {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "idle")],
                vec![run("r1", "a1", "running", "work")],
            ),
            1,
        );
        let entries = vec![
            entry(0, Some(10), "r1", AgentEvent::AssistantStarted),
            entry(60, Some(11), "r1", delta("hello")),
            entry(120, Some(12), "r1", delta(" world")),
            entry(
                180,
                Some(13),
                "r1",
                AgentEvent::AssistantFinished {
                    text: "hello world".into(),
                },
            ),
            entry(
                240,
                Some(14),
                "r1",
                AgentEvent::ToolStarted {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"src/main.rs"}"#.into(),
                },
            ),
            entry(
                300,
                Some(15),
                "r1",
                AgentEvent::ToolFinished {
                    id: "c1".into(),
                    result: serde_json::json!({"ok":false,"error":"missing file"}),
                },
            ),
            entry(
                360,
                Some(16),
                "r1",
                AgentEvent::ToolOutput {
                    id: "c1".into(),
                    stream: OutputStream::Stdout,
                    text: "ignored".into(),
                },
            ),
        ];
        projection.ingest("a1", &page(entries, false), 1_000);
        let rows = projection.rows();
        assert_eq!(
            rows.iter().filter(|row| row.kind == Kind::Response).count(),
            1,
            "一个 assistant turn 只产生一行"
        );
        let response = rows.iter().find(|row| row.kind == Kind::Response).unwrap();
        assert_eq!(response.summary, "hello world");
        assert_eq!(response.status, RowStatus::Success);
        let tools = rows
            .iter()
            .filter(|row| row.kind == Kind::Tool)
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 1, "ToolOutput 不产生新活动行");
        assert_eq!(tools[0].status, RowStatus::Error);
        assert_eq!(tools[0].summary, "missing file");
        assert_eq!(projection.agents()[0].tools, 1);
        // 新 Run 复用相同 call id 必须生成独立行。
        let more = vec![entry(
            420,
            Some(20),
            "r2",
            AgentEvent::ToolStarted {
                id: "c1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
        )];
        projection.ingest("a1", &page(more, false), 2_000);
        assert_eq!(
            projection
                .rows()
                .iter()
                .filter(|row| row.kind == Kind::Tool)
                .count(),
            2
        );
    }

    #[test]
    fn unknown_cost_and_tokens_stay_unknown() {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "idle")],
                vec![run("r1", "a1", "completed", "work")],
            ),
            1,
        );
        projection.ingest("a1", &page(Vec::new(), false), 10);
        let view = &projection.agents()[0];
        assert_eq!(view.cost, None, "未知 cost 不能补零");
        assert_eq!(view.tokens, None, "0 tokens 且无 usage 证据显示未知");
        assert_eq!(view.time_ms, Some(0));
        // 旧版只有 Run.tokens 时按总量展示。
        let mut legacy = Projection::default();
        let mut completed = run("r1", "a1", "completed", "work");
        completed.tokens = 5_000;
        legacy.apply_snapshot(
            snapshot(vec![record("a1", "Main", "idle")], vec![completed]),
            1,
        );
        assert_eq!(legacy.agents()[0].tokens, Some(5_000));
    }

    #[test]
    fn legacy_rows_keep_file_order_and_synthesize_once() {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "idle")],
                vec![run("r1", "a1", "failed", "legacy work")],
            ),
            1,
        );
        // 旧日志：只有事件、无 timestamp、无 RunState。
        let entries = vec![
            entry(0, None, "r1", AgentEvent::AssistantStarted),
            entry(
                90,
                None,
                "r1",
                AgentEvent::AssistantFinished {
                    text: "legacy answer".into(),
                },
            ),
        ];
        projection.ingest("a1", &page(entries, false), 10);
        projection.ingest("a1", &page(Vec::new(), false), 10);
        let rows = projection.rows();
        assert_eq!(
            rows.iter().filter(|row| row.title == "Task").count(),
            1,
            "旧日志只合成一次 task 关联"
        );
        assert_eq!(
            rows.iter().filter(|row| row.title == "Run").count(),
            1,
            "旧日志只合成一次终态"
        );
        let task = rows.iter().position(|row| row.title == "Task").unwrap();
        let first_event = rows
            .iter()
            .position(|row| row.kind == Kind::Response)
            .unwrap();
        assert!(task < first_event, "task 关联排在 Run 第一条事件之前");
        let terminal = rows.iter().find(|row| row.title == "Run").unwrap();
        assert_eq!(terminal.status, RowStatus::Error);
        assert_eq!(terminal.timestamp, None, "合成行不带伪造时间");
        let without = rows.iter().filter(|row| row.timestamp.is_none()).count();
        let with = rows.iter().filter(|row| row.timestamp.is_some()).count();
        assert!(with <= 1 && without >= 3);
        // 无时间记录排在有时间记录之前。
        let first_ts = rows.iter().position(|row| row.timestamp.is_some());
        if let Some(first_ts) = first_ts {
            assert!(rows[..first_ts].iter().all(|row| row.timestamp.is_none()));
        }
    }

    #[test]
    fn tree_keeps_lineage_and_protects_cycles() {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![
                    record("a1", "Main", "idle"),
                    record("a2", "a1", "idle"),
                    record("a3", "Main", "idle"),
                ],
                vec![],
            ),
            1,
        );
        let rows = projection.list(true, "a2");
        let ids = rows
            .iter()
            .map(|row| projection.agents()[row.agent].id.clone())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["a1", "a2"], "过滤保留 lineage");
        assert!(rows[0].muted && !rows[1].muted);
        // 孤立 parent 与循环都不能丢行或死循环。
        let mut orphan = Projection::default();
        orphan.apply_snapshot(
            snapshot(
                vec![
                    record("a1", "ghost", "idle"),
                    record("a2", "a3", "idle"),
                    record("a3", "a2", "idle"),
                ],
                vec![],
            ),
            1,
        );
        let rows = orphan.list(true, "");
        assert_eq!(rows.len(), 3);
        assert_eq!(orphan.list(true, "zzz").len(), 0, "无匹配显示空列表");
    }

    #[test]
    fn subtree_scope_and_error_category() {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![
                    record("a1", "Main", "idle"),
                    record("a2", "a1", "idle"),
                    record("a3", "Main", "idle"),
                ],
                vec![run("r1", "a1", "running", "parent task")],
            ),
            1,
        );
        let entries = vec![
            entry(0, Some(5), "r1", AgentEvent::AssistantStarted),
            entry(
                60,
                Some(6),
                "r1",
                AgentEvent::ToolStarted {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            ),
            entry(
                120,
                Some(7),
                "r1",
                AgentEvent::ToolFinished {
                    id: "c1".into(),
                    result: serde_json::json!({"ok":false,"error":"boom"}),
                },
            ),
        ];
        projection.ingest("a1", &page(entries, false), 10);
        let all = projection.activity(Category::All, Scope::All, None, "");
        assert_eq!(all.len(), 3);
        let errors = projection.activity(Category::Errors, Scope::All, Some("a1"), "");
        assert_eq!(errors.len(), 1);
        assert_eq!(projection.rows()[errors[0]].status, RowStatus::Error);
        let subtree = projection.activity(Category::All, Scope::Subtree, Some("a1"), "");
        assert_eq!(subtree.len(), 3, "subtree 包含自身与后代，不含 sibling");
        let selected = projection.activity(Category::All, Scope::Selected, Some("a3"), "");
        assert!(selected.is_empty(), "scope 无有效 Agent 时明确空态");
        let tools = projection.activity(Category::Tools, Scope::All, None, "read");
        assert!(tools.is_empty());
        let searched = projection.activity(Category::All, Scope::All, None, "BASH");
        assert_eq!(searched.len(), 1, "搜索大小写不敏感");
    }

    #[test]
    fn frozen_rank_ignores_status_heartbeat() {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "idle"), record("a2", "Main", "parked")],
                vec![],
            ),
            1,
        );
        assert_eq!(
            projection
                .agents()
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            ["a1", "a2"],
            "初次按 running/idle/parked/aborted 排序"
        );
        // 状态心跳不重排。
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "parked"), record("a2", "Main", "idle")],
                vec![run("r1", "a2", "running", "new work")],
            ),
            2,
        );
        assert_eq!(
            projection
                .agents()
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            ["a1", "a2"]
        );
        // 新批次插到前面。
        projection.apply_snapshot(
            snapshot(
                vec![
                    record("a1", "Main", "parked"),
                    record("a2", "Main", "idle"),
                    record("a3", "Main", "idle"),
                ],
                vec![],
            ),
            3,
        );
        assert_eq!(projection.agents()[0].id, "a3");
    }

    #[test]
    fn activity_row_ids_are_stable_across_ingest() {
        let mut projection = Projection::default();
        projection.apply_snapshot(snapshot(vec![record("a1", "Main", "idle")], vec![]), 1);
        projection.ingest(
            "a1",
            &page(
                vec![entry(0, Some(10), "r1", AgentEvent::AssistantStarted)],
                false,
            ),
            10,
        );
        let id = projection.rows()[0].id();
        projection.ingest(
            "a1",
            &page(vec![entry(30, Some(11), "r1", delta("more"))], false),
            20,
        );
        assert_eq!(projection.rows().len(), 1);
        assert_eq!(projection.rows()[0].id(), id);
    }
}
