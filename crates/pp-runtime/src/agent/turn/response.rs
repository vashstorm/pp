//! Responses 协议的模型／工具轮次。
use super::super::hooks::{BatchCall, HookHalt, HookSession, PreparedBatch, prepare_batch};
use super::batch::{batch_failure, not_executed};
use super::plan_progress::{apply_plan_change, plan_file_touched, tracked_plan_path};
use super::request::{collect_thinking_event, record_thinking, record_usage};
use super::*;

pub(in crate::agent) fn response_turn(
    resources: TurnResources<'_>,
    request: &mut CreateResponse,
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
        protocol = "response"
    );
    let request_started = std::time::Instant::now();
    tools.project_request_sent();
    let mut turn_thinking = String::new();
    let mut turn_summary = String::new();
    let turn = {
        let journal = RefCell::new(&mut *journal);
        let emit = RefCell::new(&mut *emit);
        let thinking_buf = RefCell::new(&mut turn_thinking);
        let summary_buf = RefCell::new(&mut turn_summary);
        let mut sent = || {
            let mut journal = journal.borrow_mut();
            journal.sent(
                metadata,
                SessionRef::Response(request),
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
            if let Some(id) = journal.id() {
                update.record_prompt(SessionRef::Response(request), metadata, id);
            }
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
        let outcome = client.respond(request, cancellation, &mut sent, &mut progress);
        journal
            .borrow_mut()
            .track_stats(client.take_stats_key(), outcome.is_ok());
        log_model_request_finished("response", request_started, &outcome);
        outcome?
    };
    cancellation::check(cancellation)?;
    let awaiting_user_answer = todo::is_awaiting_user_answer(&turn.text);
    let text = turn.text;
    let event = AgentEvent::AssistantFinished { text: text.clone() };
    journal.event(&event);
    emit(event);
    // 先按短借用追加本轮输出，参数改写后再拿长借用执行。
    let start = response_input(request).len();
    let appended: Vec<InputItem> = turn.output.into_iter().map(InputItem::from).collect();
    response_input(request).extend(appended);
    if !turn.has_calls {
        record_usage(last_request, session_output_tokens, turn.usage);
        record_thinking(thinking_history, turn_thinking, turn_summary);
        return Ok(TurnOutcome::Completed {
            awaiting_user_answer,
            text,
        });
    }
    let end = response_input(request).len();
    // Responses 在本轮调用段确定后、执行前准备参数；改写结果写回原生调用对象。
    // 任何准备失败（含取消）都要回到 `start`：残留的 function_call 没有对应结果，
    // 下一次任务的原生历史会被供应商拒绝。
    let hook_prep = (|| -> Result<(Option<HookSession>, Option<PreparedBatch>)> {
        let mut hook_session = None;
        let mut prepared = None;
        if hooks.tool_events() {
            let session = HookSession::prepare(
                hooks.clone(),
                journal,
                metadata,
                SessionRef::Response(request),
                workflow,
                tools.cwd(),
            )?;
            let batch: Vec<BatchCall<'_>> = response_input(request)[start..end]
                .iter()
                .filter_map(|item| match item {
                    InputItem::Item(Item::FunctionCall(call)) => Some(BatchCall {
                        name: call.name.as_str(),
                        call_id: call.call_id.as_str(),
                        arguments: call.arguments.as_str(),
                    }),
                    _ => None,
                })
                .collect();
            prepared = Some(prepare_batch(&session, &batch, cancellation, emit)?);
            drop(batch);
            if let Some(prepared) = &prepared {
                let mut updated = prepared.calls.iter();
                for item in response_input(request)[start..end].iter_mut() {
                    let InputItem::Item(Item::FunctionCall(call)) = item else {
                        continue;
                    };
                    let Some(prepared) = updated.next() else {
                        break;
                    };
                    if let Some(arguments) = &prepared.updated_arguments {
                        call.arguments.clone_from(arguments);
                    }
                }
            }
            hook_session = Some(session);
        }
        Ok((hook_session, prepared))
    })();
    let (hook_session, mut prepared) = match hook_prep {
        Ok(prepared) => prepared,
        Err(error) => {
            response_input(request).truncate(start);
            return Err(error);
        }
    };
    tools.prepare_instruction_batch(
        response_input(request)[start..end]
            .iter()
            .filter_map(|item| match item {
                InputItem::Item(Item::FunctionCall(call)) => {
                    Some((call.name.as_str(), call.arguments.as_str()))
                }
                _ => None,
            }),
        workflow.mode,
    );
    let input = response_input(request);
    let mut stop_error = None;
    let mut sealed = false;
    let mut hook_context: Vec<String> = Vec::new();
    let mut hook_halt = prepared.as_mut().and_then(|batch| batch.halt.take());
    let mut call_index = 0usize;
    let batch_result = (|| -> Result<()> {
        for index in start..end {
            let InputItem::Item(Item::FunctionCall(call)) = &input[index] else {
                continue;
            };
            let position = call_index;
            call_index += 1;
            if let Some(halt) = &hook_halt {
                let serialized = not_executed(
                    journal,
                    &mut *emit,
                    &call.call_id,
                    &call.name,
                    &call.arguments,
                    halt,
                );
                input.push(InputItem::Item(Item::FunctionCallOutput(
                    FunctionCallOutputItemParam {
                        call_id: Some(call.call_id.clone()),
                        output: serialized.into(),
                        id: None,
                        status: None,
                        name: None,
                        namespace: None,
                        caller: None,
                    },
                )));
                continue;
            }
            if !sealed && tools.instruction_batch_blocked().is_none() && call.name == "todo" {
                plan_sync::sync_plan_todos(tools, journal, workflow, emit, false)?;
                *plan_dirty = false;
            }
            let output = {
                let progress_plan_path = tracked_plan_path(workflow);
                let mut progress = |event| {
                    if plan_file_touched(tools, &progress_plan_path, &event) {
                        *plan_dirty = true;
                    }
                    journal.event(&event);
                    emit(event);
                };
                let hook = hook_session.as_ref().and_then(|session| {
                    let prepared = prepared.as_ref()?.calls.get(position)?;
                    Some(crate::agent::hooks::ToolHookCall {
                        session,
                        call: prepared,
                    })
                });
                tools.execute_with_web(
                    &call.name,
                    &call.arguments,
                    &call.call_id,
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
            journal.record_tool_output(&call.call_id, &output.output);
            reminder.awaiting_progress = false;
            // 前置 hook 的上下文已在工具执行器里随 `hook_context` 返回，不重复追加。
            hook_context.extend(output.hook_context);
            if hook_halt.is_none() {
                hook_halt = output.hook_halt;
            }
            if stop_error.is_none() {
                stop_error = output.stop_error;
            }
            let call_id = Some(call.call_id.clone());
            input.push(InputItem::Item(Item::FunctionCallOutput(
                FunctionCallOutputItemParam {
                    call_id,
                    output: tool_images::response_output(output.output, output.images),
                    id: None,
                    status: None,
                    name: None,
                    namespace: None,
                    caller: None,
                },
            )));
            apply_plan_change(workflow, journal, &mut sealed, output.plan_change, emit)?;
            let changed_checks = output
                .todo_change
                .as_ref()
                .and_then(|change| change.plan.as_ref())
                .is_some_and(|change| change.content.is_some());
            let todo_changed = output.todo_change.is_some();
            apply_todo_change(workflow, journal, output.todo_change)?;
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
        response_input(request).truncate(start);
        return Err(error);
    }
    if !hook_context.is_empty() {
        response_input(request).push(InputItem::EasyMessage(EasyInputMessage {
            role: Role::Developer,
            content: hook_context.join("\n\n").into(),
            ..Default::default()
        }));
    }
    record_usage(last_request, session_output_tokens, turn.usage);
    record_thinking(thinking_history, turn_thinking, turn_summary);
    let hook_error = hook_halt.map(|halt| match halt {
        HookHalt::Stop(reason) => anyhow::anyhow!("{reason}"),
        HookHalt::Failure(error) => error,
    });
    if let Some(error) = batch_failure(hook_error, stop_error) {
        journal.commit(SessionRef::Response(request))?;
        return Err(error);
    }
    Ok(if sealed {
        TurnOutcome::PlanSubmitted
    } else {
        TurnOutcome::Continue
    })
}
