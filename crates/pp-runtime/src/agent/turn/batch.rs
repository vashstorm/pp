//! Synthesized `ok:false` results for calls that were not executed and batch failure precedence.
use super::super::hooks::HookHalt;
use super::*;

/// Calls that did not run still need an `ok:false` result and a paired tool event;
/// the synthesized events also enter the display capture boundary so a restore and the session tree show these cards.
/// Skips caused by cancellation keep the `cancelled` marker, so tool statistics and cards classify them as cancelled
/// instead of turning them into errors because of `HookHalt::Failure`.
pub(super) fn not_executed(
    journal: &mut SessionJournal,
    emit: &mut dyn FnMut(AgentEvent),
    call_id: &str,
    name: &str,
    arguments: &str,
    halt: &HookHalt,
) -> String {
    let started = AgentEvent::ToolStarted {
        id: call_id.to_owned(),
        name: name.to_owned(),
        arguments: arguments.to_owned(),
    };
    journal.event(&started);
    emit(started);
    let result = match halt {
        HookHalt::Stop(reason) => {
            serde_json::json!({"ok": false, "error": format!("Not executed: {reason}")})
        }
        HookHalt::Failure(error) if error.chain().any(|cause| cause.is::<Cancelled>()) => {
            serde_json::json!({"ok": false, "cancelled": true, "error": "Not executed: task cancelled."})
        }
        HookHalt::Failure(error) => {
            serde_json::json!({"ok": false, "error": format!("Not executed: {error:#}")})
        }
    };
    let finished = AgentEvent::ToolFinished {
        id: call_id.to_owned(),
        result: result.clone(),
    };
    journal.event(&finished);
    emit(finished);
    let serialized = result.to_string();
    journal.record_tool_output(call_id, &serialized);
    serialized
}

/// CleanupError takes precedence: neither can hook feedback mask the cleanup failure, nor the reverse.
pub(super) fn batch_failure(
    hook: Option<anyhow::Error>,
    tool: Option<anyhow::Error>,
) -> Option<anyhow::Error> {
    match (hook, tool) {
        (Some(hook), Some(tool)) => Some(if hook.is::<pp_core::cancellation::CleanupError>() {
            hook
        } else if tool.is::<pp_core::cancellation::CleanupError>() {
            tool
        } else {
            hook
        }),
        (hook, tool) => hook.or(tool),
    }
}
