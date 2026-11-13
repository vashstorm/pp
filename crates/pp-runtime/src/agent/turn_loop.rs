//! 轮次循环与提交边界上的补充输入应用／归还。
use super::{
    Agent, Session, debug,
    input::{InputQueue, PendingInput},
    persistence::Metadata,
    plan_sync, todo,
    turn::{
        self, RequestUpdate, TurnOutcome, TurnResources, append_developer_to, completion_turn,
        response_input, response_turn,
    },
};
use anyhow::{Result, bail};
use async_openai::types::chat::ChatCompletionRequestMessage;
use async_openai::types::responses::{InputItem, Role};
use pp_core::cancellation::{self, CancellationToken, Cancelled, RunOutcome};
use pp_core::events::AgentEvent;
use std::collections::VecDeque;

impl Agent {
    pub(in crate::agent) fn run_loop(
        &mut self,
        mut user: Option<&str>,
        input: Option<&InputQueue>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        self.tools.yielded.borrow_mut().take();
        self.tools.start_wait_batch();
        let mut request_count = 0usize;
        let mut claim_boundary = true;
        let mut reminder = todo::TodoReminderState::default();
        loop {
            self.journal.check()?;
            self.ensure_subagents()?;
            self.receive_subagents()?;
            if cancellation.is_cancelled() {
                self.persist_pending_input(user.take())?;
                return Err(Cancelled.into());
            }
            if self
                .runtime_models
                .as_ref()
                .is_some_and(|binding| binding.pending())
            {
                self.persist_pending_input(user.take())?;
            }
            self.apply_runtime_models()?;
            plan_sync::sync_plan_todos(
                &self.tools,
                &mut self.journal,
                &mut self.workflow,
                emit,
                true,
            )?;
            self.refresh_instructions()?;
            self.receive_lsp_diagnostics(emit)?;
            // 补充边界：仅首次普通请求前，以及上一轮完整回复/整批工具结果成功提交之后。
            let claimed = match (input, claim_boundary) {
                (Some(queue), true) => queue.claim_steering(false)?,
                _ => VecDeque::new(),
            };
            claim_boundary = true;
            if !claimed.is_empty() {
                self.apply_pending(claimed, input, &mut user, cancellation, emit)?;
            }
            let usage = self.context_usage()?;
            emit(AgentEvent::ContextUpdated { usage });
            let should_compact = usage.auto_compact
                && usage.context_window.is_some_and(|window| {
                    u128::from(usage.estimated_tokens) * 100
                        >= u128::from(window) * u128::from(usage.threshold_percent)
                });
            if should_compact {
                // Save accepted input exactly once before cutoff, capacity, or
                // quota checks can fail. Never journal the temporary summary.
                self.persist_pending_input(user.take())?;
                self.compact_impl(true, cancellation, emit)?;
                cancellation::check(cancellation)?;
                // 摘要期间文件也可能被修改；重新加载、估算并检查阈值后再发普通请求。
                continue;
            }
            let metadata = Metadata {
                role_route: self.role_route.as_ref(),
                cwd: self.tools.cwd(),
                model_reference: &self.current_model.reference,
                think_level: self.think_level,
                edit_mode: self.tools.edit_mode(),
                tool_limits: self.tools.limits,
                base_system: &self.base_system,
                prompt_source: self.prompt_source.as_ref(),
            };
            if let Some(debug) = &mut self.debug
                && debug
                    .request(&self.session, &metadata, self.journal.id())
                    .is_err()
            {
                debug::DebugState::warn_unavailable();
                self.debug = None;
            }
            if let Some(binding) = &self.tools.subagent
                && binding.depth > 0
            {
                let budget = binding.request_budget();
                if budget > 0 && request_count >= budget.saturating_mul(3) / 2 {
                    bail!(
                        "subagent request budget exhausted; partial results are saved in history."
                    );
                }
                if budget > 0 && request_count == budget {
                    append_developer_to(&mut self.session,"Request budget reached. Finish your current work and return the partial findings now.".into());
                }
            }
            request_count += 1;
            emit(AgentEvent::AssistantStarted);
            let update = RequestUpdate {
                user: user.take(),
                instructions: self.pending_instructions.as_deref(),
                debug: &mut self.debug,
                pending_project_load: &mut self.pending_project_load,
                reported_project_load: &mut self.reported_project_load,
            };
            let mut plan_dirty = false;
            let resources = TurnResources {
                client: &mut self.client,
                skills: &self.request_skills,
                last_request: &mut self.last_request,
                session_output_tokens: &mut self.session_output_tokens,
                tools: &self.tools,
                journal: &mut self.journal,
                workflow: &mut self.workflow,
                input,
                plan_dirty: &mut plan_dirty,
                reminder: &mut reminder,
            };
            let finished = match &mut self.session {
                Session::Completion(request) => {
                    completion_turn(resources, request, &metadata, update, cancellation, emit)
                }
                Session::Response(request) => {
                    response_turn(resources, request, &metadata, update, cancellation, emit)
                }
            };
            let finished = match finished {
                Ok(finished) => finished,
                Err(error) if pp_llm::retry::model_request_failed(&error) => {
                    cancellation::check(cancellation)?;
                    self.pending_instructions = None;
                    if self.activate_fallback(emit)? {
                        claim_boundary = false;
                        request_count -= 1;
                        continue;
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            self.pending_instructions = None;
            self.journal.commit((&self.session).into())?;
            self.tools.commit_wait_results()?;
            self.apply_runtime_models()?;
            if plan_dirty {
                self.refresh_plan_progress(emit)?;
            }
            self.publish_context(emit)?;
            if self.tools.yielded.borrow().is_some() {
                if let Some(binding) = self.tools.subagent.clone()
                    && binding.pending()
                {
                    self.tools.yielded.borrow_mut().take();
                    self.wait_owned_subagents(cancellation)?;
                    append_developer_to(&mut self.session,"Owned background work completed after your submitted result. Review the new results and submit a fresh final yield.".into());
                    continue;
                }
                return Ok(RunOutcome::Completed);
            }
            if matches!(finished, TurnOutcome::PlanSubmitted) {
                self.workflow
                    .plan
                    .as_mut()
                    .expect("submitted plan exists")
                    .status = pp_core::plan::PlanStatus::Ready;
                if let Err(error) = self.journal.write_workflow(&self.workflow) {
                    self.workflow
                        .plan
                        .as_mut()
                        .expect("submitted plan exists")
                        .status = pp_core::plan::PlanStatus::Draft;
                    return Err(error);
                }
                turn::publish_todo_state(&self.workflow, emit);
                return Ok(RunOutcome::AwaitingPlanReview);
            }
            if let TurnOutcome::Completed {
                awaiting_user_answer,
            } = finished
            {
                plan_sync::sync_plan_todos(
                    &self.tools,
                    &mut self.journal,
                    &mut self.workflow,
                    emit,
                    true,
                )?;
                if let Some(binding) = self.tools.subagent.clone()
                    && binding.pending()
                {
                    self.wait_owned_subagents(cancellation)?;
                    continue;
                }
                if self.tools.yield_schema.is_some() && self.tools.yielded.borrow().is_none() {
                    bail!(
                        "structured subagent task did not call yield; result does not satisfy the completion protocol."
                    );
                }
                let remind = todo::eligible(&self.workflow, &reminder, awaiting_user_answer);
                // 原子“领取或关闭”：最终回复与新补充仅在这个短锁内仲裁。
                if let Some(queue) = input {
                    let batch = queue.claim_steering(!remind)?;
                    if !batch.is_empty() {
                        self.apply_pending(batch, input, &mut user, cancellation, emit)?;
                        // 本边界只消费固定批次；确认期间新到达的消息等下一边界。
                        claim_boundary = false;
                        continue;
                    }
                }
                if remind {
                    cancellation::check(cancellation)?;
                    let attempt = reminder.count + 1;
                    let text = todo::reminder_text(&self.workflow.todos, attempt);
                    append_developer_to(&mut self.session, text);
                    self.journal.commit((&self.session).into())?;
                    let event = AgentEvent::TodoReminder {
                        todos: todo::open_todos(&self.workflow.todos),
                        attempt,
                        max_attempts: todo::TODO_REMINDERS_MAX,
                    };
                    self.journal.event(&event);
                    self.journal.check()?;
                    reminder.count = attempt;
                    reminder.awaiting_progress = true;
                    emit(event);
                    continue;
                }
                return Ok(RunOutcome::Completed);
            }
            cancellation::check(cancellation)?;
        }
    }

    // 在提交边界应用一批补充：先记账尚未持久化的初始输入，再逐条 append、
    // 持久化并确认。已确认项永不归还；取消或持久化失败时归还未应用项——
    // 归还只在唯一出口执行，失败路径不必各自维护 push_front 配对。
    pub(in crate::agent) fn apply_pending(
        &mut self,
        mut batch: VecDeque<PendingInput>,
        input: Option<&InputQueue>,
        user: &mut Option<&str>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        let result = (|| -> Result<()> {
            if user.is_some()
                && let Err(error) = self.persist_pending_input(user.take())
            {
                return Err(error);
            }
            while !batch.is_empty() {
                if cancellation.is_cancelled() {
                    return Err(Cancelled.into());
                }
                let item = batch.front().expect("batch is non-empty");
                if let Some(prepared) = &item.prepared {
                    turn::append_input_to(&mut self.session, prepared);
                } else {
                    self.append_user(item.prompt());
                }
                if let Err(error) = self.persist_pending_input(Some(item.prompt())) {
                    self.remove_last_user();
                    return Err(error);
                }
                let item = batch.pop_front().expect("batch is non-empty");
                emit(AgentEvent::UserInputAccepted {
                    id: item.id,
                    text: item.into_display(),
                });
            }
            Ok(())
        })();
        if result.is_err() {
            self.restore_pending(input, batch);
        }
        result
    }

    fn restore_pending(&self, input: Option<&InputQueue>, batch: VecDeque<PendingInput>) {
        if let Some(queue) = input
            && let Err(error) = queue.restore_front(batch)
        {
            tracing::error!(event = "queue_restore_failed", error = format!("{error:#}"));
        }
    }

    // 回退尚未持久化的内存 user append；只移除确认属于用户身份的末条消息。
    fn remove_last_user(&mut self) {
        match &mut self.session {
            Session::Completion(request) => {
                if matches!(
                    request.messages.last(),
                    Some(ChatCompletionRequestMessage::User(_))
                ) {
                    request.messages.pop();
                }
            }
            Session::Response(request) => {
                let input = response_input(request);
                if matches!(
                    input.last(),
                    Some(InputItem::EasyMessage(message)) if matches!(message.role, Role::User)
                ) {
                    input.pop();
                }
            }
        }
    }
}
