//! Chat Completions 协议的模型／工具轮次。
use super::super::hooks::{BatchCall, HookHalt, HookSession, PreparedBatch, prepare_batch};
use super::batch::{batch_failure, not_executed};
use super::plan_progress::{apply_plan_change, plan_file_touched, tracked_plan_path};
use super::request::{collect_thinking_event, push_developer, record_thinking, record_usage};
use super::*;

pub(in crate::agent) fn completion_turn(
    resources: TurnResources<'_>,
    request: &mut CreateChatCompletionRequest,
    metadata: &Metadata<'_>,
    mut update: RequestUpdate<'_>,
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<TurnOutcome> {
    let TurnResources {
        client,
        skills,
        last_request,
        session_output_tokens,
        thinking_history,
        tools,
        journal,
        workflow,
        input: queue,
        plan_dirty,
        reminder,
        hooks,
    } = resources;
    tracing::info!(
        event = "model_request_started",
        pid = std::process::id(),
        protocol = "completion"
    );
    let request_started = std::time::Instant::now();
    tools.project_request_sent();
    let mut turn_thinking = String::new();
    let mut turn_summary = String::new();
    let response = {
        let journal = RefCell::new(&mut *journal);
        let emit = RefCell::new(&mut *emit);
        let thinking_buf = RefCell::new(&mut turn_thinking);
        let summary_buf = RefCell::new(&mut turn_summary);
        let mut sent = || {
            let mut journal = journal.borrow_mut();
            journal.sent(
                metadata,
                SessionRef::Completion(request),
                workflow,
                update.user,
                update.instructions,
            )?;
            if let (Some(binding), Some(id)) = (&tools.subagent, journal.id()) {
                binding.bind_session(journal.directory(), id);
            }
            if tools
                .subagent
                .as_ref()
                .is_none_or(|binding| binding.is_root())
                && let Some(id) = journal.id()
            {
                let _ = tools.artifacts.bind(journal.directory(), id);
            }
            publish_project_load(
                update.pending_project_load,
                update.reported_project_load,
                &mut journal,
                &mut **emit.borrow_mut(),
            )?;
            update.record_prompt(
                SessionRef::Completion(request),
                metadata,
                journal.id().expect("sent request has a session ID"),
            );
            journal.event(&AgentEvent::AssistantStarted);
            journal.check()
        };
        let mut progress = |event| {
            collect_thinking_event(
                &event,
                &mut thinking_buf.borrow_mut(),
                &mut summary_buf.borrow_mut(),
            );
            journal.borrow_mut().event(&event);
            (*emit.borrow_mut())(event);
        };
        client.set_stats_context(journal.borrow().stats_context(
            metadata,
            if tools.subagent.as_ref().is_some_and(|b| !b.is_root()) {
                "subagent"
            } else {
                "task"
            },
            skills,
        ));
        let outcome = client.complete(request, cancellation, &mut sent, &mut progress);
        journal
            .borrow_mut()
            .track_stats(client.take_stats_key(), outcome.is_ok());
        log_model_request_finished("completion", request_started, &outcome);
        outcome?
    };
    cancellation::check(cancellation)?;
    let usage = response.usage;
    let mut response = response.message;
    let text = match &response.content {
        Some(ChatCompletionRequestAssistantMessageContent::Text(text)) => text.clone(),
        _ => String::new(),
    };
    let awaiting_user_answer = todo::is_awaiting_user_answer(&text);
    let event = AgentEvent::AssistantFinished { text: text.clone() };
    journal.event(&event);
    emit(event);
    let calls = response.tool_calls.as_deref().unwrap_or_default();
    if calls.is_empty() {
        request
            .messages
            .push(ChatCompletionRequestMessage::Assistant(response));
        record_usage(last_request, session_output_tokens, usage);
        record_thinking(thinking_history, turn_thinking, turn_summary);
        return Ok(TurnOutcome::Completed {
            awaiting_user_answer,
            text,
        });
    }
    // 整批 PreToolUse 先于项目指令预检与任何执行；参数改写要写回原生调用，
    // 这样预检、审批、ToolStarted 与已提交历史看到同一份有效参数。
    let mut hook_session = None;
    let mut prepared: Option<PreparedBatch> = None;
    if hooks.tool_events() {
        let session = HookSession::prepare(
            hooks.clone(),
            journal,
            metadata,
            SessionRef::Completion(request),
            workflow,
            tools.cwd(),
        )?;
        let batch: Vec<BatchCall<'_>> = response
            .tool_calls
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|call| match call {
                ChatCompletionMessageToolCalls::Function(call) => Some(BatchCall {
                    name: call.function.name.as_str(),
                    call_id: call.id.as_str(),
                    arguments: call.function.arguments.as_str(),
                }),
                _ => None,
            })
            .collect();
        prepared = Some(prepare_batch(&session, &batch, cancellation, emit)?);
        drop(batch);
        if let (Some(calls), Some(prepared)) = (response.tool_calls.as_mut(), &prepared) {
            for (call, prepared) in calls.iter_mut().zip(&prepared.calls) {
                if let ChatCompletionMessageToolCalls::Function(call) = call
                    && let Some(arguments) = &prepared.updated_arguments
                {
                    call.function.arguments.clone_from(arguments);
                }
            }
        }
        hook_session = Some(session);
    }
    let calls = response.tool_calls.as_deref().unwrap_or_default();
    tools.prepare_instruction_batch(
        calls.iter().filter_map(|call| match call {
            ChatCompletionMessageToolCalls::Function(call) => Some((
                call.function.name.as_str(),
                call.function.arguments.as_str(),
            )),
            _ => None,
        }),
        workflow.mode,
    );
    let count = calls.len();
    let assistant_index = request.messages.len();
    request
        .messages
        .push(ChatCompletionRequestMessage::Assistant(response));
    let mut stop_error = None;
    let mut sealed = false;
    let mut attachments = Vec::new();
    let mut hook_context: Vec<String> = Vec::new();
    let mut hook_halt = prepared.as_mut().and_then(|batch| batch.halt.take());
    let batch_result = (|| -> Result<()> {
        for index in 0..count {
            let ChatCompletionRequestMessage::Assistant(response) =
                &request.messages[assistant_index]
            else {
                bail!("agent invariant: missing assistant calls");
            };
            let Some(ChatCompletionMessageToolCalls::Function(call)) = response
                .tool_calls
                .as_ref()
                .and_then(|calls| calls.get(index))
            else {
                bail!("agent invariant: missing function call");
            };
            if let Some(halt) = &hook_halt {
                let serialized = not_executed(
                    journal,
                    &mut *emit,
                    &call.id,
                    &call.function.name,
                    &call.function.arguments,
                    halt,
                );
                request.messages.push(ChatCompletionRequestMessage::Tool(
                    ChatCompletionRequestToolMessage {
                        tool_call_id: call.id.clone(),
                        content: serialized.into(),
                    },
                ));
                continue;
            }
            if !sealed
                && tools.instruction_batch_blocked().is_none()
                && call.function.name == "todo"
            {
                plan_sync::sync_plan_todos(tools, journal, workflow, emit, false)?;
                *plan_dirty = false;
            }
            let content = {
                let progress_plan_path = tracked_plan_path(workflow);
                let mut progress = |event| {
                    if plan_file_touched(tools, &progress_plan_path, &event) {
                        *plan_dirty = true;
                    }
                    journal.event(&event);
                    emit(event);
                };
                let hook = hook_session.as_ref().and_then(|session| {
                    let prepared = prepared.as_ref()?.calls.get(index)?;
                    Some(crate::agent::hooks::ToolHookCall {
                        session,
                        call: prepared,
                    })
                });
                tools.execute_with_web(
                    &call.function.name,
                    &call.function.arguments,
                    &call.id,
                    tool_policy(workflow, sealed, queue)?,
                    hook.as_ref().map(|h| h as &dyn pp_tools::ToolHookCall),
                    cancellation,
                    &mut progress,
                    &mut |action, token| {
                        crate::agent::tool_access::execute_web_action(client, action, token)
                    },
                )
            };
            // 结果字符数取最终回填模型的文本：post hook 可能已改写正文。
            journal.record_tool_output(&call.id, &content.output);
            reminder.awaiting_progress = false;
            // 前置 hook 的上下文已在工具执行器里随 `hook_context` 返回，不重复追加。
            hook_context.extend(content.hook_context);
            if hook_halt.is_none() {
                hook_halt = content.hook_halt;
            }
            if stop_error.is_none() {
                stop_error = content.stop_error;
            }
            let tool_call_id = call.id.clone();
            attachments.push((tool_call_id.clone(), content.images));
            request.messages.push(ChatCompletionRequestMessage::Tool(
                ChatCompletionRequestToolMessage {
                    tool_call_id,
                    content: content.output.into(),
                },
            ));
            apply_plan_change(workflow, journal, &mut sealed, content.plan_change, emit)?;
            let changed_checks = content
                .todo_change
                .as_ref()
                .and_then(|change| change.plan.as_ref())
                .is_some_and(|change| change.content.is_some());
            let todo_changed = content.todo_change.is_some();
            apply_todo_change(workflow, journal, content.todo_change)?;
            if todo_changed {
                publish_todo_state(workflow, emit);
            }
            if changed_checks {
                publish_plan_progress(workflow, emit);
            }
            if !sealed && *plan_dirty {
                plan_sync::sync_plan_todos(tools, journal, workflow, emit, true)?;
                *plan_dirty = false;
            }
        }
        Ok(())
    })();
    if let Err(error) = batch_result {
        request.messages.truncate(assistant_index);
        return Err(error);
    }
    tool_images::append_chat(&mut request.messages, attachments);
    if !hook_context.is_empty() {
        push_developer(request, hook_context.join("\n\n"));
    }
    record_usage(last_request, session_output_tokens, usage);
    record_thinking(thinking_history, turn_thinking, turn_summary);
    let hook_error = hook_halt.map(|halt| match halt {
        HookHalt::Stop(reason) => anyhow::anyhow!("{reason}"),
        HookHalt::Failure(error) => error,
    });
    if let Some(error) = batch_failure(hook_error, stop_error) {
        journal.commit(SessionRef::Completion(request))?;
        return Err(error);
    }
    Ok(if sealed {
        TurnOutcome::PlanSubmitted
    } else {
        TurnOutcome::Continue
    })
}
