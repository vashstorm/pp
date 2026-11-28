use super::super::Agent;
use super::super::ExecutionPreparation;
use super::super::{context, turn};
use super::{NO_PLAN, NO_TASKS, NOT_CURRENT, NOT_PLAN};
use crate::agent::input::InputQueue;
use anyhow::{Result, anyhow, ensure};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::plan;
use pp_core::plan::ExecutionContext;
use pp_core::plan::PlanLocation;
use pp_core::plan::PlanStatus;
use pp_core::plan::ReviewToken;
use pp_core::plan::WorkMode;
use pp_core::plan::WorkflowState;
use pp_tools::plan_catalog;

/// Endpoints awaiting cleanup for a lifecycle move: the (file name, location) of the source
/// and the target.
pub(super) type PlanMoveEndpoints = ((String, PlanLocation), (String, PlanLocation));

impl Agent {
    /// Drafts→Plans handoff after approval: publish the target link → CAS-update the
    /// registration → commit the binding (a pure path change, no revision bump) → after the
    /// journal handoff succeeds the caller removes the source link. A plan already in Plans
    /// is only verified/updated in the registry and not moved again. Returns the source/target
    /// identities awaiting cleanup.
    fn promote_approved_plan(&mut self, token: &ReviewToken) -> Result<Option<PlanMoveEndpoints>> {
        let document = self.workflow.plan.as_ref().expect("ready plan bound");
        if document.location == PlanLocation::Plans {
            return Ok(None);
        }
        let file_name = document
            .file_name
            .clone()
            .ok_or_else(|| anyhow!("{NO_PLAN}"))?;
        let source = (file_name.clone(), document.location);
        let target = (file_name, PlanLocation::Plans);
        {
            let workspace = self.tools.plan_workspace();
            plan_catalog::publish_plan_move(
                workspace,
                &document.id,
                (source.0.as_str(), source.1),
                (target.0.as_str(), target.1),
                token.content.as_bytes(),
            )?;
        }
        let previous = self.workflow.clone();
        self.workflow
            .plan
            .as_mut()
            .expect("ready plan bound")
            .location = PlanLocation::Plans;
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error);
        }
        Ok(Some((source, target)))
    }

    /// Source link cleanup after the journal handoff commits; a failure does not revoke the
    /// committed state, and the leftover source link is handed to ID reconciliation for a
    /// same-inode confirmed cleanup, never reported as a rollback.
    fn finish_approved_move(&self, pending_move: Option<PlanMoveEndpoints>, token: &ReviewToken) {
        let Some((source, target)) = pending_move else {
            return;
        };
        let workspace = self.tools.plan_workspace();
        if let Err(error) = plan_catalog::commit_plan_move(
            workspace,
            (source.0.as_str(), source.1),
            (target.0.as_str(), target.1),
            token.content.as_bytes(),
        ) {
            tracing::warn!(event = "plan_move_cleanup_failed", error = %error);
        }
    }

    /// Exit direction of the `/plan` toggle: return to Execute and release the current
    /// plan/review binding.
    /// The saved draft file and registration are kept as-is (no longer marked Abandoned) and
    /// the old draft continues through /plan-list. An unsaved draft may only be left after the
    /// user confirms abandoning that same binding.
    pub fn leave_plan(
        &mut self,
        cancellation: &CancellationToken,
        discard_plan_id: Option<&str>,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_PLAN}");
        if let Some(id) = discard_plan_id {
            ensure!(
                self.workflow
                    .plan
                    .as_ref()
                    .is_some_and(|plan| plan.id == id && plan.revision == 0),
                "Plan changed; confirm leaving again."
            );
        }
        if let Some(plan) = &self.workflow.plan
            && plan.revision == 0
            && !plan.origin_prompt.trim().is_empty()
        {
            ensure!(
                discard_plan_id == Some(plan.id.as_str()),
                "Confirm discarding the unsaved draft before leaving Plan mode."
            );
        }
        self.journal.check()?;
        let model = self.default_execution_model()?;
        self.ensure_plan_mcp_access(cancellation)?;
        self.ensure_execute_mcp_access(cancellation)?;
        // Release the binding and keep the plan and registration on disk.
        let workflow = WorkflowState::default();
        let session = self.execution_session(&model, &workflow, true)?;
        let metadata = model.metadata(&self.tools, &self.base_system, self.prompt_source.as_ref());
        cancellation::check(cancellation)?;
        self.journal
            .commit_plan_exit(&metadata, (&session).into(), &workflow)?;
        self.session = session;
        self.workflow = workflow;
        self.install_execution_model(model);
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: None,
            todo_snapshot: None,
        })
    }

    /// Approve the reviewed plan and prepare the execution handoff; the context may be
    /// compacted first under Plan permissions.
    /// Before committing, MCP services may be reconnected under execution permissions
    /// cancellably, then the token is verified; on failure Plan/Ready is kept and an
    /// implementation request must never be issued from an unfinished handoff.
    pub fn approve_plan(
        &mut self,
        token: ReviewToken,
        context: ExecutionContext,
        input: &InputQueue,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(pp_core::events::AgentEvent),
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_CURRENT}");
        // Reconcile the actual path by ID before approval access; tombstones, moves and first
        // registrations surface here.
        self.reconcile_plan_location()?;
        // Reconnect missing services under Plan permissions before approval, then touch the
        // approval state.
        self.ensure_plan_mcp_access(cancellation)?;
        {
            let document = self
                .workflow
                .plan
                .as_ref()
                .ok_or_else(|| anyhow!("{NOT_CURRENT}"))?;
            ensure!(token.content == document.content, "{NOT_CURRENT}");
            let (_, total) = plan::task_progress(&token.content);
            ensure!(total > 0, "{NO_TASKS}");
        }
        self.check_plan_token_for_approval(&token, input)?;
        if context == ExecutionContext::Compact {
            // Compaction keeps Plan permissions; on failure or cancellation there is no
            // execution handoff. Recheck the token and queue afterwards to reject external
            // revisions and supplemental input that arrived during compaction.
            let mut attempted = false;
            let outcome = self.compact(cancellation, &mut |event| {
                attempted |= matches!(
                    &event,
                    pp_core::events::AgentEvent::CompactionStarted { .. }
                );
                emit(event);
            })?;
            ensure!(
                !matches!(outcome, context::CompactionOutcome::Cancelled),
                "Plan execution cancelled during context compaction."
            );
            ensure!(
                !attempted || !matches!(outcome, context::CompactionOutcome::Unchanged),
                "Context compaction did not reduce history; review the plan before executing."
            );
            cancellation::check(cancellation)?;
            self.check_plan_token_for_approval(&token, input)?;
        }
        let model = self.default_execution_model()?;
        self.ensure_execute_mcp_access(cancellation)?;
        let fresh = context == ExecutionContext::Fresh;
        let candidate = (|| -> Result<_> {
            let mut workflow = self.workflow.clone();
            workflow.mode = WorkMode::Execute;
            if fresh {
                workflow.review = None;
            }
            let document = workflow.plan.as_mut().expect("ready plan bound");
            document.status = PlanStatus::Executed;
            document.location = PlanLocation::Plans;
            // Freeze the task text and order from the reviewed body; later progress imports keep
            // that baseline.
            document.approved_tasks = Some(
                plan::plan_tasks(&token.content)
                    .into_iter()
                    .map(|task| task.text)
                    .collect(),
            );
            let prompt_context = if fresh {
                ExecutionContext::Fresh
            } else {
                ExecutionContext::Keep
            };
            let prompt = plan::execution_prompt(document, prompt_context);
            let mut todos = pp_tools::todo::project_plan_todos(document, &[], false)?;
            pp_tools::todo::normalize_in_progress(&mut todos);
            workflow.todos = todos;
            workflow.todo_plan_id = Some(document.id.clone());
            let mut session = self.execution_session(&model, &workflow, !fresh)?;
            turn::append_user_to(&mut session, &prompt);
            // Verify the input queue and plan body after preparing the native request, and only
            // then publish the plan move.
            cancellation::check(cancellation)?;
            self.check_plan_token_for_approval(&token, input)?;
            Ok((workflow, session, prompt))
        })();
        let (workflow, session, prompt) = candidate?;
        let committed = (|| -> Result<_> {
            // Held in PlanLease → registry lock order until the execution task finishes.
            self.ensure_task_plan_lease()?;
            let pending_move = self.promote_approved_plan(&token)?;
            let metadata =
                model.metadata(&self.tools, &self.base_system, self.prompt_source.as_ref());
            let mut fork = fresh.then(|| self.journal.fork_empty());
            let target = fork.as_mut().unwrap_or(&mut self.journal);
            target.commit_plan_handoff(&metadata, (&session).into(), &workflow, &prompt)?;
            if fresh {
                // The source planning session stays in Plan; the approved plan is read-only in
                // the source session.
                let mut source = self.workflow.clone();
                source.plan.clone_from(&workflow.plan);
                self.journal.write_workflow(&source)?;
            }
            Ok((pending_move, fork))
        })();
        let (pending_move, fork) = match committed {
            Ok(committed) => committed,
            Err(error) => {
                self.plan_lease = None;
                return Err(error);
            }
        };
        self.session = session;
        self.workflow = workflow;
        self.install_execution_model(model);
        if let Some(journal) = fork {
            self.reset_lsp_diagnostics();
            self.tools.permission.clear_session();
            self.journal = journal;
            self.tools.artifacts = Default::default();
            self.tools.reset_pipe_store();
            if let Some(id) = self.journal.id() {
                let _ = self.tools.artifacts.bind(self.journal.directory(), id);
            }
            self.tools.clear_caches();
            self.clear_session_usage();
            self.thinking_history.clear();
            self.clear_session_cost();
            self.pending_project_load.clear();
            self.reported_project_load.clear();
            self.static_project_prompt = None;
            self.delivered_project_docs.clear();
            self.session_date = super::super::current_session_date();
            self.session.clear_prompt_cache_key();
            self.session.apply_cache_policy(
                self.prompt_cache_key_enabled,
                self.prompt_cache_retention,
                || super::super::generate_prompt_cache_key(self.journal.id()),
            );
            self.tools.reset_project_instructions([]);
        }
        self.finish_approved_move(pending_move, &token);
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: fresh.then(Vec::new),
            todo_snapshot: turn::todo_snapshot(&self.workflow),
        })
    }
}
