//! Command/event protocol between the TUI worker and the main loop, plus the worker-side publish and ACK helpers.

use anyhow::Result;
use pp_config::config::ModelChoice;
use pp_config::model_state::Selection;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::RunOutcome;
use pp_core::events::AgentEvent;
use pp_core::model::ModelSource;
use pp_core::model::ThinkLevel;
use pp_core::plan::ExecutionContext;
use pp_core::plan::PlanReview;
use pp_core::plan::PlanReviewReport;
use pp_core::plan::ReviewToken;
use pp_core::plan::WorkflowState;
use pp_runtime::agent::Agent;
use pp_runtime::agent::ExecutionPreparation;
use pp_runtime::agent::context::CompactionOutcome;
use pp_runtime::agent::input::InputQueue;
use pp_runtime::agent::plan_management::PlanListing;
use pp_runtime::agent::plan_management::PlanOpenIntent;
use pp_runtime::agent::plan_management::PlanSelection;
use std::{
    collections::BTreeMap,
    sync::mpsc,
    time::{Duration, Instant},
};
pub(super) enum WorkerCommand {
    Mcp {
        action: pp_mcp::Action,
        cancellation: CancellationToken,
    },
    ReloadModels,
    Shell {
        command: String,
        call_id: String,
        cancellation: CancellationToken,
    },
    #[cfg(debug_assertions)]
    TestWebSearch {
        queries: Vec<String>,
        provider: Option<pp_config::config::WebSearchProvider>,
        call_id: String,
        cancellation: CancellationToken,
    },
    RunInput {
        prepared: pp_runtime::user_input::UserInput,
        cancellation: CancellationToken,
        input: InputQueue,
    },
    Run {
        prompt: String,
        cancellation: CancellationToken,
        input: InputQueue,
    },
    Continue {
        cancellation: CancellationToken,
        input: InputQueue,
    },
    Compact {
        cancellation: CancellationToken,
    },
    UpdateCompactionPolicy {
        auto_compact: bool,
        threshold_percent: u8,
    },
    Sandbox(Option<pp_config::config::SandboxMode>),
    SyncPermissions,
    SyncSandboxSettings(pp_config::config::SandboxMode),
    SyncWebSearchSettings(pp_config::config::WebSearchSettings),
    InspectContext,
    Dump(pp_runtime::agent::DumpMode),
    Reset,
    /// A session root switch (/resume, /tree, /new) stops the old subagent root; the Hub rebuilds on demand while open.
    EnsureSubagents,
    SwitchModel(String),
    ApplyModelTargets,
    SaveDefaultThinkLevel(ThinkLevel),
    SaveDefaultModel(String),
    SaveModelRole(
        pp_config::config::ModelRole,
        Option<pp_config::config::RoleAssignment>,
    ),
    ToggleFavoriteModel(String),
    DeleteSession(String),
    Resume {
        id: String,
        cancellation: CancellationToken,
    },
    ReadTree,
    NavigateTree(String),
    EnterPlan {
        cancellation: CancellationToken,
    },
    ReadPlan,
    LeavePlan {
        cancellation: CancellationToken,
        discard_plan_id: Option<String>,
    },
    SavePlanAndLeave {
        token: ReviewToken,
        cancellation: CancellationToken,
        input: InputQueue,
    },
    // /plan-list management: list/preview/open/rename/delete, with all I/O done on the worker side.
    ListPlans,
    ReadListedPlan {
        id: String,
    },
    OpenListedPlan {
        selection: PlanSelection,
        intent: PlanOpenIntent,
        cancellation: CancellationToken,
    },
    RenameListedPlan {
        selection: PlanSelection,
        file_name: String,
    },
    DeleteListedPlan {
        selection: PlanSelection,
    },
    ApprovePlan {
        token: pp_core::plan::ReviewToken,
        context: ExecutionContext,
        cancellation: CancellationToken,
        input: InputQueue,
    },
    // Independent reviewer: progress flows back as ReviewProgress events, the terminal state is PlanReviewed.
    ReviewPlan {
        token: pp_core::plan::ReviewToken,
        cancellation: CancellationToken,
        input: InputQueue,
    },
}

#[derive(Clone, Copy)]
pub(super) enum DialogOrigin {
    Ask,
    Permission,
    #[cfg(debug_assertions)]
    Mock,
}
pub(super) enum WorkerEvent {
    McpManaged {
        result: Result<()>,
        view: pp_mcp::View,
        show: bool,
    },
    McpSnapshot(pp_mcp::View),
    DialogFinished {
        call_id: String,
    },
    ModelsReloaded(Result<ModelListing>),
    Ask {
        origin: DialogOrigin,
        request: pp_core::interaction::AskRequest,
        reply: mpsc::SyncSender<Vec<pp_core::interaction::AskAnswer>>,
        cancellation: CancellationToken,
    },
    Progress(AgentEvent),
    ShellCompleted(Result<()>),
    #[cfg(debug_assertions)]
    WebSearchCompleted(Result<()>),
    #[cfg(debug_assertions)]
    MockPrepared(pp_mock_ai::MockTestSnapshot),
    #[cfg(debug_assertions)]
    MockCompleted {
        result: Result<pp_mock_ai::MockTestReport>,
        elapsed: Duration,
        finished_at: chrono::DateTime<chrono::Local>,
    },
    Completed {
        result: Result<RunOutcome>,
        elapsed: Duration,
        finished_at: chrono::DateTime<chrono::Local>,
    },
    SessionStarted(String),
    // Compaction terminal state: a different discriminant from an ordinary task Completed, so no task-final display is triggered.
    Compacted(Result<CompactionOutcome>),
    SandboxStatus(Result<(pp_sandbox::SandboxStatus, bool)>),
    ContextInspected(Result<pp_runtime::agent::context_diagnosis::ContextReport>),
    Dumped(Result<pp_runtime::agent::DumpOutput>),
    Reset,
    ModelSwitched(Result<(ModelChoice, ThinkLevel)>),
    DefaultThinkLevelSaved(Result<ThinkLevel>),
    DefaultModelSaved(Result<String>),
    ModelRolesSaved(Result<(pp_config::config::ModelRoles, String, ThinkLevel)>),
    ModelState {
        revision: u64,
        model: ModelChoice,
        effort: ThinkLevel,
        source: ModelSource,
    },
    FavoriteModelsSaved(Result<Vec<String>>),
    SessionDeleted(Result<()>),
    SessionResumed(Result<ResumeState>),
    /// Subagent root binding rebuild result: affects only Hub readability, never session state.
    SubagentsEnsured(Result<()>),
    TreeRead(Result<pp_runtime::agent::persistence::tree::View>),
    TreeNavigated(Result<pp_runtime::agent::TreeNavigation>),
    // /plan-list management result; on failure the old list and input are kept and only a visible error is shown.
    PlansListed(Result<PlanListing>),
    ListedPlanRead(Result<PlanSelection>),
    // Carries full execution preparation: the UI adopts the workflow/replay/todo snapshots but does not request the model automatically.
    ListedPlanOpened(Result<ExecutionPreparation>),
    ListedPlanRenamed(Result<PlanListing>),
    ListedPlanDeleted(Result<PlanListing>),
    // Plan mode switch result; on Err the worker follows immediately with a real workflow snapshot
    // so the UI does not show the wrong mode in a failed-switch notice.
    WorkflowChanged(Result<WorkflowState>),
    PlanRead(Result<PlanReview>),
    // Review progress: goes only to the dedicated review transcript, never to the main conversation, usage or dispatch.
    ReviewProgress(AgentEvent),
    // Reviewer terminal state: success carries the final report; failure/cancel keeps the last complete report.
    PlanReviewed(Result<PlanReviewReport>),
    // Snapshot published after actions without a dedicated result event (plain runs/compaction/model switches/tree navigation).
    Workflow(WorkflowState),
    // lazy=false background warm-up failed: isolated to one server, ordinary tools are unaffected.
    LspWarmupFailed {
        server: String,
        error: String,
    },
    // Error when an idle poll receives a late diagnostic; deduplicated and surfaced to the UI without ending the session.
    LspPollFailed(String),
    // One-shot handoff: the worker continues only after the UI adopts the workflow/replay/usage/todo snapshots and ACKs.
    ExecutionPrepared {
        workflow: WorkflowState,
        session_id: Option<String>,
        usage: Option<pp_core::context::ContextUsage>,
        replay: Option<Vec<pp_runtime::agent::persistence::ReplayRecord>>,
        /// Handoff snapshot: the UI must render after adopting replay/replacing the workflow and before the ACK,
        /// so it enters the card only after Fresh cleanup and the source session cannot be emitted and then cleared.
        todo_snapshot: Option<AgentEvent>,
        reply: mpsc::SyncSender<()>,
    },
}

/// Correction snapshot after a failed switch: publishes the real workflow and model fields; shared by all Plan switch failure paths.
pub(super) fn publish_transition_failure(agent: &Agent, snapshots: &mut Vec<WorkerEvent>) {
    push_workflow_snapshot(agent, snapshots);
}

/// Publish the workflow snapshot so the UI never keeps a stale plan/mode.
pub(super) fn push_workflow_snapshot(agent: &Agent, snapshots: &mut Vec<WorkerEvent>) {
    snapshots.push(WorkerEvent::model_state(agent));
    snapshots.push(WorkerEvent::Workflow(agent.workflow().clone()));
}

/// Status-bar usage plus workflow snapshot after a task or state boundary.
pub(super) fn push_runtime_snapshot(agent: &Agent, snapshots: &mut Vec<WorkerEvent>) {
    if let Some(event) = usage_snapshot(agent) {
        snapshots.push(event);
    }
    push_workflow_snapshot(agent, snapshots);
}

/// Send one UI-bound event with a reply channel and wait for the ACK; a
/// disconnected UI or cancellation cancels the worker.
fn send_and_wait_ack<T: Send>(
    events: &mpsc::SyncSender<WorkerEvent>,
    make: impl FnOnce(mpsc::SyncSender<T>) -> WorkerEvent,
    cancellation: &CancellationToken,
) -> Result<T> {
    let (reply, ack) = mpsc::sync_channel(1);
    if events.send(make(reply)).is_err() {
        cancellation.cancel();
        return Err(pp_core::cancellation::Cancelled.into());
    }
    loop {
        pp_core::cancellation::check(cancellation)?;
        match ack.recv_timeout(Duration::from_millis(50)) {
            Ok(value) => return Ok(value),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                cancellation.cancel();
                return Err(pp_core::cancellation::Cancelled.into());
            }
        }
    }
}

/// Publish ExecutionPrepared and wait for the ACK once the UI has adopted the snapshot.
/// Returns an error when the ACK fails or is cancelled.
pub(super) fn publish_prepared(
    events: &mpsc::SyncSender<WorkerEvent>,
    agent: &Agent,
    preparation: ExecutionPreparation,
    cancellation: &CancellationToken,
) -> Result<()> {
    events
        .send(WorkerEvent::model_state(agent))
        .map_err(|_| anyhow::anyhow!("UI disconnected during workflow transition"))?;
    send_and_wait_ack(
        events,
        |reply| WorkerEvent::ExecutionPrepared {
            workflow: preparation.workflow,
            session_id: preparation.session_id,
            usage: agent.context_usage().ok(),
            replay: preparation.replay,
            todo_snapshot: preparation.todo_snapshot,
            reply,
        },
        cancellation,
    )
}

pub(super) fn request_ask(
    events: &mpsc::SyncSender<WorkerEvent>,
    origin: DialogOrigin,
    request: pp_core::interaction::AskRequest,
    cancellation: &CancellationToken,
) -> Result<Vec<pp_core::interaction::AskAnswer>> {
    pp_core::cancellation::check(cancellation)?;
    let call_id = request.call_id.clone();
    let answers = send_and_wait_ack(
        events,
        |reply| WorkerEvent::Ask {
            origin,
            request,
            reply,
            cancellation: cancellation.clone(),
        },
        cancellation,
    );
    let _ = events.try_send(WorkerEvent::DialogFinished { call_id });
    pp_core::cancellation::check(cancellation)?;
    answers
}

impl WorkerEvent {
    pub(super) fn model_state(agent: &Agent) -> Self {
        Self::ModelState {
            // The snapshot carries both the model and the thinking level, so the version must reflect the latest application of either.
            revision: agent
                .runtime_models
                .as_ref()
                .map_or(0, |binding| binding.seen.max(binding.seen_think)),
            model: agent.current_model().clone(),
            effort: agent.think_level(),
            source: agent.model_source(),
        }
    }

    pub(super) fn completed(started: Instant, result: Result<RunOutcome>) -> Self {
        Self::Completed {
            result,
            elapsed: started.elapsed(),
            finished_at: chrono::Local::now(),
        }
    }
}

/// Model catalog snapshot fully loaded and validated by the worker.
pub(super) struct ModelListing {
    pub(super) roles: pp_config::config::ModelRoles,
    pub(super) default_effort: ThinkLevel,
    pub(super) choices: Vec<ModelChoice>,
    pub(super) default_reference: String,
    pub(super) favorites: Vec<String>,
    pub(super) usage: BTreeMap<String, u64>,
    /// `a`: manual override snapshot, used to restore the `/roles` read-only row and the source tag.
    pub(super) override_selection: Option<Selection>,
    /// Currently effective model source; the override may be corrected by `ModelState` after the catalog is reloaded.
    pub(super) source: ModelSource,
}

impl ModelListing {
    pub(super) fn from_agent(agent: &Agent) -> Result<Self> {
        Ok(Self {
            choices: agent.model_choices(),
            default_reference: agent.default_model_reference(),
            favorites: agent.favorite_models(),
            roles: agent.model_roles(),
            default_effort: agent.default_think_level(),
            usage: agent.model_usage()?,
            override_selection: agent.models().override_selection(),
            source: agent.model_source(),
        })
    }
}

/// Session switch artifacts prepared in one shot by the worker; UI state is replaced only once everything is ready.
/// History replay is handed over through ExecutionPrepared, so only the session ID is kept here.
pub(super) struct ResumeState {
    pub(super) id: String,
}

// The worker wraps the Agent usage snapshot in a progress event; estimation failures are skipped without aborting the command.
pub(super) fn usage_snapshot(agent: &Agent) -> Option<WorkerEvent> {
    agent
        .context_usage()
        .ok()
        .map(|usage| WorkerEvent::Progress(AgentEvent::ContextUpdated { usage }))
}
