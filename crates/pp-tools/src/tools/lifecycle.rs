//! 单次工具调用的启动指标、结束收尾日志、冗余展示字段剥离还原、全局大输出落盘护栏与 post hook。
use super::*;
use pp_config::config::OutputSettings;
use pp_core::events::OutputStream;

/// 全局工具输出护栏上限：对未自带分页/artifact 的工具（如 MCP/LSP/web_search）
/// 超过该字节数时自动落盘到 `artifact://` 并仅向模型返回有界头尾预览。
const GLOBAL_SPILL_CAP_BYTES: usize = 32 * 1024;

pub(super) fn tool_metric_name(name: &str) -> &'static str {
    match name {
        "ask" => "ask",
        "read" => "read",
        "edit" => "edit",
        "write" => "write",
        "bash" => "bash",
        "bash_task" => "bash_task",
        "eval" => "eval",
        "wait" => "wait",
        "rg" => "rg",
        "glob" => "glob",
        "todo" => "todo",
        "lsp" => "lsp",
        "plan_search" | "plan_find" | "plan_submit" => "plan_tool",
        _ => "external",
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn finalize_tool_execution(
    call: &mut ExecuteCall<'_, '_>,
    hook: Option<&dyn ToolHookCall>,
    pid: u32,
    tool: &str,
    started: Instant,
    value: &mut Value,
    has_stop_error: bool,
    artifacts: &output::Store,
    output_settings: OutputSettings,
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
    let projection = ModelProjection::strip(tool, value);
    // post hook 在模型输出序列化前、UI 展示与冗余字段已剥离时运行；
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
    let serialized =
        spill_oversized_model_output(call.name, value, serialized, artifacts, output_settings);
    projection.restore(value);
    (call.emit)(AgentEvent::ToolFinished {
        id: call.call_id.to_owned(),
        result: value.clone(),
    });
    (hook_context, hook_halt, serialized)
}

fn spill_oversized_model_output(
    tool_name: &str,
    value: &Value,
    serialized: String,
    artifacts: &output::Store,
    output_settings: OutputSettings,
) -> String {
    let guarded =
        tool_name.starts_with("mcp__") || matches!(tool_name, "mcp_call" | "lsp" | "web_search");
    if !guarded || value.get("kind").and_then(Value::as_str) == Some("image") {
        return serialized;
    }
    let threshold = output_settings
        .artifact_spill_threshold
        .min(GLOBAL_SPILL_CAP_BYTES);
    if serialized.len() <= threshold {
        return serialized;
    }
    let head_bytes = output_settings
        .artifact_head_bytes
        .min(threshold / 2)
        .max(1);
    let sink_settings = OutputSettings {
        artifact_spill_threshold: threshold,
        artifact_head_bytes: head_bytes,
        output_max_columns: 0,
        artifact_max_bytes: output_settings.artifact_max_bytes,
    };
    let mut sink = output::OutputSink::new(sink_settings, artifacts.clone());
    sink.append(OutputStream::Stdout, serialized.as_bytes());
    sink.finish();
    let mut spilled = sink.value();
    spilled["ok"] = Value::Bool(value.get("ok").and_then(Value::as_bool).unwrap_or(true));
    spilled["spilled"] = Value::Bool(true);
    spilled["tool"] = Value::String(tool_name.to_owned());
    spilled.to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_external_tool_output_spills_to_artifact_while_builtin_tools_stay_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store = output::Store::default();
        store.bind(dir.path(), "session").unwrap();
        let big_body = "x".repeat(GLOBAL_SPILL_CAP_BYTES + 4096);
        let value = json!({"ok": true, "content": big_body});
        let raw = value.to_string();

        // MCP/LSP 等无内置落盘的工具超过 32 KiB 时落盘为 artifact:// 并返回有界预览。
        let spilled_str = spill_oversized_model_output(
            "mcp__demo__fetch",
            &value,
            raw.clone(),
            &store,
            OutputSettings::default(),
        );
        let spilled: Value = serde_json::from_str(&spilled_str).unwrap();
        assert_eq!(spilled["ok"], true);
        assert_eq!(spilled["spilled"], true);
        assert_eq!(spilled["tool"], "mcp__demo__fetch");
        assert_eq!(spilled["truncated"], true);
        let uri = spilled["artifact"]["uri"].as_str().unwrap();
        assert!(uri.starts_with("artifact://"), "{spilled}");
        assert!(spilled_str.len() < raw.len());
        let snapshot = store.snapshot(uri, &CancellationToken::new()).unwrap();
        assert_eq!(std::fs::read_to_string(snapshot.path).unwrap(), raw);

        // 已自带分页/artifact 的内置工具（如 read/bash/rg/eval）不重复二次包裹。
        let untouched = spill_oversized_model_output(
            "read",
            &value,
            raw.clone(),
            &store,
            OutputSettings::default(),
        );
        assert_eq!(untouched, raw);
    }
}
