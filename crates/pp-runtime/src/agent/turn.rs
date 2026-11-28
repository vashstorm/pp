mod batch;
mod completion;
mod log;
mod plan_progress;
mod request;
mod response;

use super::hooks::HookScope;
use super::*;

pub(super) use completion::completion_turn;
pub(super) use log::{log_model_request_finished, log_task_finished};
pub(super) use plan_progress::{
    apply_todo_change, publish_plan_progress, publish_todo_snapshot, publish_todo_state,
    todo_snapshot,
};
pub(super) use request::{
    ReportedProjectLoads, append_developer_to, append_input_to, append_user_to,
    publish_project_load, reported_project_load, response_input,
};
pub(super) use response::response_turn;

pub(super) struct RequestUpdate<'a> {
    pub(super) user: Option<&'a str>,
    pub(super) instructions: Option<&'a str>,
    pub(super) debug: &'a mut Option<debug::DebugState>,
    pub(super) pending_project_load: &'a mut Vec<pp_tools::project_instructions::Document>,
    pub(super) reported_project_load: &'a mut ReportedProjectLoads,
}

impl RequestUpdate<'_> {
    fn record_prompt(&mut self, session: SessionRef<'_>, metadata: &Metadata<'_>, id: &str) {
        if let Some(debug) = self.debug
            && debug.prompt(session, metadata, id).is_err()
        {
            debug::DebugState::warn_unavailable();
            *self.debug = None;
        }
    }
}

// Both protocols share the same synchronous dependencies within one model/tool turn.
pub(super) struct TurnResources<'a> {
    pub(super) skills: &'a [pp_config::skills::SkillHeader],
    pub(super) client: &'a mut OpenAiClient,
    pub(super) last_request: &'a mut Option<TokenUsage>,
    /// Cumulative session output tokens, updated in lockstep with `last_request`.
    pub(super) session_output_tokens: &'a mut u64,
    /// Thinking/reasoning records of the already committed turns of this session.
    pub(super) thinking_history: &'a mut Vec<super::context_diagnosis::ThinkingEntry>,
    pub(super) tools: &'a Tools,
    pub(super) journal: &'a mut SessionJournal,
    pub(super) workflow: &'a mut pp_core::plan::WorkflowState,
    pub(super) input: Option<&'a InputQueue>,
    pub(super) plan_dirty: &'a mut bool,
    pub(super) reminder: &'a mut todo::TodoReminderState,
    pub(super) hooks: HookScope,
}

fn tool_policy<'a>(
    workflow: &'a pp_core::plan::WorkflowState,
    sealed: bool,
    input: Option<&'a InputQueue>,
) -> Result<pp_tools::ToolPolicy<'a>> {
    Ok(match workflow.mode {
        pp_core::plan::WorkMode::Execute => pp_tools::ToolPolicy::Execute { workflow },
        pp_core::plan::WorkMode::Plan => pp_tools::ToolPolicy::Plan {
            document: workflow
                .plan
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No current plan."))?,
            sealed,
            input: input.map(|q| q as &dyn pp_tools::PlanSubmitGate),
        },
    })
}

pub(super) enum TurnOutcome {
    Continue,
    Completed {
        awaiting_user_answer: bool,
        /// Final assistant text of this turn; the Stop hook's `last_assistant_message` uses it directly.
        text: String,
    },
    PlanSubmitted,
}
