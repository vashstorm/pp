use super::*;
pub(super) fn reported_project_load(replay: &[persistence::ReplayRecord]) -> Option<String> {
    replay.iter().rev().find_map(|record| match record {
        persistence::ReplayRecord::Event(AgentEvent::ProjectInstructionsLoaded {
            content, ..
        }) => Some(content.clone()),
        _ => None,
    })
}

pub(super) fn publish_project_load(
    pending: &mut Option<String>,
    reported: &mut Option<String>,
    journal: &mut SessionJournal,
    cwd: &std::path::Path,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<()> {
    let Some(content) = pending.as_ref() else {
        return Ok(());
    };
    let event = AgentEvent::ProjectInstructionsLoaded {
        path: cwd.join("AGENTS.md").display().to_string(),
        content: content.clone(),
    };
    journal.event(&event);
    journal.check()?;
    emit(event);
    *reported = pending.take();
    Ok(())
}

pub(super) struct RequestUpdate<'a> {
    pub(super) user: Option<&'a str>,
    pub(super) instructions: Option<&'a str>,
    pub(super) debug: &'a mut Option<debug::DebugState>,
    pub(super) pending_project_load: &'a mut Option<String>,
    pub(super) reported_project_load: &'a mut Option<String>,
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
        Err(_) => tracing::error!(
            event = "task_finished",
            pid,
            operation,
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
        Err(_) => tracing::error!(
            event = "model_request_finished",
            pid,
            protocol,
            status = "error",
            elapsed_ms
        ),
    }
}

// 两种协议在一次模型／工具轮次中使用相同的同步依赖。
pub(super) struct TurnResources<'a> {
    pub(super) skills: &'a [crate::skills::SkillHeader],
    pub(super) client: &'a mut OpenAiClient,
    pub(super) last_request: &'a mut Option<TokenUsage>,
    pub(super) tools: &'a Tools,
    pub(super) journal: &'a mut SessionJournal,
    pub(super) workflow: &'a mut crate::plan::WorkflowState,
    pub(super) input: Option<&'a InputQueue>,
    pub(super) plan_dirty: &'a mut bool,
    pub(super) reminder: &'a mut todo::TodoReminderState,
}

fn tool_policy<'a>(
    workflow: &'a crate::plan::WorkflowState,
    sealed: bool,
    input: Option<&'a InputQueue>,
) -> Result<crate::tools::ToolPolicy<'a>> {
    Ok(match workflow.mode {
        crate::plan::WorkMode::Execute => crate::tools::ToolPolicy::Execute { workflow },
        crate::plan::WorkMode::Plan => crate::tools::ToolPolicy::Plan {
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
    Completed { awaiting_user_answer: bool },
    PlanSubmitted,
}

/// Plan file tracked during execution: only an Execute-mode run with an
/// Executed plan watches `FileChanged` events for plan-progress refreshes.
/// The actual short path is cached so the progress callback stays independent
/// of the workflow borrow without cloning the whole document.
fn tracked_plan_path(workflow: &crate::plan::WorkflowState) -> Option<String> {
    if workflow.mode == crate::plan::WorkMode::Execute {
        workflow
            .plan
            .as_ref()
            .filter(|plan| plan.status == crate::plan::PlanStatus::Executed)
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
    workflow: &mut crate::plan::WorkflowState,
    journal: &mut SessionJournal,
    sealed: &mut bool,
    change: Option<crate::tools::PlanChange>,
    emit: &mut dyn FnMut(AgentEvent),
) -> Result<()> {
    let Some(change) = change else { return Ok(()) };
    ensure!(
        workflow.mode == crate::plan::WorkMode::Plan && !*sealed,
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
        plan.status = crate::plan::PlanStatus::Draft;
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

pub(super) fn todo_snapshot(workflow: &crate::plan::WorkflowState) -> Option<AgentEvent> {
    if workflow.mode == crate::plan::WorkMode::Plan {
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
    workflow: &crate::plan::WorkflowState,
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
    workflow: &crate::plan::WorkflowState,
    emit: &mut dyn FnMut(AgentEvent),
) {
    emit(AgentEvent::TodoStateUpdated {
        state: crate::events::TodoState::from_workflow(workflow),
    });
}

pub(super) fn publish_plan_progress(
    workflow: &crate::plan::WorkflowState,
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
    workflow: &mut crate::plan::WorkflowState,
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
        tools,
        journal,
        workflow,
        input: queue,
        plan_dirty,
        reminder,
    } = resources;
    tracing::info!(
        event = "model_request_started",
        pid = std::process::id(),
        protocol = "completion"
    );
    let request_started = std::time::Instant::now();
    let response = {
        let _permit = tools
            .subagent
            .as_ref()
            .map(|b| b.permit(cancellation))
            .transpose()?;
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
            publish_project_load(
                update.pending_project_load,
                update.reported_project_load,
                &mut journal,
                metadata.cwd,
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
    let response = response.message;
    let text = match &response.content {
        Some(ChatCompletionRequestAssistantMessageContent::Text(text)) => text.clone(),
        _ => String::new(),
    };
    let awaiting_user_answer = todo::is_awaiting_user_answer(&text);
    let event = AgentEvent::AssistantFinished { text };
    journal.event(&event);
    emit(event);
    let calls = response.tool_calls.as_deref().unwrap_or_default();
    if calls.is_empty() {
        request
            .messages
            .push(ChatCompletionRequestMessage::Assistant(response));
        *last_request = usage;
        return Ok(TurnOutcome::Completed {
            awaiting_user_answer,
        });
    }
    let count = calls.len();
    let assistant_index = request.messages.len();
    request
        .messages
        .push(ChatCompletionRequestMessage::Assistant(response));
    let mut stop_error = None;
    let mut sealed = false;
    let mut attachments = Vec::new();
    for index in 0..count {
        let ChatCompletionRequestMessage::Assistant(response) = &request.messages[assistant_index]
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
        if !sealed && call.function.name == "todo" {
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
            tools.execute_with_fetch(
                &call.function.name,
                &call.function.arguments,
                &call.id,
                tool_policy(workflow, sealed, queue)?,
                cancellation,
                &mut progress,
                &mut |url, token| client.fetch_read_url(url, token),
            )
        };
        reminder.awaiting_progress = false;
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
    tool_images::append_chat(&mut request.messages, attachments);
    *last_request = usage;
    if let Some(error) = stop_error {
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
        tools,
        journal,
        workflow,
        input: queue,
        plan_dirty,
        reminder,
    } = resources;
    tracing::info!(
        event = "model_request_started",
        pid = std::process::id(),
        protocol = "response"
    );
    let request_started = std::time::Instant::now();
    let turn = {
        let _permit = tools
            .subagent
            .as_ref()
            .map(|b| b.permit(cancellation))
            .transpose()?;
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
            publish_project_load(
                update.pending_project_load,
                update.reported_project_load,
                &mut journal,
                metadata.cwd,
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
    let event = AgentEvent::AssistantFinished { text: turn.text };
    journal.event(&event);
    emit(event);
    let input = response_input(request);
    let start = input.len();
    input.extend(turn.output.into_iter().map(InputItem::from));
    if !turn.has_calls {
        *last_request = turn.usage;
        return Ok(TurnOutcome::Completed {
            awaiting_user_answer,
        });
    }
    let end = input.len();
    let mut stop_error = None;
    let mut sealed = false;
    for index in start..end {
        let InputItem::Item(Item::FunctionCall(call)) = &input[index] else {
            continue;
        };
        if !sealed && call.name == "todo" {
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
            tools.execute_with_fetch(
                &call.name,
                &call.arguments,
                &call.call_id,
                tool_policy(workflow, sealed, queue)?,
                cancellation,
                &mut progress,
                &mut |url, token| client.fetch_read_url(url, token),
            )
        };
        reminder.awaiting_progress = false;
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
    *last_request = turn.usage;
    if let Some(error) = stop_error {
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
