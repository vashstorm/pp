//! Pure data projection of the Hub: it derives the list, the inspector and the
//! Activity only from the typed registry and observation records; it never calls the model, reads arbitrary artifact bodies or changes result consumed.

use pp_core::context::ContextUsage;
use pp_core::events::AgentEvent;
use pp_runtime::subagent::EventPage;
use pp_runtime::subagent::HubEvent;
use pp_runtime::subagent::HubSnapshot;
use pp_runtime::subagent::ObservedEntry;
use pp_runtime::subagent::OverviewPage;
use pp_runtime::subagent::Run;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

mod rows;
mod summary;
mod text;
mod window;

pub(in crate::tui) use text::now_ms;
pub(super) use text::truncate;

/// Unicode scalar cap kept for an Activity summary; the full output lives only in the detail.
const SUMMARY_SCALARS: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum AgentStatus {
    Running,
    Queued,
    Completed,
    Idle,
    Parked,
    Aborted,
}

impl AgentStatus {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Queued => "queued",
            Self::Completed => "completed",
            Self::Idle => "idle",
            Self::Parked => "parked",
            Self::Aborted => "aborted",
        }
    }

    pub(super) fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Queued)
    }

    fn roster_priority(self) -> u8 {
        match self {
            Self::Running => 0,
            Self::Queued => 1,
            Self::Completed => 2,
            Self::Idle | Self::Parked | Self::Aborted => 3,
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

/// One Activity line; `offset` is the starting source line offset used to position the detail.
#[derive(Clone, Debug)]
pub(super) struct ActivityRow {
    /// Stable row key; used across re-sorts, paused follow and detail anchors.
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
    /// Sort phase of rows synthesized from old logs: task=0, event=1, terminal=2.
    phase: u8,
}

impl ActivityRow {
    pub(super) fn id(&self) -> String {
        self.key.clone()
    }

    pub(super) fn display_title(&self) -> std::borrow::Cow<'_, str> {
        if self.kind == Kind::Tool {
            crate::tui::transcript_style::tool_label(&self.title)
        } else {
            self.title.as_str().into()
        }
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
    /// Working directory of the subagent; for an isolated Agent the runtime already rewrote it to the worktree path.
    pub cwd: PathBuf,
    pub thinking: String,
    pub model_change: Option<String>,
    pub status: AgentStatus,
    /// After a forced stop the Run may still be wrapping up.
    pub has_active_run: bool,
    /// A finished task may still belong to a parked Agent at the same time.
    pub parked: bool,
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
    /// Metrics show loading while the full history scan is incomplete.
    pub loading: bool,
    pub context: Option<ContextUsage>,
    pub readonly: bool,
    pub isolated: bool,
    pub artifacts: Vec<String>,
    pub last_error: Option<String>,
    pub last_activity: Option<u64>,
}

/// A row in the tree or flat list; `prefix` already contains the branch glyphs.
#[derive(Clone, Debug)]
pub(super) struct TreeRow {
    pub agent: usize,
    pub prefix: String,
    /// Ancestor row kept only for lineage.
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
    /// `agent|run` -> current assistant turn count.
    turns: BTreeMap<String, u64>,
    /// Whether that run ever saw a RunState marker.
    markers: BTreeSet<String>,
    legacy_task: BTreeSet<String>,
    legacy_terminal: BTreeSet<String>,
    /// run -> timestamp (milliseconds) of the real running state.
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
pub(in crate::tui) struct Projection {
    snapshot: Option<HubSnapshot>,
    agents: Vec<AgentView>,
    index: BTreeMap<String, usize>,
    /// (id, generation) -> frozen sort weight; a larger weight comes first.
    ranks: BTreeMap<(String, u64), u64>,
    next_rank: u64,
    rows: Vec<ActivityRow>,
    keys: BTreeMap<String, usize>,
    streams: BTreeMap<String, Stream>,
    /// Agents whose full history scan finished.
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

    #[cfg(test)]
    pub(in crate::tui) fn agent_ready(&self, id: &str) -> bool {
        self.agent(id).is_some_and(|agent| !agent.loading)
    }

    pub(super) fn rows(&self) -> &[ActivityRow] {
        &self.rows
    }

    /// Treated as loading before the first snapshot; the empty state must not flash first.
    pub(super) fn loading(&self) -> bool {
        self.snapshot.is_none()
    }

    /// The old projection and detail anchor of that agent are stale: only clear that lane.
    pub(in crate::tui) fn reset(&mut self, agent: &str) {
        self.rows.retain(|row| row.agent != agent);
        self.streams.remove(agent);
        self.scanned.remove(agent);
        self.reindex();
        self.sorted = false;
    }

    /// Once-per-second clock refresh: recompute age and the elapsed time of active Runs.
    pub(super) fn refresh(&mut self, now_ms: u64) {
        if self.snapshot.is_some() {
            self.rebuild(now_ms);
        }
    }

    pub(in crate::tui) fn apply_snapshot(&mut self, snapshot: HubSnapshot, now_ms: u64) {
        self.snapshot = Some(snapshot);
        self.needs_finalize = true;
        self.rebuild(now_ms);
    }

    /// Ingest one page of observation records; `page.has_more == false` means that agent's history is fully read.
    pub(in crate::tui) fn ingest(&mut self, agent: &str, page: &EventPage, now_ms: u64) {
        for entry in &page.entries {
            self.consume(agent, entry);
        }
        if !page.has_more {
            self.scanned.insert(agent.to_owned());
            self.needs_finalize = true;
        }
        self.rebuild(now_ms);
    }

    /// Ingest a batched overview/Activity aggregation page; projection is rebuilt exactly once after the whole page is consumed.
    pub(in crate::tui) fn ingest_overview_batch(
        &mut self,
        pages: &[(String, OverviewPage)],
        now_ms: u64,
    ) {
        if pages.is_empty() {
            return;
        }
        for (agent, page) in pages {
            if let Some(ts) = page.last_activity_ms {
                let stream = self.streams.entry(agent.clone()).or_default();
                stream.last_activity = Some(stream.last_activity.map_or(ts, |old| old.max(ts)));
            }
            for entry in &page.entries {
                self.consume(agent, entry);
            }
            if !page.has_more {
                self.scanned.insert(agent.clone());
                self.needs_finalize = true;
            }
        }
        self.rebuild(now_ms);
    }

    fn rebuild(&mut self, now_ms: u64) {
        self.finalize();
        if let Some(snapshot) = self.snapshot.take() {
            self.freeze_ranks(&snapshot);
            self.rebuild_agents(&snapshot, now_ms);
            self.snapshot = Some(snapshot);
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
