//! 单次工具调用的启动指标、结束收尾日志、shell 展示字段剥离还原与 post hook。
use super::*;

pub(super) fn tool_metric_name(name: &str) -> &'static str {
    match name {
        "ask" => "ask",
        "read" => "read",
        "edit" => "edit",
        "write" => "write",
        "bash" => "bash",
        "bash_task" => "bash_task",
        "wait" => "wait",
        "rg" => "rg",
        "glob" => "glob",
        "todo" => "todo",
        "lsp" => "lsp",
        "plan_search" | "plan_find" | "plan_submit" => "plan_tool",
        _ => "external",
    }
}

pub(super) fn finalize_tool_execution(
    call: &mut ExecuteCall<'_, '_>,
    hook: Option<&dyn ToolHookCall>,
    pid: u32,
    tool: &str,
    started: Instant,
    value: &mut Value,
    has_stop_error: bool,
) -> (Vec<String>, Option<HookHalt>, String) {
    let status = if value.get("cancelled").and_then(Value::as_bool) == Some(true) {
        "cancelled"
    } else if value.get("ok").and_then(Value::as_bool) == Some(true) {
        "ok"
    } else {
        "error"
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    log_tool_finished(pid, tool, status, elapsed_ms, value, call.arguments);
    let shell_display = if matches!(tool, "bash" | "bash_task" | "wait") {
        remove_shell_display(value)
    } else {
        Vec::new()
    };
    // post hook 在模型输出序列化前、shell 展示字段已剥离时运行；
    // `value` 始终保留真实结果，只有序列化文本可被替换。
    let mut hook_context = hook.map_or_else(Vec::new, |hook| hook.pre_context().to_vec());
    let mut hook_halt = None;
    let mut model_value = None;
    let denied_by_hook = hook.and_then(|hook| hook.deny_reason());
    let cancelled = value.get("cancelled").and_then(Value::as_bool) == Some(true);
    if let Some(hook) = hook
        && denied_by_hook.is_none()
        && !has_stop_error
        && !cancelled
    {
        run_post_tool_hook(
            call,
            hook,
            status,
            value,
            &mut hook_context,
            &mut hook_halt,
            &mut model_value,
        );
    }
    let serialized = match model_value {
        Some(updated) => updated.to_string(),
        None => value.to_string(),
    };
    for (path, shell) in shell_display {
        restore_shell_display(value, &path, shell);
    }
    (call.emit)(AgentEvent::ToolFinished {
        id: call.call_id.to_owned(),
        result: value.clone(),
    });
    (hook_context, hook_halt, serialized)
}

fn log_tool_finished(
    pid: u32,
    tool: &str,
    status: &str,
    elapsed_ms: u64,
    value: &Value,
    arguments: &str,
) {
    if status == "error" {
        tracing::warn!(
            event = "tool_finished",
            pid,
            tool,
            status,
            elapsed_ms,
            error = pp_core::logging::diagnostic_text(
                value
                    .get("error")
                    .and_then(|value| value.as_str())
                    .unwrap_or("tool reported failure")
            ),
            arguments = pp_core::logging::diagnostic_text(arguments)
        );
    } else {
        tracing::info!(event = "tool_finished", pid, tool, status, elapsed_ms);
    }
}

fn run_post_tool_hook(
    call: &mut ExecuteCall<'_, '_>,
    hook: &dyn ToolHookCall,
    status: &str,
    value: &Value,
    hook_context: &mut Vec<String>,
    hook_halt: &mut Option<HookHalt>,
    model_value: &mut Option<Value>,
) {
    match hook.run_post_tool(
        crate::PostToolCall {
            is_error: status == "error",
            name: call.name,
            call_id: call.call_id,
            arguments: call.arguments,
            value,
        },
        call.cancellation,
        call.emit,
    ) {
        Ok(Some(outcome)) => {
            hook_context.extend(outcome.additional_context);
            *model_value = outcome.updated_output;
            if let Some(reason) = outcome.stop_reason {
                *hook_halt = Some(HookHalt::Stop(reason));
            }
        }
        Ok(None) => {}
        // 取消与无法回收的进程直接停止任务，不能降级为工具失败。
        Err(error) => *hook_halt = Some(HookHalt::Failure(error)),
    }
}
