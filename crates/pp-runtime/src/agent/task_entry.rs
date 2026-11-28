//! Task entry: plan lease, run orchestration and task teardown boundaries.
use super::{
    Agent,
    input::InputQueue,
    persistence::Metadata,
    turn::{self, append_user_to, log_task_finished},
};
use anyhow::{Result, bail, ensure};
use pp_core::cancellation::{self, CancellationToken, Cancelled, CleanupError, RunOutcome};
use pp_core::events::AgentEvent;

impl Agent {
    pub(in crate::agent) fn connect_pending_plan_mcp(
        &mut self,
        cancellation: &CancellationToken,
        access: pp_mcp::Access,
    ) -> Result<()> {
        if self.workflow.mode == pp_core::plan::WorkMode::Plan {
            let result = self.tools.mcp.connect_enabled(cancellation, access);
            if let Err(error) = result {
                if error.is::<pp_core::cancellation::CleanupError>() {
                    self.plan_cleanup_error = Some(format!(
                        "MCP cleanup failed for Plan mode: {error:#}; run /plan again to retry."
                    ));
                }
                return Err(error);
            }
            self.session
                .set_tools(self.tools.all_definitions(self.workflow.mode));
        }
        Ok(())
    }

    pub(in crate::agent) fn ensure_task_plan_lease(&mut self) -> Result<()> {
        let Some(document) = &self.workflow.plan else {
            self.plan_lease = None;
            return Ok(());
        };
        if self.workflow.mode == pp_core::plan::WorkMode::Execute
            && matches!(
                document.status,
                pp_core::plan::PlanStatus::Completed | pp_core::plan::PlanStatus::Abandoned
            )
            && self.workflow.todo_plan_id.is_none()
        {
            self.plan_lease = None;
            return Ok(());
        }
        if self
            .plan_lease
            .as_ref()
            .is_some_and(|lease| lease.id() == document.id)
        {
            return Ok(());
        }
        self.plan_lease = None;
        self.plan_lease = Some(pp_tools::plan_catalog::PlanLease::acquire(
            self.tools.plan_workspace(),
            &document.id,
        )?);
        // After the lease is taken, coordinate by ID (lock order lease → registry lock): registry renames,
        // moves and first registrations are synced to the binding before the task and plan write tools touch it.
        self.reconcile_plan_location()
    }

    pub(in crate::agent) fn prepare_plan_task(&mut self, prompt: &str) -> Result<()> {
        if self.workflow.mode != pp_core::plan::WorkMode::Plan {
            return Ok(());
        }
        self.journal.check()?;
        // A bare /plan only stands by: the first non-empty task allocates the document and binds the origin.
        let created = self.workflow.plan.is_none() && !prompt.trim().is_empty();
        let before_creation = created.then(|| self.workflow.clone());
        if created {
            self.workflow.plan = Some(pp_core::plan::PlanDocument::new()?);
        }
        if self.workflow.plan.is_none() {
            return Ok(());
        }
        let plan = self.workflow.plan.as_mut().expect("plan allocated");
        ensure!(
            !matches!(
                plan.status,
                pp_core::plan::PlanStatus::Executed | pp_core::plan::PlanStatus::Abandoned
            ),
            "This plan has already been handed off; use /plan to toggle."
        );
        // A Plan opened from the list can also be discussed further; once a discussion is accepted it drops to Draft uniformly,
        // opening only the planning tools and forbidding execution until it is resubmitted and explicitly approved.
        let old_status = plan.status;
        let set_origin = plan.origin_prompt.is_empty() && !prompt.trim().is_empty();
        let persist = created || set_origin || old_status == pp_core::plan::PlanStatus::Ready;
        let previous = before_creation.or_else(|| persist.then(|| self.workflow.clone()));
        let plan = self.workflow.plan.as_mut().expect("plan allocated");
        if set_origin {
            plan.origin_prompt = prompt.to_owned();
        }
        if plan.status == pp_core::plan::PlanStatus::Ready {
            plan.status = pp_core::plan::PlanStatus::Draft;
        }
        if created {
            self.workflow.todo_plan_id = self.workflow.plan.as_ref().map(|plan| plan.id.clone());
            self.workflow.todos.clear();
        }
        if let Some(previous) = previous {
            self.commit_workflow(previous)?;
        }
        Ok(())
    }

    pub fn run(
        &mut self,
        prompt: &str,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        self.run_user(prompt, None, cancellation, input, emit)
    }

    /// Outcome of UserPromptSubmit: a denial or `continue:false` fails outright; otherwise the context
    /// to inject is returned. Subagent task text is not human input and does not pass through here.
    fn user_prompt_hook(
        &mut self,
        prompt: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Vec<String>> {
        let Some(effect) = self.hook_user_input(prompt, cancellation, emit)? else {
            return Ok(Vec::new());
        };
        if let Some(reason) = effect.stop_reason {
            bail!("{reason}");
        }
        if let Some(reason) = effect.deny_reason {
            bail!("{reason}");
        }
        Ok(effect.additional_context)
    }

    pub fn input_limits(&self) -> pp_config::config::ToolLimits {
        self.tools.limits
    }

    pub fn run_input(
        &mut self,
        prepared: &crate::user_input::UserInput,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        self.run_user(&prepared.display, Some(prepared), cancellation, input, emit)
    }

    fn run_user(
        &mut self,
        prompt: &str,
        prepared: Option<&crate::user_input::UserInput>,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        tracing::info!(
            event = "task_started",
            pid = std::process::id(),
            operation = "run"
        );
        let started = std::time::Instant::now();
        let result = (|| -> Result<RunOutcome> {
            self.journal.check()?;
            if let Some(error) = &self.plan_cleanup_error {
                bail!("{error}");
            }
            ensure!(!prompt.trim().is_empty(), "prompt must not be blank");
            // Closing the startup background connect: wait only when the task really starts, so the UI and tip line render first.
            // Cancellation while waiting is not reported here: it is left to the cancellation boundary below to save accepted input.
            pp_core::logging::perf::mark("mcp_wait_start");
            let _ = self.tools.mcp.await_connect(cancellation);
            pp_core::logging::perf::mark("mcp_wait_end");
            // Check before commit: on a denial or failure, append no user message, prepare no plan and request no model.
            let hook_context = self.user_prompt_hook(prompt, cancellation, emit)?;
            self.connect_pending_plan_mcp(cancellation, self.tools.mcp_access())?;
            self.apply_runtime_models()?;
            self.restore_resolved(emit)?;
            self.ensure_task_plan_lease()?;
            self.prepare_plan_task(prompt)?;
            self.ensure_task_plan_lease()?;
            self.refresh_instructions()?;
            self.begin_task_status(prompt, cancellation);
            if let Some(state) = &self.auto_review {
                state.set_user_intent(prompt);
            }
            self.sync_auto_review();
            self.publish_context(emit)?;
            pp_core::logging::perf::mark("request_ready");
            if let Some(prepared) = prepared {
                turn::append_input_to(&mut self.session, prepared);
            } else {
                self.append_user(prompt);
            }
            if !hook_context.is_empty() {
                // The context is committed together with this input, so a restore replays only saved history.
                turn::append_developer_to(&mut self.session, hook_context.join("\n\n"));
            }
            self.run_loop(Some(prompt), input, cancellation, emit)
        })();
        self.finish_task("run", started, result, cancellation, input, emit)
    }

    pub fn continue_session(
        &mut self,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        tracing::info!(
            event = "task_started",
            pid = std::process::id(),
            operation = "continue"
        );
        let started = std::time::Instant::now();
        let result = (|| -> Result<RunOutcome> {
            self.journal.check()?;
            if let Some(error) = &self.plan_cleanup_error {
                bail!("{error}");
            }
            // A resumed run goes through the same task entry barrier: wait for the startup background connect to finish if it has not;
            // cancellation while waiting is handled by the turn loop's cancellation boundary.
            pp_core::logging::perf::mark("mcp_wait_start");
            let _ = self.tools.mcp.await_connect(cancellation);
            pp_core::logging::perf::mark("mcp_wait_end");
            self.connect_pending_plan_mcp(cancellation, self.tools.mcp_access())?;
            self.ensure_task_plan_lease()?;
            self.prepare_plan_task("")?;
            self.ensure_task_plan_lease()?;
            self.continue_task_status();
            self.sync_auto_review();
            self.run_loop(None, input, cancellation, emit)
        })();
        self.finish_task("continue", started, result, cancellation, input, emit)
    }

    // Task teardown shared by run and continue_session: close the input queue; a context publish failure
    // must not mask cancellation or a real task error.
    pub(in crate::agent) fn finish_task(
        &mut self,
        operation: &str,
        started: std::time::Instant,
        result: Result<RunOutcome>,
        cancellation: &CancellationToken,
        input: Option<&InputQueue>,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<RunOutcome> {
        self.finish_task_title();
        if self.task_status.view.is_none() {
            self.set_task_accounting(None);
        }
        let result = self.close_queue(input, result);
        let mut result = self.finish_run(result, cancellation);
        if let Err(error) = self.refresh_plan_progress(emit) {
            let error = match result {
                Err(original) => match original.downcast::<CleanupError>() {
                    Ok(CleanupError(inner)) => CleanupError(
                        inner.context(format!("Plan progress update also failed: {error:#}")),
                    )
                    .into(),
                    Err(original) => {
                        original.context(format!("Plan progress update also failed: {error:#}"))
                    }
                },
                Ok(RunOutcome::Cancelled) => {
                    error.context("Task was cancelled; file changes were not rolled back.")
                }
                _ => error,
            };
            self.journal.error(&format!("{error:#}"));
            result = Err(error);
        }
        // Automatic archiving at the success boundary: attempted only when close_queue, finish_run (including background cleanup) and
        // progress sync all succeeded, the result really is Completed and cancellation is not set; silently skipped when the plan
        // is not fully checked off or has zero tasks (keeping Executed/Plans), while a task set that does not match the approved set
        // and real IO/registry/journal failures turn the result into an error.
        if matches!(&result, Ok(RunOutcome::Completed))
            && !cancellation.is_cancelled()
            && let Err(error) = self.archive_completed_plan(emit)
        {
            let error = error.context("Completed plan was not archived.");
            self.journal.error(&format!("{error:#}"));
            result = Err(error);
        }
        if !matches!(&result, Ok(RunOutcome::Completed)) {
            let _ = self.publish_context(emit);
        }
        log_task_finished(operation, started, &result);
        self.plan_lease = None;
        result
    }

    // Every exit path closes the queue; a close failure becomes the primary error only when the task itself succeeded.
    fn close_queue(
        &self,
        input: Option<&InputQueue>,
        result: Result<RunOutcome>,
    ) -> Result<RunOutcome> {
        match input {
            None => result,
            Some(queue) => match (result, queue.close()) {
                (result, Ok(())) => result,
                (Ok(_), Err(error)) => Err(error),
                (Err(original), Err(_)) => Err(original),
            },
        }
    }

    pub(in crate::agent) fn append_user(&mut self, prompt: &str) {
        append_user_to(&mut self.session, prompt);
    }

    pub(in crate::agent) fn finish_run(
        &mut self,
        result: Result<RunOutcome>,
        cancellation: &CancellationToken,
    ) -> Result<RunOutcome> {
        let mut result = result;
        if cancellation.is_cancelled() || result.is_err() {
            self.cancel_lsp_diagnostics();
        }
        if cancellation.is_cancelled() {
            if let Some(binding) = &self.subagent
                && let Err(cleanup) = binding.cancel_owned()
            {
                result = merge_outcome(result, Err(cleanup), "cancelling subagents");
            }
            self.client.cancel_pending_io();
        }
        let result = match self.tools.stop_background_tasks() {
            Ok(()) => result,
            Err(cleanup) => merge_outcome(result, Err(cleanup), "stopping background tasks"),
        };
        let result = if !matches!(
            &result,
            Ok(RunOutcome::Completed | RunOutcome::AwaitingPlanReview)
        ) {
            if let Some(binding) = &self.subagent {
                merge_outcome(result, binding.reap_owned(), "reaping subagents")
            } else {
                result
            }
        } else {
            result
        };
        match result {
            Err(error) if error.is::<Cancelled>() => {
                self.journal.check()?;
                self.journal.cancelled()?;
                Ok(RunOutcome::Cancelled)
            }
            Err(error) => {
                self.journal.error(&format!("{error:#}"));
                self.journal.check()?;
                Err(error)
            }
            Ok(outcome) => {
                self.journal.check()?;
                Ok(outcome)
            }
        }
    }

    pub(in crate::agent) fn persist_pending_input(&mut self, user: Option<&str>) -> Result<()> {
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
        self.journal.sent(
            &metadata,
            (&self.session).into(),
            &self.workflow,
            user,
            self.pending_instructions.as_deref(),
        )?;
        self.pending_instructions = None;
        Ok(())
    }
}

fn merge_outcome(
    result: Result<RunOutcome>,
    cleanup: Result<()>,
    label: &str,
) -> Result<RunOutcome> {
    match result {
        Ok(outcome) => cancellation::merge_cleanup_result(Ok(()), cleanup, label).map(|()| outcome),
        Err(error) => match cancellation::merge_cleanup_result(Err(error), cleanup, label) {
            Ok(()) => unreachable!("merging an Err result always returns Err"),
            Err(merged) => Err(merged),
        },
    }
}
