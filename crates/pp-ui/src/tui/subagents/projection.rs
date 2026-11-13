//! Hub 的纯数据投影：只从类型化 registry 与观察记录计算列表、inspector 和
//! Activity，不调用模型、不读取任意 artifact 正文、不改变 result consumed。

use pp_core::context::ContextUsage;
use pp_core::events::AgentEvent;
use pp_runtime::subagent::EventPage;
use pp_runtime::subagent::HubEvent;
use pp_runtime::subagent::HubSnapshot;
use pp_runtime::subagent::ObservedEntry;
use pp_runtime::subagent::Run;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

mod rows;
mod summary;
mod text;
mod window;

pub(in crate::tui) use text::now_ms;
pub(super) use text::truncate;

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
    brief: Option<String>,
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
    pub role: Option<&'static str>,
    pub registered_ms: Option<u64>,
    pub parent: String,
    pub model: String,
    /// 子 Agent 的工作目录；isolated Agent 由 runtime 已改写为 worktree 路径。
    pub cwd: PathBuf,
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
    pub active_ms: Option<u64>,
    pub span_ms: Option<u64>,
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
struct WorkSummary {
    draft: String,
    latest: String,
    tool: String,
}

impl WorkSummary {
    fn current(&self) -> Option<String> {
        [&self.draft, &self.latest, &self.tool]
            .into_iter()
            .find(|text| !text.trim().is_empty())
            .map(|text| truncate(text))
    }
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
    ended: BTreeMap<String, u64>,
    work: BTreeMap<String, WorkSummary>,
    usage_in: u64,
    usage_out: u64,
    usage_seen: bool,
    context: Option<ContextUsage>,
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
}

#[cfg(test)]
mod fixtures {
    use super::*;
    use pp_core::events::AgentEvent;
    use pp_core::model::ThinkLevel;
    use pp_runtime::subagent::HubAgent;
    use pp_runtime::subagent::ObservedRecord;
    use pp_runtime::subagent::Record;
    use pp_runtime::subagent::Spec;

    pub(super) fn spec(depth: usize) -> Spec {
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
            depth,
            readonly: false,
            isolated: false,
            cost: None,
        }
    }

    pub(super) fn record(id: &str, parent: &str, status: &str) -> Record {
        Record {
            id: id.into(),
            name: format!("{id}-name"),
            parent: parent.into(),
            generation: 1,
            status: status.into(),
            spec: spec(1),
            session_file: None,
            registered_ms: None,
            updated_ms: 100,
        }
    }

    pub(super) fn run(id: &str, agent: &str, status: &str, task: &str) -> Run {
        Run {
            id: id.into(),
            agent_id: agent.into(),
            owner: "Main".into(),
            status: status.into(),
            task: task.into(),
            result: None,
            consumed: false,
            duration_ms: 0,
            started_ms: 0,
            requests: 0,
            tokens: 0,
            cost: None,
        }
    }

    pub(super) fn snapshot(agents: Vec<Record>, runs: Vec<Run>) -> HubSnapshot {
        HubSnapshot {
            output_previews: Vec::new(),
            pipeline_previews: Vec::new(),
            shell_previews: Vec::new(),
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

    pub(super) fn entry(
        offset: u64,
        timestamp: Option<u64>,
        run: &str,
        event: AgentEvent,
    ) -> ObservedEntry {
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

    pub(super) fn page(entries: Vec<ObservedEntry>, has_more: bool) -> EventPage {
        EventPage {
            entries,
            cursor: None,
            reset: false,
            has_more,
            warnings: vec![],
        }
    }

    pub(super) fn delta(text: &str) -> AgentEvent {
        AgentEvent::AssistantDelta {
            text: text.into(),
            refusal: false,
        }
    }
}
