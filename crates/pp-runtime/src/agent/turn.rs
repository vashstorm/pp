use super::hooks::{BatchCall, HookHalt, HookScope, HookSession, PreparedBatch, prepare_batch};
use super::*;

pub(super) type ReportedProjectLoads = std::collections::BTreeMap<String, String>;

pub(super) fn reported_project_load(replay: &[persistence::ReplayRecord]) -> ReportedProjectLoads {
    replay
        .iter()
        .filter_map(|record| match record {
            persistence::ReplayRecord::Event(AgentEvent::ProjectInstructionsLoaded {
                path,
                content,
            }) => Some((path.clone(), content.clone())),
            _ => None,
        })
        .collect()
}

pub(super) fn publish_project_load(
    pending: &mut Vec<crate::project_instructions::Document>,
    reported: &mut ReportedProjectLoads,
    journal: &mut SessionJournal,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<()> {
    while let Some(document) = pending.first() {
        let path = document.path.display().to_string();
        let event = AgentEvent::ProjectInstructionsLoaded {
            path: path.clone(),
            content: document.content.clone(),
        };
        journal.event(&event);
        journal.check()?;
        emit(event);
        reported.insert(path, pending.remove(0).content);
    }
    Ok(())
}

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

pub(super) fn log_task_finished(
    operation: &str,
    started: std::time::Instant,
    result: &Result<RunOutcome>,
) {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let pid = std::process::id();
    match result {
        Ok(RunOutcome::Completed) => tracing::info!(
            event = "task_finished",
            pid,
            operation,
            status = "ok",
            elapsed_ms
        ),
        Ok(RunOutcome::AwaitingPlanReview) => tracing::info!(
            event = "task_finished",
            pid,
            operation,
            status = "awaiting_plan_review",
            elapsed_ms
        ),
        Ok(RunOutcome::Cancelled) => tracing::info!(
            event = "task_finished",
            pid,
            operation,
            status = "cancelled",
            elapsed_ms
        ),
        Err(error) => tracing::error!(
            event = "task_finished",
            pid,
            operation,
            error = pp_core::logging::diagnostic_text(&format!("{error:#}")),
            status = "error",
            elapsed_ms
        ),
    }
}

pub(super) fn log_model_request_finished<T>(
    protocol: &str,
    started: std::time::Instant,
    outcome: &anyhow::Result<T>,
) {
    let elapsed_ms = started.elapsed().as_millis() as u64;
    let pid = std::process::id();
    match outcome {
        Ok(_) => tracing::info!(
            event = "model_request_finished",
            pid,
            protocol,
            status = "ok",
            elapsed_ms
        ),
        Err(error) if error.is::<Cancelled>() => tracing::info!(
            event = "model_request_finished",
            pid,
            protocol,
            status = "cancelled",
            elapsed_ms
        ),
        Err(error) => tracing::error!(
            event = "model_request_finished",
            pid,
            protocol,
            error = pp_core::logging::diagnostic_text(&format!("{error:#}")),
            status = "error",
            elapsed_ms
        ),
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

/// 未执行的调用仍然要产生一条 `ok:false` 结果与成对的工具事件；
/// 合成事件同样进入展示捕获边界，使恢复与会话树能看到这些卡片。
/// 取消导致的跳过保留 `cancelled` 标记，这样工具统计与卡片按取消归类，
/// 不会因为 `HookHalt::Failure` 而变成错误。
fn not_executed(
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
fn batch_failure(
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

fn push_developer(request: &mut CreateChatCompletionRequest, text: String) {
    request
        .messages
        .push(ChatCompletionRequestMessage::Developer(
            async_openai::types::chat::ChatCompletionRequestDeveloperMessage {
                content: text.into(),
                ..Default::default()
            },
        ));
}

/// 记录一次普通请求的用量：`last_request` 只保存最近一次，会话累计只增不减。
fn record_usage(
    last_request: &mut Option<TokenUsage>,
    session_output_tokens: &mut u64,
    usage: Option<TokenUsage>,
) {
    *last_request = usage;
    if let Some(usage) = usage {
        *session_output_tokens = session_output_tokens.saturating_add(usage.output_tokens);
    }
}

/// Plan file tracked during execution: only an Execute-mode run with an
/// Executed plan watches `FileChanged` events for plan-progress refreshes.
/// The actual short path is cached so the progress callback stays independent
/// of the workflow borrow without cloning the whole document.
fn tracked_plan_path(workflow: &pp_core::plan::WorkflowState) -> Option<String> {
    if workflow.mode == pp_core::plan::WorkMode::Execute {
        workflow
            .plan
            .as_ref()
            .filter(|plan| plan.status == pp_core::plan::PlanStatus::Executed)
            .and_then(|plan| plan.path())
    } else {
        None
    }
}

/// Whether one progress event touches the tracked plan file.
fn plan_file_touched(tools: &Tools, plan_path: &Option<String>, event: &AgentEvent) -> bool {
    match (event, plan_path) {
        (AgentEvent::FileChanged { path, .. }, Some(expected_path)) => {
            tools.is_plan_path(path, expected_path)
        }
        _ => false,
    }
}

fn apply_plan_change(
    workflow: &mut pp_core::plan::WorkflowState,
    journal: &mut SessionJournal,
    sealed: &mut bool,
    change: Option<crate::tools::PlanChange>,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<()> {
    let Some(change) = change else { return Ok(()) };
    ensure!(
        workflow.mode == pp_core::plan::WorkMode::Plan && !*sealed,
        "Invalid plan tool transition."
    );
    if matches!(change, crate::tools::PlanChange::Submitted) {
        *sealed = true;
        return Ok(());
    }
    let previous = workflow.clone();
    let plan = workflow
        .plan
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("No current plan."))?;
    if let crate::tools::PlanChange::Written {
        title,
        file_name,
        content,
    } = change
    {
        plan.revision = plan.next_revision()?;
        plan.title = title;
        plan.file_name = Some(file_name);
        plan.content = content;
        plan.status = pp_core::plan::PlanStatus::Draft;
    }
    workflow.todos.clear();
    workflow.todo_plan_id = None;
    if let Err(error) = journal.write_workflow(workflow) {
        *workflow = previous;
        return Err(error.context("Plan file saved but session synchronization failed."));
    }
    publish_plan_progress(workflow, emit);
    Ok(())
}

pub(super) fn todo_snapshot(workflow: &pp_core::plan::WorkflowState) -> Option<AgentEvent> {
    if workflow.mode == pp_core::plan::WorkMode::Plan {
        return None;
    }
    let plan = workflow.plan.as_ref()?;
    (workflow.todo_plan_id.as_deref() == Some(plan.id.as_str())).then(|| AgentEvent::TodoSnapshot {
        plan_id: plan.id.clone(),
        revision: plan.revision,
        phases: workflow.todos.clone(),
        status: plan.status,
    })
}

pub(super) fn publish_todo_snapshot(
    workflow: &pp_core::plan::WorkflowState,
    journal: &mut SessionJournal,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<()> {
    if let Some(event) = todo_snapshot(workflow) {
        journal.event(&event);
        journal.check()?;
        emit(event);
        publish_todo_state(workflow, emit);
    }
    Ok(())
}

pub(super) fn publish_todo_state(
    workflow: &pp_core::plan::WorkflowState,
    emit: &mut dyn FnMut(AgentEvent),
) {
    emit(AgentEvent::TodoStateUpdated {
        state: pp_core::events::TodoState::from_workflow(workflow),
    });
}

pub(super) fn publish_plan_progress(
    workflow: &pp_core::plan::WorkflowState,
    emit: &mut dyn FnMut(AgentEvent),
) {
    if let Some(document) = &workflow.plan {
        emit(AgentEvent::PlanProgressUpdated {
            plan_id: document.id.clone(),
            revision: document.revision,
            content: document.content.clone(),
            file_name: document.file_name.clone(),
            location: Some(document.location),
            status: Some(document.status),
        });
    }
}

pub(super) fn apply_todo_change(
    workflow: &mut pp_core::plan::WorkflowState,
    journal: &mut SessionJournal,
    change: Option<crate::tools::todo::TodoChange>,
) -> Result<()> {
    let Some(change) = change else { return Ok(()) };
    if let Some(plan_change) = &change.plan {
        let document = workflow
            .plan
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("No current plan."))?;
        ensure!(
            document.id == plan_change.plan_id
                && workflow.todo_plan_id.as_deref() == Some(document.id.as_str())
                && plan_change.revision
                    == if plan_change.content.is_some() {
                        document.next_revision()?
                    } else {
                        document.revision
                    },
            "Invalid plan todo transition."
        );
    }
    let source = change.plan.as_ref().map(|plan| plan.plan_id.clone());
    let previous_todos = std::mem::replace(&mut workflow.todos, change.phases);
    let previous_source = std::mem::replace(&mut workflow.todo_plan_id, source);
    let previous_document = change.plan.and_then(|change| {
        change.content.map(|content| {
            let document = workflow.plan.as_mut().expect("plan effect validated");
            (
                std::mem::replace(&mut document.content, content),
                std::mem::replace(&mut document.revision, change.revision),
            )
        })
    });
    let saved_progress = previous_document.is_some();
    if let Err(error) = journal.write_workflow(workflow) {
        workflow.todos = previous_todos;
        workflow.todo_plan_id = previous_source;
        if let Some((content, revision)) = previous_document {
            let document = workflow.plan.as_mut().expect("plan effect validated");
            document.content = content;
            document.revision = revision;
        }
        return Err(if saved_progress {
            error.context("Plan progress saved but session synchronization failed; the plan file was not rolled back.")
        } else {
            error
        });
    }
    Ok(())
}

pub(super) fn completion_turn(
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
    let response = {
        let journal = RefCell::new(&mut *journal);
        let emit = RefCell::new(&mut *emit);
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
                .is_none_or(|binding| binding.depth == 0)
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
            journal.borrow_mut().event(&event);
            (*emit.borrow_mut())(event);
        };
        client.set_stats_context(journal.borrow().stats_context(
            metadata,
            if tools.subagent.as_ref().is_some_and(|b| b.depth > 0) {
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
                tools.execute_with_fetch(
                    &call.function.name,
                    &call.function.arguments,
                    &call.id,
                    tool_policy(workflow, sealed, queue)?,
                    hook,
                    cancellation,
                    &mut progress,
                    &mut |url, token| client.fetch_read_url(url, token),
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

pub(super) fn response_turn(
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
    let turn = {
        let journal = RefCell::new(&mut *journal);
        let emit = RefCell::new(&mut *emit);
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
                .is_none_or(|binding| binding.depth == 0)
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
                SessionRef::Response(request),
                metadata,
                journal.id().expect("sent request has a session ID"),
            );
            journal.event(&AgentEvent::AssistantStarted);
            journal.check()
        };
        let mut progress = |event| {
            journal.borrow_mut().event(&event);
            (*emit.borrow_mut())(event);
        };
        client.set_stats_context(journal.borrow().stats_context(
            metadata,
            if tools.subagent.as_ref().is_some_and(|b| b.depth > 0) {
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
                tools.execute_with_fetch(
                    &call.name,
                    &call.arguments,
                    &call.call_id,
                    tool_policy(workflow, sealed, queue)?,
                    hook,
                    cancellation,
                    &mut progress,
                    &mut |url, token| client.fetch_read_url(url, token),
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

/// Append a user message to either protocol's native history; shared by the
/// agent loop and the fresh-context execution candidate session.
pub(super) fn append_input_to(session: &mut Session, input: &crate::user_input::UserInput) {
    use crate::user_input::Part;
    use async_openai::types::{chat as c, responses as r};
    if !input.has_images() {
        let text = input
            .parts
            .iter()
            .filter_map(|p| {
                if let Part::Text(t) = p {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect::<String>();
        append_user_to(session, &text);
        return;
    }
    match session {
        Session::Completion(request) => request.messages.push(
            c::ChatCompletionRequestMessage::User(c::ChatCompletionRequestUserMessage {
                content: c::ChatCompletionRequestUserMessageContent::Array(
                    crate::user_input::chat_parts(&input.parts),
                ),
                ..Default::default()
            }),
        ),
        Session::Response(request) => {
            response_input(request).push(r::InputItem::EasyMessage(r::EasyInputMessage {
                role: r::Role::User,
                content: r::EasyInputContent::ContentList(crate::user_input::response_parts(
                    &input.parts,
                )),
                ..Default::default()
            }))
        }
    }
}

pub(super) fn append_user_to(session: &mut Session, prompt: &str) {
    match session {
        Session::Completion(request) => request.messages.push(ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: prompt.into(),
                ..Default::default()
            },
        )),
        Session::Response(request) => {
            response_input(request).push(InputItem::EasyMessage(EasyInputMessage {
                role: Role::User,
                content: prompt.into(),
                ..Default::default()
            }))
        }
    }
}

pub(super) fn append_developer_to(session: &mut Session, text: String) {
    match session {
        Session::Completion(request) => {
            request
                .messages
                .push(ChatCompletionRequestMessage::Developer(
                    async_openai::types::chat::ChatCompletionRequestDeveloperMessage {
                        content: text.into(),
                        ..Default::default()
                    },
                ))
        }
        Session::Response(request) => {
            response_input(request).push(InputItem::EasyMessage(EasyInputMessage {
                role: Role::Developer,
                content: text.into(),
                ..Default::default()
            }))
        }
    }
}

pub(super) fn response_input(request: &mut CreateResponse) -> &mut Vec<InputItem> {
    let InputParam::Items(input) = &mut request.input else {
        unreachable!("agent always uses an item array for Responses history");
    };
    input
}
