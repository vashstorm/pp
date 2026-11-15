//! 未执行调用合成的 `ok:false` 结果与批次失败优先级。
use super::super::hooks::HookHalt;
use super::*;

/// 未执行的调用仍然要产生一条 `ok:false` 结果与成对的工具事件；
/// 合成事件同样进入展示捕获边界，使恢复与会话树能看到这些卡片。
/// 取消导致的跳过保留 `cancelled` 标记，这样工具统计与卡片按取消归类，
/// 不会因为 `HookHalt::Failure` 而变成错误。
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

/// CleanupError 优先：既不能让 hook 反馈掩盖回收失败，也不能反过来。
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
