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
    pub(super) pending_project_load: &'a mut Vec<crate::project_instructions::Document>,
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

// 两种协议在一次模型／工具轮次中使用相同的同步依赖。
pub(super) struct TurnResources<'a> {
    pub(super) skills: &'a [pp_config::skills::SkillHeader],
    pub(super) client: &'a mut OpenAiClient,
    pub(super) last_request: &'a mut Option<TokenUsage>,
    /// 会话累计 output tokens，与 `last_request` 同步更新。
    pub(super) session_output_tokens: &'a mut u64,
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
) -> Result<crate::tools::ToolPolicy<'a>> {
    Ok(match workflow.mode {
        pp_core::plan::WorkMode::Execute => crate::tools::ToolPolicy::Execute { workflow },
        pp_core::plan::WorkMode::Plan => crate::tools::ToolPolicy::Plan {
            document: workflow
                .plan
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("No current plan."))?,
            sealed,
            input,
        },
    })
}

pub(super) enum TurnOutcome {
    Continue,
    Completed {
        awaiting_user_answer: bool,
        /// 本次最终助手文本；Stop hook 的 `last_assistant_message` 直接用它。
        text: String,
    },
    PlanSubmitted,
}
