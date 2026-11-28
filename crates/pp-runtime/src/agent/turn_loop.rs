//! Supplementary input application/return at the turn loop and commit boundaries.
use super::{
    Agent, Session, debug,
    input::{InputQueue, PendingInput},
    persistence::Metadata,
    plan_sync, todo,
    turn::{
        self, RequestUpdate, TurnOutcome, TurnResources, append_developer_to, completion_turn,
        response_input, response_turn,
    },
    usage::SessionEstimate,
};
use anyhow::{Result, bail};
use pp_core::cancellation::{self, CancellationToken, Cancelled, RunOutcome};
use pp_core::events::AgentEvent;
use std::collections::VecDeque;

/// At most 8 hook continuations per `run_loop`; tool calls and steering do not reset the count.
const STOP_HOOK_LIMIT: u8 = 8;

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
        let mut subagent_tokens = 0u64;
        let mut subagent_cost_usd = 0.0f64;
        let mut resource_budget_warned = false;
        let mut claim_boundary = true;
        let mut reminder = todo::TodoReminderState::default();
        // Only the `hooks` field is borrowed; identity and eligibility are fixed on loop entry.
        let hooks = super::hooks::scope(
            &self.hooks,
            self.subagent.as_ref(),
            &self.workflow,
            &self.tools.permission,
            self.hook_agent_type.as_deref(),
        );
        let mut stop_hook_active = false;
        let mut stop_continues = 0u8;
        let mut pruned_since_progress = false;
        let mut overflow_stage = 0u8;
        let mut cached_usage: Option<pp_core::context::ContextUsage> = None;
        // Incremental usage accounting for this task run: only new messages are serialized, and wholesale history replacement invalidates it explicitly.
        let mut estimate = SessionEstimate::default();
        if let Some(schema) = &mut self.output_schema {
            schema.reset_retries();
            append_developer_to(&mut self.session, schema.instruction_message());
        }
        loop {
            self.journal.check()?;
            self.ensure_subagents()?;
            if cancellation.is_cancelled() {
                self.persist_pending_input(user.take())?;
                return Err(Cancelled.into());
            }
            if self.receive_subagents(emit)? {
                cached_usage = None;
            }
            if self.runtime_model_pending() {
                self.persist_pending_input(user.take())?;
            }
            if self.apply_runtime_models_changed()? {
                cached_usage = None;
                // A model switch may rebuild history across protocols, so the old manual accumulation no longer matches the native request.
                estimate.invalidate();
            }
            plan_sync::sync_plan_todos(
                &self.tools,
                &mut self.journal,
                &mut self.workflow,
                emit,
                true,
            )?;
            if self.refresh_instructions_changed()? {
                cached_usage = None;
                // The system prompt rewrites the first message in place, so the byte count must be accumulated again.
                estimate.invalidate();
            }
            if self.receive_lsp_diagnostics(emit)? {
                cached_usage = None;
            }
            // Supplementary boundary: only before the first normal request, and after the previous turn's full reply/tool batch was committed successfully.
            let claimed = match (input, claim_boundary) {
                (Some(queue), true) => queue.claim_steering(false)?,
                _ => VecDeque::new(),
            };
            claim_boundary = true;
            if !claimed.is_empty() {
                cached_usage = None;
                self.apply_pending(claimed, input, &mut user, cancellation, emit)?;
            }
            let usage = match cached_usage.take() {
                Some(usage) => usage,
                None => self.context_usage_cached(&mut estimate)?,
            };
            emit(AgentEvent::ContextUpdated { usage });
            let effective_tokens = self.calibrated_context_tokens(usage.estimated_tokens);
            let should_compact = usage.auto_compact
                && usage.context_window.is_some_and(|window| {
                    u128::from(effective_tokens) * 100
                        >= u128::from(window) * u128::from(usage.threshold_percent)
                });
            let should_prune = !should_compact
                && !pruned_since_progress
                && self.auto_prune
                && usage.context_window.is_some_and(|window| {
                    u128::from(effective_tokens) * 100
                        >= u128::from(window) * u128::from(self.auto_prune_threshold_percent)
                });
            if should_prune {
                self.persist_pending_input(user.take())?;
                pruned_since_progress = true;
                if self.prune_history(false)?.is_some() {
                    estimate.invalidate();
                    cancellation::check(cancellation)?;
                    continue;
                }
            }
            if should_compact {
                // Save accepted input exactly once before cutoff, capacity, or
                // quota checks can fail. Never journal the temporary summary.
                self.persist_pending_input(user.take())?;
                self.compact_impl(true, cancellation, emit)?;
                // Compaction replaces the whole history, so the next estimate must recompute fully.
                estimate.invalidate();
                cancellation::check(cancellation)?;
                // Files may also change during summarization; reload, re-estimate and re-check thresholds before sending the normal request.
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
            // The pre-request usage was computed at this turn's entry, so context publishing reuses it and avoids a second full estimate.
            let mut published_usage = Some(usage);
            if let Some(binding) = &self.subagent
                && binding.depth > 0
            {
                let budget = binding.request_budget();
                if budget > 0 && request_count >= budget.saturating_mul(3) / 2 {
                    bail!(
                        "subagent request budget exhausted; partial results are saved in history."
                    );
                }
                if let Some(token_limit) = binding.token_budget()
                    && u128::from(subagent_tokens) * 2 >= u128::from(token_limit) * 3
                {
                    bail!(
                        "subagent token budget exhausted ({subagent_tokens} tokens); partial results are saved in history."
                    );
                }
                if let Some(cost_limit) = binding.cost_budget_usd()
                    && subagent_cost_usd >= cost_limit * 1.5
                {
                    bail!(
                        "subagent cost budget exhausted (${subagent_cost_usd:.4}); partial results are saved in history."
                    );
                }
                if budget > 0 && request_count == budget {
                    append_developer_to(&mut self.session,"Request budget reached. Finish your current work and return the partial findings now.".into());
                    // History changed after the usage estimate, so the publishing path must re-estimate.
                    published_usage = None;
                }
                if !resource_budget_warned
                    && (binding
                        .token_budget()
                        .is_some_and(|limit| subagent_tokens >= limit)
                        || binding
                            .cost_budget_usd()
                            .is_some_and(|limit| subagent_cost_usd >= limit))
                {
                    resource_budget_warned = true;
                    append_developer_to(
                        &mut self.session,
                        "Token or cost budget reached. Finish your current work and return the partial findings now.".into(),
                    );
                    published_usage = None;
                }
            }
            self.publish_context_snapshot_with_usage(published_usage);
            if let Some(limit) = self.max_turns
                && request_count >= limit
            {
                return Err(pp_core::cancellation::MaxTurnsExceeded { limit }.into());
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
                thinking_history: &mut self.thinking_history,
                tools: &self.tools,
                journal: &mut self.journal,
                workflow: &mut self.workflow,
                input,
                plan_dirty: &mut plan_dirty,
                reminder: &mut reminder,
                hooks: hooks.clone(),
            };
            let pre_request_raw_tokens = usage.estimated_tokens;
            let finished = match &mut self.session {
                Session::Completion(request) => {
                    completion_turn(resources, request, &metadata, update, cancellation, emit)
                }
                Session::Response(request) => {
                    response_turn(resources, request, &metadata, update, cancellation, emit)
                }
            };
            let finished = match finished {
                Ok(finished) => {
                    self.last_request_raw_tokens =
                        self.last_request.map(|_| pre_request_raw_tokens);
                    if self
                        .subagent
                        .as_ref()
                        .is_some_and(|binding| binding.depth > 0)
                    {
                        if let Some(turn_usage) = self.last_request {
                            let turn_tokens = turn_usage
                                .input_tokens
                                .saturating_add(turn_usage.output_tokens);
                            subagent_tokens = subagent_tokens.saturating_add(turn_tokens);
                            if let Some(price) = self.catalog.cost(&self.current_model.reference) {
                                let cost = price.calculate(turn_usage).or_else(|| {
                                    price.calculate(pp_core::context::TokenUsage {
                                        cached_tokens: Some(0),
                                        cache_write_tokens: Some(0),
                                        ..turn_usage
                                    })
                                });
                                if let Some(total) = cost.map(|c| c.total())
                                    && total.is_finite()
                                    && total >= 0.0
                                {
                                    subagent_cost_usd += total;
                                }
                            }
                        } else {
                            subagent_tokens =
                                subagent_tokens.saturating_add(pre_request_raw_tokens);
                        }
                    }
                    pruned_since_progress = false;
                    overflow_stage = 0;
                    finished
                }
                Err(error) if pp_llm::retry::is_context_overflow(&error) && overflow_stage < 2 => {
                    cancellation::check(cancellation)?;
                    self.persist_pending_input(user.take())?;
                    let mut recovered = false;
                    if overflow_stage == 0 {
                        overflow_stage = 1;
                        if self.prune_history(true)?.is_some() {
                            recovered = true;
                        }
                    }
                    if !recovered && overflow_stage == 1 {
                        overflow_stage = 2;
                        if matches!(
                            self.compact_impl(true, cancellation, emit),
                            Ok(super::context::CompactionOutcome::Compacted { .. })
                        ) {
                            recovered = true;
                        }
                    }
                    if recovered {
                        estimate.invalidate();
                        cached_usage = None;
                        claim_boundary = false;
                        continue;
                    }
                    // Emergency pruning and compaction freed no space, so take the existing model failure/fallback path.
                    if !self.model_source.is_fallback()
                        && let Some(limit) = self.max_turns
                        && request_count >= limit
                    {
                        return Err(pp_core::cancellation::MaxTurnsExceeded { limit }.into());
                    }
                    self.pending_instructions = None;
                    if self.activate_fallback(emit)? {
                        claim_boundary = false;
                        estimate.invalidate();
                        continue;
                    }
                    return Err(error);
                }
                Err(error) if pp_llm::retry::model_request_failed(&error) => {
                    cancellation::check(cancellation)?;
                    // A failed main-model request still consumes the --max-turns budget: once turns are exhausted,
                    // terminate at the limit instead of activating the fallback model or issuing extra requests;
                    // a fallback's own failure keeps the existing semantics and surfaces the model error directly.
                    if !self.model_source.is_fallback()
                        && let Some(limit) = self.max_turns
                        && request_count >= limit
                    {
                        return Err(pp_core::cancellation::MaxTurnsExceeded { limit }.into());
                    }
                    self.pending_instructions = None;
                    if self.activate_fallback(emit)? {
                        claim_boundary = false;
                        // The fallback model may switch protocols and rebuild history.
                        estimate.invalidate();
                        continue;
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            self.pending_instructions = None;
            self.journal.commit((&self.session).into())?;
            self.tools.commit_wait_results()?;
            if matches!(finished, TurnOutcome::PlanSubmitted) && !cancellation.is_cancelled() {
                self.receive_subagents(emit)?;
            }
            if self.apply_runtime_models_changed()? {
                // Same as at loop entry: after a model switch the history may already be rebuilt across protocols.
                estimate.invalidate();
            }
            if plan_dirty {
                self.refresh_plan_progress(emit)?;
            }
            let usage = self.context_usage_cached(&mut estimate)?;
            emit(AgentEvent::ContextUpdated { usage });
            cached_usage = Some(usage);
            if self.tools.yielded.borrow().is_some() {
                if let Some(binding) = self.subagent.clone()
                    && binding.pending()
                {
                    cached_usage = None;
                    self.tools.yielded.borrow_mut().take();
                    self.wait_owned_subagents(cancellation, emit)?;
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
                text,
            } = finished
            {
                cached_usage = None;
                plan_sync::sync_plan_todos(
                    &self.tools,
                    &mut self.journal,
                    &mut self.workflow,
                    emit,
                    true,
                )?;
                if let Some(binding) = self.subagent.clone()
                    && binding.pending()
                {
                    self.wait_owned_subagents(cancellation, emit)?;
                    continue;
                }
                if self.tools.yield_schema.is_some() && self.tools.yielded.borrow().is_none() {
                    bail!(
                        "structured subagent task did not call yield; result does not satisfy the completion protocol."
                    );
                }
                let remind = todo::eligible(&self.workflow, &reminder, awaiting_user_answer);
                // A completion candidate awaiting the user or holding an error never runs Stop; only root Execute is eligible.
                let stop_hooks =
                    !awaiting_user_answer && hooks.enabled(pp_config::config::HookEvent::Stop);
                // Atomic "claim or close": the final reply and new supplementary input are arbitrated only inside this short lock.
                // With a Stop handler the queue stays open so the user can still supply input while the script runs.
                if let Some(queue) = input {
                    let batch = queue.claim_steering(!remind && !stop_hooks)?;
                    if !batch.is_empty()
                        && self.apply_pending(batch, input, &mut user, cancellation, emit)? > 0
                    {
                        // This boundary only consumes a fixed batch; messages arriving during confirmation wait for the next boundary.
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
                if let Err(reason) = self
                    .output_schema
                    .as_ref()
                    .map_or(Ok(()), |schema| schema.validate(&text))
                {
                    if self
                        .output_schema
                        .as_mut()
                        .is_some_and(super::output_schema::OutputSchema::consume_retry)
                    {
                        let message =
                            super::output_schema::OutputSchema::correction_message(&reason);
                        append_developer_to(&mut self.session, message);
                        self.journal.commit((&self.session).into())?;
                        continue;
                    }
                    bail!("output does not match --output-schema: {reason}");
                }
                if stop_hooks {
                    // The queue lock is not held while the script runs; on return, re-arbitrate the stop decision and input.
                    let effect = self.hook_stop(&text, stop_hook_active, cancellation, emit)?;
                    cancellation::check(cancellation)?;
                    // `continue:false` is a task-level stop: it applies before supplementary input,
                    // the queue stays as is, and pending items are left for the next run.
                    if let Some(effect) = &effect
                        && let Some(reason) = &effect.stop_reason
                    {
                        bail!("{reason}");
                    }
                    if let Some(queue) = input {
                        let batch = queue.claim_steering(false)?;
                        if !batch.is_empty()
                            && self.apply_pending(batch, input, &mut user, cancellation, emit)? > 0
                        {
                            claim_boundary = false;
                            continue;
                        }
                    }
                    if let Some(effect) = effect
                        && let Some(reason) = effect.deny_reason
                    {
                        if stop_continues >= STOP_HOOK_LIMIT {
                            bail!("Stop hook continuation limit reached ({STOP_HOOK_LIMIT}).");
                        }
                        stop_continues += 1;
                        stop_hook_active = true;
                        // Feedback goes into native developer history: a restore never re-runs the script.
                        append_developer_to(&mut self.session, reason);
                        self.journal.commit((&self.session).into())?;
                        continue;
                    }
                }
                // Final arbitration: atomically claim or close the queue; late batches are still applied at the next boundary.
                if let Some(queue) = input {
                    let batch = queue.claim_steering(true)?;
                    if !batch.is_empty()
                        && self.apply_pending(batch, input, &mut user, cancellation, emit)? > 0
                    {
                        claim_boundary = false;
                        continue;
                    }
                }
                return Ok(RunOutcome::Completed);
            }
            cancellation::check(cancellation)?;
        }
    }

    // Apply one supplementary batch at the commit boundary: first account for the initial input that is
    // not yet persisted, then append, persist and confirm item by item. Confirmed items are never returned;
    // on cancellation or a persistence failure the unapplied items go back—the return happens at a single
    // exit, so failure paths need not maintain matching push_front calls.
    // Returns how many items were accepted: when hooks reject them all the caller must not treat that as new input to the model.
    pub(in crate::agent) fn apply_pending(
        &mut self,
        mut batch: VecDeque<PendingInput>,
        input: Option<&InputQueue>,
        user: &mut Option<&str>,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<usize> {
        let result = (|| -> Result<usize> {
            if user.is_some()
                && let Err(error) = self.persist_pending_input(user.take())
            {
                return Err(error);
            }
            let mut accepted = 0usize;
            while !batch.is_empty() {
                if cancellation.is_cancelled() {
                    return Err(Cancelled.into());
                }
                let item = batch.front().expect("batch is non-empty");
                // Supplementary input also passes UserPromptSubmit first: a rejection drops only this item, a failure stops the task.
                let effect = self.hook_user_input(item.prompt(), cancellation, emit)?;
                if let Some(effect) = &effect {
                    if let Some(reason) = &effect.deny_reason {
                        let item = batch.pop_front().expect("batch is non-empty");
                        emit(AgentEvent::UserInputRejected {
                            id: item.id,
                            text: item.into_display(),
                            reason: reason.clone(),
                        });
                        continue;
                    }
                    if let Some(reason) = &effect.stop_reason {
                        bail!("{reason}");
                    }
                }
                let before_len = self.session_len();
                if let Some(prepared) = &item.prepared {
                    turn::append_input_to(&mut self.session, prepared);
                } else {
                    self.append_user(item.prompt());
                }
                if let Some(effect) = &effect
                    && !effect.additional_context.is_empty()
                {
                    turn::append_developer_to(
                        &mut self.session,
                        effect.additional_context.join("\n\n"),
                    );
                }
                if let Err(error) = self.persist_pending_input(Some(item.prompt())) {
                    self.truncate_session(before_len);
                    return Err(error);
                }
                let item = batch.pop_front().expect("batch is non-empty");
                accepted += 1;
                emit(AgentEvent::UserInputAccepted {
                    id: item.id,
                    text: item.into_display(),
                });
            }
            Ok(accepted)
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

    pub(in crate::agent) fn session_len(&self) -> usize {
        super::persistence::SessionRef::from(&self.session).len()
    }

    // Roll back the in-memory user/developer appends that are not persisted yet.
    fn truncate_session(&mut self, len: usize) {
        match &mut self.session {
            Session::Completion(request) => request.messages.truncate(len),
            Session::Response(request) => response_input(request).truncate(len),
        }
    }
}
