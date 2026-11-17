//! `eval` 工具分支：持久 Python REPL 内核执行与嵌套宿主工具回调桥。

use super::*;
use pp_core::cancellation::Cancelled;
use pp_workspace::image::decode_image;
use std::cell::Cell;

impl Tools {
    pub(in crate::tools) fn run_eval(
        &self,
        call: &mut ExecuteCall<'_, '_>,
        hook: Option<&dyn ToolHookCall>,
        images: &mut Vec<read::ToolImage>,
    ) -> Result<(Value, Option<PlanChange>, Option<todo::TodoChange>)> {
        let args: EvalArgs =
            serde_json::from_str(call.arguments).context("invalid eval arguments")?;
        let EvalLanguage::Py = args.language;
        let _ = args.title.as_deref();
        ensure!(!args.code.trim().is_empty(), "code must not be blank");
        let timeout_seconds = args
            .timeout_seconds
            .unwrap_or(self.limits.eval_timeout_seconds);
        ensure!(
            (1..=self.limits.eval_max_timeout_seconds).contains(&timeout_seconds),
            "timeout_seconds must be an integer between 1 and {} seconds (eval.max_timeout_seconds); omit it to use {} seconds.",
            self.limits.eval_max_timeout_seconds,
            self.limits.eval_timeout_seconds
        );
        let cache_dir = self.workspace.cache_dir()?;
        let sink = output::OutputSink::new(self.limits.output, self.artifacts.clone());

        let policy = call.policy;
        let cancellation = call.cancellation;
        let outer_call_id = call.call_id;
        let seq = Cell::new(0u64);
        let mut todo_change: Option<todo::TodoChange> = None;
        let mut nested_hook_context: Vec<String> = Vec::new();
        let mut nested_hook_halt: Option<HookHalt> = None;

        let outcome = {
            let emit = RefCell::new(&mut *call.emit);
            let fetch = &mut *call.fetch;
            let mut forward_emit = |event: AgentEvent| (emit.borrow_mut())(event);
            self.eval.execute(
                pp_eval::CellRequest {
                    cwd: &self.cwd,
                    cache_dir: &cache_dir,
                    interpreter: self.eval_interpreter.as_deref(),
                    code: &args.code,
                    call_id: outer_call_id,
                    timeout_seconds,
                    reset: args.reset,
                },
                cancellation,
                sink,
                &mut forward_emit,
                &mut |tool_name, tool_args| {
                    let next = seq.get().saturating_add(1);
                    seq.set(next);
                    let bridge_call_id = format!("{outer_call_id}:py:{next}");
                    let mut bridge_emit = |event: AgentEvent| (emit.borrow_mut())(event);
                    self.run_eval_bridge_call(
                        tool_name,
                        tool_args,
                        &bridge_call_id,
                        policy,
                        hook,
                        cancellation,
                        &mut bridge_emit,
                        fetch,
                        images,
                        &mut todo_change,
                        &mut nested_hook_context,
                        &mut nested_hook_halt,
                    )
                },
            )?
        };

        call.nested_hook_context.extend(nested_hook_context);
        if call.nested_hook_halt.is_none() {
            call.nested_hook_halt = nested_hook_halt;
        }

        for captured in outcome.images {
            match decode_image(&format!("eval:{}", captured.mime_type), &captured.bytes) {
                Ok(decoded) => images.extend(decoded.images),
                Err(error) => tracing::warn!(
                    event = "eval_image_decode_failed",
                    error = %error,
                    "dropping invalid image emitted by Python eval"
                ),
            }
        }

        Ok((outcome.value, None, todo_change))
    }

    #[allow(clippy::too_many_arguments)]
    fn run_eval_bridge_call(
        &self,
        name: &str,
        args: &Value,
        call_id: &str,
        policy: ToolPolicy<'_>,
        hook: Option<&dyn ToolHookCall>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
        fetch: &mut read::Fetch<'_>,
        images: &mut Vec<read::ToolImage>,
        todo_change: &mut Option<todo::TodoChange>,
        hook_context: &mut Vec<String>,
        hook_halt: &mut Option<HookHalt>,
    ) -> Result<Value> {
        if matches!(name, "eval" | "yield") || is_plan_tool(name) || name.starts_with("plan_") {
            return Ok(json!({
                "ok": false,
                "error": format!("Tool '{name}' cannot be called from inside eval.")
            }));
        }
        if hook_halt.is_some() {
            return Ok(json!({
                "ok": false,
                "error": "Not executed: previous tool hook halted the batch."
            }));
        }
        let mut arguments = serde_json::to_string(args).context("serializing eval bridge args")?;
        let mut hook_ask: Option<String> = None;

        if let Some(hook) = hook
            && let Some(pre) =
                hook.run_pre_nested_tool(name, call_id, &arguments, cancellation, emit)?
        {
            if let Some(reason) = pre.stop_reason {
                *hook_halt = Some(HookHalt::Stop(reason.clone()));
                return Ok(json!({
                    "ok": false,
                    "error": format!("Not executed: {reason}")
                }));
            }
            if let Some(reason) = pre.deny_reason {
                return Ok(json!({
                    "ok": false,
                    "error": format!("Not executed: {reason}")
                }));
            }
            hook_context.extend(pre.additional_context);
            hook_ask = pre.ask_reason;
            if let Some(updated) = pre.updated_arguments {
                arguments = updated;
            }
        }

        let mut nested_emit = |event: AgentEvent| {
            // 仅向宿主透传文件修改与诊断等真实副作用事件，避免内部子调用覆盖 eval 卡片状态。
            if matches!(
                event,
                AgentEvent::FileChanged { .. } | AgentEvent::LspDiagnostics { .. }
            ) {
                emit(event);
            }
        };
        let mut nested_call = ExecuteCall {
            name,
            arguments: &arguments,
            call_id,
            policy,
            cancellation,
            emit: &mut nested_emit,
            fetch,
            nested_hook_context: Vec::new(),
            nested_hook_halt: None,
        };

        let mut nested_images = Vec::new();
        let (mut value, cancelled) = match self.execute_inner(
            &mut nested_call,
            None,
            hook_ask.as_deref(),
            &mut nested_images,
        ) {
            Ok((value, _plan, todo)) => {
                images.extend(nested_images);
                if let Some(todo) = todo {
                    *todo_change = Some(todo);
                }
                (value, false)
            }
            Err(error) => {
                if error.chain().any(|cause| cause.is::<CleanupError>()) {
                    return Err(error);
                }
                let is_cancelled = error.chain().any(|cause| cause.is::<Cancelled>());
                let err_val = if is_cancelled {
                    json!({
                        "ok": false,
                        "cancelled": true,
                        "error": "Tool cancelled; side effects may have occurred."
                    })
                } else {
                    json!({
                        "ok": false,
                        "error": format!("{error:#}")
                    })
                };
                (err_val, is_cancelled)
            }
        };

        if let Some(hook) = hook
            && !cancelled
        {
            let is_error = value.get("ok").and_then(Value::as_bool) != Some(true);
            match hook.run_post_tool(
                crate::PostToolCall {
                    is_error,
                    name,
                    call_id,
                    arguments: &arguments,
                    value: &value,
                },
                cancellation,
                emit,
            ) {
                Ok(Some(post)) => {
                    hook_context.extend(post.additional_context);
                    if let Some(updated) = post.updated_output {
                        value = updated;
                    }
                    if let Some(reason) = post.stop_reason
                        && hook_halt.is_none()
                    {
                        *hook_halt = Some(HookHalt::Stop(reason));
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if error.chain().any(|cause| cause.is::<CleanupError>())
                        || error.chain().any(|cause| cause.is::<Cancelled>())
                    {
                        return Err(error);
                    }
                    if hook_halt.is_none() {
                        *hook_halt = Some(HookHalt::Failure(error));
                    }
                }
            }
        }

        Ok(value)
    }
}
