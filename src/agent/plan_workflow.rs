//! Worker-side Plan mode lifecycle: mode entry/exit with MCP lifecycle
//! safety, safe external plan import, review-token validation, and the two
//! post-approval execution handoffs.
//!
//! Every permission change is persisted through the journal's transactional
//! workflow record before or as part of the transition, and a failed MCP
//! cleanup keeps the fail-closed `plan_cleanup_error` gate so no request can
//! start with half-removed capabilities.

use super::{
    Agent, ExecutionPreparation, Session,
    persistence::{Metadata, SessionJournal},
};
use crate::{
    agent::input::InputQueue,
    cancellation::{self, CancellationToken},
    mcp::Registry,
    plan::{
        self, ExecutionContext, PlanDocument, PlanReview, PlanStatus, ReviewToken, WorkMode,
        WorkflowState,
    },
    project_instructions,
};
use anyhow::{Result, anyhow, bail, ensure};

const NOT_CURRENT: &str = "Plan review is no longer current.";
const CHANGED: &str = "Plan changed; review the new revision before executing.";
const NO_TASKS: &str = "Plan must contain at least one Markdown TODO item.";
const PENDING: &str = "Apply pending input before approving the plan.";
const NOT_PLAN: &str = "Plan mode is not active.";
const NO_PLAN: &str = "No saved plan.";
const NO_ORIGIN: &str = "Enter the task before reviewing this plan.";

enum SavedInstructions {
    Completion(async_openai::types::chat::ChatCompletionRequestSystemMessageContent),
    Response(Option<String>),
}

impl SavedInstructions {
    fn take(session: &mut Session) -> Self {
        match session {
            Session::Completion(request) => {
                let Some(async_openai::types::chat::ChatCompletionRequestMessage::System(message)) =
                    request.messages.first_mut()
                else {
                    unreachable!("session has system instructions");
                };
                Self::Completion(std::mem::replace(&mut message.content, "".into()))
            }
            Session::Response(request) => Self::Response(request.instructions.take()),
        }
    }

    fn restore(self, session: &mut Session) {
        match (self, session) {
            (Self::Completion(content), Session::Completion(request)) => {
                let Some(async_openai::types::chat::ChatCompletionRequestMessage::System(message)) =
                    request.messages.first_mut()
                else {
                    unreachable!("session has system instructions");
                };
                message.content = content;
            }
            (Self::Response(instructions), Session::Response(request)) => {
                request.instructions = instructions
            }
            _ => unreachable!("approval does not change protocol"),
        }
    }
}

impl Agent {
    /// Base system prompt: current project instructions and skill summary on
    /// top of `base_system`, without any mode rules. Shared by the ordinary
    /// system composition and the independent reviewer.
    pub(super) fn compose_base_system(&self) -> Result<String> {
        let project = project_instructions::load(self.tools.cwd())?;
        let summary = self
            .tools
            .skills
            .lock()
            .map_err(|_| anyhow::anyhow!("skill registry unavailable"))?
            .summary();
        Ok(project_instructions::compose(
            &self.base_system,
            self.tools.cwd(),
            &project,
            &summary,
        ))
    }

    /// Full system prompt for the given workflow, including mode-aware plan
    /// instructions. Shared by the per-request refresh, resume, tree
    /// navigation and the fresh-context execution candidate; Plan rules are
    /// never baked into `base_system`.
    pub(super) fn compose_system(&self, workflow: &WorkflowState) -> Result<String> {
        let mut system = self.compose_base_system()?;
        let instructions = workflow.instructions();
        if !instructions.is_empty() {
            system.push_str("\n\n");
            system.push_str(&instructions);
        }
        Ok(system)
    }

    /// Persist a workflow change; on failure restore the whole `previous`
    /// state so callers keep one transactional rollback idiom.
    pub(super) fn commit_workflow(&mut self, previous: WorkflowState) -> Result<()> {
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error);
        }
        Ok(())
    }

    /// Stop every managed MCP process so Plan mode cannot reach them. On
    /// failure the registry, its slots and the UI's exit handle stay in place
    /// and `plan_cleanup_error` blocks run/continue until `/plan` retries.
    fn ensure_plan_mcp_shutdown(&mut self, cancellation: &CancellationToken) -> Result<()> {
        cancellation::check(cancellation)?;
        if let Err(error) = self.tools.mcp.shutdown() {
            let message = format!(
                "MCP servers could not be stopped for Plan mode: {error:#}; run /plan again to retry."
            );
            self.plan_cleanup_error = Some(message.clone());
            bail!("{message}");
        }
        self.plan_cleanup_error = None;
        self.tools.mcp = Registry::default();
        self.session
            .set_tools(self.tools.all_definitions(WorkMode::Plan));
        Ok(())
    }

    fn abort_execution_preparation(
        &mut self,
        mut registry: Registry,
        error: anyhow::Error,
    ) -> anyhow::Error {
        match registry.shutdown() {
            Ok(()) => error,
            Err(cleanup) => {
                self.plan_cleanup_error = Some(format!(
                    "Prepared MCP services could not be stopped: {cleanup:#}; run /plan again to retry."
                ));
                self.tools.mcp = registry;
                cleanup.context(format!("Execution preparation also failed: {error:#}"))
            }
        }
    }

    /// `/plan`: enter Plan mode. The workflow authority (mode plus a fresh
    /// revision-0 document) is persisted before any capability is removed; a
    /// previous plan's file on disk is never touched. When already in Plan
    /// mode this retries a previously failed shutdown.
    pub(crate) fn enter_plan(&mut self, cancellation: &CancellationToken) -> Result<WorkflowState> {
        cancellation::check(cancellation)?;
        if self.workflow.mode == WorkMode::Plan {
            self.ensure_plan_mcp_shutdown(cancellation)?;
            return Ok(self.workflow.clone());
        }
        self.install_fresh_plan()?;
        self.ensure_plan_mcp_shutdown(cancellation)?;
        Ok(self.workflow.clone())
    }

    /// `/plan new`: bind another plan document, keeping the old file on disk.
    /// In Execute mode this is entering Plan with a fresh plan.
    pub(crate) fn new_plan(&mut self, cancellation: &CancellationToken) -> Result<WorkflowState> {
        cancellation::check(cancellation)?;
        if self.workflow.mode != WorkMode::Plan {
            return self.enter_plan(cancellation);
        }
        if self.plan_cleanup_error.is_some() {
            self.ensure_plan_mcp_shutdown(cancellation)?;
        }
        self.install_fresh_plan()?;
        Ok(self.workflow.clone())
    }

    /// Bind a fresh revision-0 document and persist the transition atomically.
    fn install_fresh_plan(&mut self) -> Result<()> {
        self.journal.check()?;
        let document = PlanDocument::new()?;
        let previous = std::mem::replace(
            &mut self.workflow,
            WorkflowState {
                mode: WorkMode::Plan,
                plan: Some(document),
                review: None,
            },
        );
        self.commit_workflow(previous)
    }

    /// Register on-disk plan bytes as a new revision. Returns `false` when the
    /// file matches the registered snapshot. Draft/Ready documents become
    /// Ready only when the imported content has a genuine Markdown task list;
    /// Executed/Abandoned documents remain read-only after importing progress.
    fn import_plan_revision(&mut self, content: String, new_title: &str) -> Result<bool> {
        plan::validate_content(&content)?;
        let revision = {
            let bound = self.workflow.plan.as_ref().expect("plan bound");
            if bound.content == content {
                return Ok(false);
            }
            if bound.revision == 0 {
                plan::validate_title(new_title)?;
            }
            bound.next_revision()?
        };
        let previous = self.workflow.clone();
        let document = self.workflow.plan.as_mut().expect("plan bound");
        if document.revision == 0 {
            document.title = new_title.to_owned();
        }
        document.revision = revision;
        let (_, total) = plan::task_progress(&content);
        document.content = content;
        if matches!(document.status, PlanStatus::Draft | PlanStatus::Ready) {
            document.status = if total > 0 {
                PlanStatus::Ready
            } else {
                PlanStatus::Draft
            };
        }
        self.commit_workflow(previous)?;
        Ok(true)
    }

    /// Safe plan review snapshot: reads the current file through the NOFOLLOW
    /// workspace handle, imports external edits as a new revision, and issues
    /// a review token bound to the current session, plan and exact bytes. A
    /// missing or unreadable file is an error; the logged snapshot is never a
    /// fallback. Executed/Abandoned plans safely refresh their displayed
    /// snapshot without regaining approval eligibility.
    pub(crate) fn read_plan(&mut self) -> Result<PlanReview> {
        self.journal.check()?;
        let document = self
            .workflow
            .plan
            .clone()
            .ok_or_else(|| anyhow!("{NO_PLAN}"))?;
        let report = self.workflow.review.clone();
        if document.revision == 0 {
            // An externally hand-written plan is imported with a fixed title;
            // a missing file simply means nothing was saved yet.
            match self.tools.read_plan(&document.id) {
                Ok(content) => {
                    ensure!(!document.origin_prompt.trim().is_empty(), "{NO_ORIGIN}");
                    self.import_plan_revision(content, "Plan")?;
                }
                Err(error) if plan_file_missing(&error) => bail!("{NO_PLAN}"),
                Err(error) => return Err(error),
            }
        } else {
            let content = self.tools.read_plan(&document.id)?;
            self.import_plan_revision(content, &document.title)?;
        }
        let document = self.workflow.plan.as_ref().expect("plan bound").clone();
        let session_id = self
            .journal
            .id()
            .ok_or_else(|| anyhow!("{NO_ORIGIN}"))?
            .to_owned();
        Ok(PlanReview {
            token: ReviewToken {
                session_id,
                plan_id: document.id.clone(),
                revision: document.revision,
                content: document.content.clone(),
            },
            document,
            report,
        })
    }

    /// `/plan off`: leave Plan mode. Pending cleanup is finished and a
    /// cancelable Execute registry is prepared before the persisted Abandoned
    /// transition; on any failure the workflow stays Plan and the old
    /// registry remains installed.
    pub(crate) fn leave_plan(
        &mut self,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_PLAN}");
        self.journal.check()?;
        self.ensure_plan_mcp_shutdown(cancellation)?;
        let config = self
            .catalog
            .resolve(&self.current_model.reference)
            .map_err(|_| anyhow!("current model is unavailable"))?;
        let registry =
            Registry::connect(&config.settings.mcp_servers, self.tools.cwd(), cancellation)?;
        let cleanup = match registry.cleanup() {
            Ok(cleanup) => cleanup,
            Err(error) => return Err(self.abort_execution_preparation(registry, error)),
        };
        let previous = self.workflow.clone();
        self.workflow.mode = WorkMode::Execute;
        match self.workflow.plan.as_mut() {
            // An unsaved revision-0 binding is simply cleared.
            Some(plan) if plan.revision == 0 => {
                self.workflow.plan = None;
                self.workflow.review = None;
            }
            Some(plan) => plan.status = PlanStatus::Abandoned,
            None => {}
        }
        let mut definitions = self.tools.all_definitions(WorkMode::Execute);
        definitions.extend(registry.definitions());
        self.session.set_tools(definitions);
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            self.session
                .set_tools(self.tools.all_definitions(WorkMode::Plan));
            return Err(self.abort_execution_preparation(registry, error));
        }
        self.tools.mcp = registry;
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: None,
            cleanup,
        })
    }

    pub(super) fn refresh_plan_progress(
        &mut self,
        emit: &mut dyn FnMut(crate::events::AgentEvent),
    ) -> Result<()> {
        if self.workflow.mode != WorkMode::Execute {
            return Ok(());
        }
        let Some(document) = self
            .workflow
            .plan
            .as_ref()
            .filter(|plan| plan.status == PlanStatus::Executed)
        else {
            return Ok(());
        };
        let content = self.tools.read_plan(&document.id).map_err(|error| {
            error.context(
                "Plan progress could not be refreshed; last synchronized progress retained.",
            )
        })?;
        if content == document.content {
            return Ok(());
        }
        let revision = document.next_revision()?;
        let document = self.workflow.plan.as_mut().expect("executed plan bound");
        let previous = std::mem::replace(&mut document.content, content);
        document.revision = revision;
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            let document = self.workflow.plan.as_mut().expect("executed plan bound");
            document.content = previous;
            document.revision -= 1;
            return Err(error.context(
                "Plan progress could not be refreshed; last synchronized progress retained.",
            ));
        }
        let document = self.workflow.plan.as_ref().expect("executed plan bound");
        emit(crate::events::AgentEvent::PlanProgressUpdated {
            plan_id: document.id.clone(),
            revision,
            content: document.content.clone(),
        });
        Ok(())
    }

    /// Verify a review token against the current session, plan revision,
    /// queue and exact on-disk bytes. Returns the on-disk content when it
    /// differs from the token; the caller decides whether that is an error
    /// (`validate_plan_token`) or an import before the same error
    /// (`check_plan_token_for_approval`) — no model request ever runs on a
    /// mismatch.
    fn plan_token_status(&self, token: &ReviewToken, input: &InputQueue) -> Result<Option<String>> {
        if let Some(error) = &self.plan_cleanup_error {
            bail!("{error}");
        }
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_CURRENT}");
        let document = self
            .workflow
            .plan
            .as_ref()
            .ok_or_else(|| anyhow!("{NOT_CURRENT}"))?;
        ensure!(
            document.status == PlanStatus::Ready
                && token.plan_id == document.id
                && token.revision == document.revision
                && token.content == document.content,
            "{NOT_CURRENT}"
        );
        self.journal.check()?;
        ensure!(
            self.journal.id() == Some(token.session_id.as_str()),
            "{NOT_CURRENT}"
        );
        ensure!(input.is_empty()?, "{PENDING}");
        let content = self.tools.read_plan(&document.id)?;
        Ok((content != token.content).then_some(content))
    }

    /// Shared by the isolated reviewer: a changed file is reported with the
    /// exact changed message so the caller refreshes via `read_plan`.
    pub(crate) fn validate_plan_token(
        &self,
        token: &ReviewToken,
        input: &InputQueue,
    ) -> Result<()> {
        ensure!(self.plan_token_status(token, input)?.is_none(), "{CHANGED}");
        Ok(())
    }

    /// Approval variant of the token check: a changed file is imported as the
    /// next revision before the exact changed message is returned, so the
    /// worker has regenerated the current token state and the UI only needs
    /// to read the refreshed snapshot.
    fn check_plan_token_for_approval(
        &mut self,
        token: &ReviewToken,
        input: &InputQueue,
    ) -> Result<()> {
        if let Some(content) = self.plan_token_status(token, input)? {
            let title = self
                .workflow
                .plan
                .as_ref()
                .expect("plan bound")
                .title
                .clone();
            self.import_plan_revision(content, &title)?;
            bail!("{CHANGED}");
        }
        Ok(())
    }

    /// Approve the reviewed plan and prepare one of the two execution
    /// handoffs. The candidate MCP registry is connected cancelably before
    /// any state commit, the token is re-verified afterwards, and on any
    /// failure the new registry is explicitly stopped and the workflow stays
    /// Plan/Ready — no model request ever starts from a partial handoff.
    pub(crate) fn approve_plan(
        &mut self,
        token: ReviewToken,
        context: ExecutionContext,
        input: &InputQueue,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_CURRENT}");
        // Finish any pending Plan cleanup before touching approval state.
        self.ensure_plan_mcp_shutdown(cancellation)?;
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
        let config = self
            .catalog
            .resolve(&self.current_model.reference)
            .map_err(|_| anyhow!("current model is unavailable"))?;
        let registry =
            Registry::connect(&config.settings.mcp_servers, self.tools.cwd(), cancellation)?;
        let cleanup = match registry.cleanup() {
            Ok(cleanup) => cleanup,
            Err(error) => return Err(self.abort_execution_preparation(registry, error)),
        };
        // Re-check queue and file after preparation; the approved token bytes
        // remain the execution snapshot.
        if let Err(error) = self.check_plan_token_for_approval(&token, input) {
            return Err(self.abort_execution_preparation(registry, error));
        }
        match context {
            ExecutionContext::Keep => self.prepare_keep_execution(&token, registry, cleanup),
            ExecutionContext::Fresh => self.prepare_fresh_execution(&token, registry, cleanup),
        }
    }

    /// Keep-context handoff: append exactly one execution user message to the
    /// current session and commit it together with the Execute workflow as
    /// one atomic journal group. Failure restores the Plan/Ready state and
    /// the native history; the plan file is never rolled back.
    fn prepare_keep_execution(
        &mut self,
        token: &ReviewToken,
        registry: Registry,
        cleanup: crate::mcp::Cleanup,
    ) -> Result<ExecutionPreparation> {
        let document = self
            .workflow
            .plan
            .as_ref()
            .expect("ready plan bound")
            .clone();
        debug_assert_eq!(document.content, token.content);
        let prompt = crate::plan::execution_prompt(&document, ExecutionContext::Keep);
        let previous_workflow = self.workflow.clone();
        let previous_system = SavedInstructions::take(&mut self.session);
        let previous_pending = self.pending_instructions.take();
        self.append_user(&prompt);
        self.workflow.mode = WorkMode::Execute;
        if let Some(plan) = &mut self.workflow.plan {
            plan.status = PlanStatus::Executed;
        }
        let mut definitions = self.tools.all_definitions(WorkMode::Execute);
        definitions.extend(registry.definitions());
        self.session.set_tools(definitions);
        let prepared = (|| -> Result<()> {
            // Switch the system prompt to the Execute progress rules before
            // the synced commit so the stored state matches the handoff.
            self.refresh_instructions()?;
            let metadata = Metadata {
                cwd: self.tools.cwd(),
                model_reference: &self.current_model.reference,
                think_level: self.think_level,
                edit_mode: self.tools.edit_mode(),
                tool_limits: self.tools.limits,
                base_system: &self.base_system,
            };
            self.journal.commit_plan_execution(
                &metadata,
                (&self.session).into(),
                &self.workflow,
                &prompt,
            )
        })();
        if let Err(error) = prepared {
            self.workflow = previous_workflow;
            self.remove_last_user();
            previous_system.restore(&mut self.session);
            self.pending_instructions = previous_pending;
            self.session
                .set_tools(self.tools.all_definitions(WorkMode::Plan));
            return Err(self.abort_execution_preparation(registry, error));
        }
        self.tools.mcp = registry;
        self.pending_instructions = None;
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: None,
            cleanup,
        })
    }

    /// Fresh-context handoff: prepare an empty current-model session, a blank
    /// journal fork and an Execute workflow carrying only the approved plan,
    /// then commit the target checkpoint and the source handoff record. Only
    /// when both commits succeed is the working session replaced; the
    /// planning session stays in Plan mode with an Executed (read-only) plan.
    fn prepare_fresh_execution(
        &mut self,
        token: &ReviewToken,
        registry: Registry,
        cleanup: crate::mcp::Cleanup,
    ) -> Result<ExecutionPreparation> {
        let document = self
            .workflow
            .plan
            .as_ref()
            .expect("ready plan bound")
            .clone();
        debug_assert_eq!(document.content, token.content);
        let candidate_workflow = WorkflowState {
            mode: WorkMode::Execute,
            plan: Some(PlanDocument {
                status: PlanStatus::Executed,
                ..document.clone()
            }),
            review: None,
        };
        let prompt = crate::plan::execution_prompt(&document, ExecutionContext::Fresh);
        let previous = self.workflow.clone();
        // Reload current project and skill rules; the candidate session reuses
        // the current model, thinking level and edit mode, never the planning
        // native history.
        let prepared = (|| -> Result<(Session, SessionJournal)> {
            let system = self.compose_system(&candidate_workflow)?;
            let mut session = Session::new(
                self.current_model.model.clone(),
                self.current_model.protocol,
                system,
                self.tools.edit_mode(),
            );
            session.set_think_level(self.think_level);
            let mut definitions = self.tools.all_definitions(WorkMode::Execute);
            definitions.extend(registry.definitions());
            session.set_tools(definitions);
            super::append_user_to(&mut session, &prompt);
            let metadata = Metadata {
                cwd: self.tools.cwd(),
                model_reference: &self.current_model.reference,
                think_level: self.think_level,
                edit_mode: self.tools.edit_mode(),
                tool_limits: self.tools.limits,
                base_system: &self.base_system,
            };
            let mut journal = self.journal.fork_empty();
            journal.commit_plan_execution(
                &metadata,
                (&session).into(),
                &candidate_workflow,
                &prompt,
            )?;
            Ok((session, journal))
        })();
        let (session, journal) = match prepared {
            Ok(pair) => pair,
            Err(error) => {
                // The candidate session file may remain in the session list;
                // the planning session is untouched and stays Plan/Ready.
                return Err(self.abort_execution_preparation(registry, error));
            }
        };
        // Source handoff record: plan executed, planning session stays in Plan
        // mode for read-only review until /plan new.
        if let Some(plan) = &mut self.workflow.plan {
            plan.status = PlanStatus::Executed;
        }
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(self.abort_execution_preparation(registry, error));
        }
        self.session = session;
        self.journal = journal;
        self.workflow = candidate_workflow;
        self.tools.clear_changes();
        self.last_request = None;
        self.pending_instructions = None;
        self.tools.mcp = registry;
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: Some(Vec::new()),
            cleanup,
        })
    }

    /// Resume a saved session in the worker, adapting the managed MCP
    /// registry to the target workflow mode through the same safe cleanup
    /// path: entering a Plan session stops the current registry first, and a
    /// Plan session never connects MCP at all.
    pub(crate) fn resume(
        &mut self,
        id: &str,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        let mut restored = self.prepare_resume(id)?;
        let target_mode = restored.checkpoint.workflow.mode;
        match (self.workflow.mode, target_mode) {
            (WorkMode::Plan, WorkMode::Plan) => {}
            (WorkMode::Plan, WorkMode::Execute) => {
                // The Plan registry is empty; finish any pending cleanup, then
                // connect ordinary execution MCP.
                self.ensure_plan_mcp_shutdown(cancellation)?;
                let config = self
                    .catalog
                    .resolve(&self.current_model.reference)
                    .map_err(|_| anyhow!("current model is unavailable"))?;
                self.tools.mcp = Registry::connect(
                    &config.settings.mcp_servers,
                    self.tools.cwd(),
                    cancellation,
                )?;
            }
            (WorkMode::Execute, WorkMode::Plan) => {
                // Stop the managed registry before the planning session can
                // rely on the Plan gate; failure aborts the resume.
                self.ensure_plan_mcp_shutdown(cancellation)?;
            }
            (WorkMode::Execute, WorkMode::Execute) => {}
        }
        let cleanup = self.tools.mcp.cleanup()?;
        restored.journal.history_replaced();
        self.session = restored.checkpoint.session;
        self.workflow = restored.checkpoint.workflow;
        self.session
            .set_tools(self.tools.all_definitions(self.workflow.mode));
        self.journal = restored.journal;
        self.replay = restored.replay;
        self.tools.clear_changes();
        if let Some(debug) = &mut self.debug {
            debug.set_prompt_recorded(true);
        }
        self.last_request = None;
        self.pending_instructions = None;
        Ok(ExecutionPreparation {
            workflow: self.workflow.clone(),
            session_id: self.journal.id().map(str::to_owned),
            replay: Some(std::mem::take(&mut self.replay)),
            cleanup,
        })
    }
}

/// True only when the plan file does not exist; used to keep the revision-0
/// "no saved plan" state distinct from real read failures.
fn plan_file_missing(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<rustix::io::Errno>()
            .is_some_and(|errno| *errno == rustix::io::Errno::NOENT)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::{
        EXPLORATION, ORIGIN, PLAN_CONTENT, err_message, journal_workflow, plan_workflow,
        runtime_input_agent, seed_session, write_plan_file,
    };
    use crate::config::{EditMode, ModelCatalog};
    use crate::tools::Tools;
    use serde_json::json;

    fn user_texts(agent: &Agent) -> Vec<String> {
        let native = serde_json::to_value(&agent.session).unwrap();
        let request = &native["request"];
        let entries = match request["messages"].as_array() {
            Some(messages) => messages.clone(),
            None => request["input"].as_array().unwrap().clone(),
        };
        entries
            .iter()
            .filter(|entry| entry["role"] == "user")
            .map(|entry| match &entry["content"] {
                serde_json::Value::String(text) => text.clone(),
                parts => parts
                    .as_array()
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<String>()
                    })
                    .unwrap_or_default(),
            })
            .collect()
    }

    fn session_tool_count(agent: &Agent) -> usize {
        serde_json::to_value(&agent.session).unwrap()["request"]["tools"]
            .as_array()
            .unwrap()
            .len()
    }

    fn plan_path(dir: &std::path::Path, id: &str) -> std::path::PathBuf {
        dir.join(".pp/plans").join(format!("{id}.md"))
    }

    #[test]
    fn plan_external_import_registers_revisions_and_respects_readonly_statuses() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Draft);
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);

            // Unchanged content keeps the revision and stays Draft.
            let review = agent.read_plan().unwrap();
            assert_eq!(review.document.revision, 1);
            assert_eq!(review.document.status, PlanStatus::Draft);
            assert_eq!(review.token.content, PLAN_CONTENT);
            assert_eq!(review.token.revision, 1);
            assert_eq!(review.token.session_id, agent.session_id().unwrap());

            // An external edit becomes the next revision and reaches Ready
            // only with a genuine task list.
            let edited = format!("{PLAN_CONTENT}\nextra context\n");
            write_plan_file(&agent, &edited);
            let review = agent.read_plan().unwrap();
            assert_eq!(review.document.revision, 2);
            assert_eq!(review.document.status, PlanStatus::Ready);
            assert_eq!(review.token.content, edited);

            // Missing task lists fall back to Draft.
            write_plan_file(&agent, "# Create hello\n\nno tasks here\n");
            let review = agent.read_plan().unwrap();
            assert_eq!(review.document.revision, 3);
            assert_eq!(review.document.status, PlanStatus::Draft);

            // A missing file is an error, never a journal fallback.
            let id = agent.workflow.plan.as_ref().unwrap().id.clone();
            std::fs::remove_file(dir.path().join(".pp/plans").join(format!("{id}.md"))).unwrap();
            assert!(agent.read_plan().is_err());
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().revision,
                3,
                "failed import never mutates the snapshot"
            );

            // Executed plans refresh their displayed snapshot (execution
            // progress) without regaining approval eligibility.
            let document = agent.workflow.plan.as_mut().unwrap();
            document.status = PlanStatus::Executed;
            write_plan_file(&agent, "- [ ] changed externally\n");
            let review = agent.read_plan().unwrap();
            assert_eq!(review.document.revision, 4);
            assert_eq!(review.document.status, PlanStatus::Executed);
            assert_eq!(review.document.content, "- [ ] changed externally\n");

            // A hand-written revision-0 file imports with the fixed title.
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, ORIGIN);
            agent.workflow = plan_workflow("", ORIGIN, PlanStatus::Draft);
            write_plan_file(&agent, PLAN_CONTENT);
            let review = agent.read_plan().unwrap();
            assert_eq!(review.document.revision, 1);
            assert_eq!(review.document.title, "Plan");
            assert_eq!(review.document.status, PlanStatus::Ready);

            // Without origin the review is refused before any import.
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow("", "", PlanStatus::Draft);
            write_plan_file(&agent, PLAN_CONTENT);
            let error = err_message(agent.read_plan());
            assert_eq!(error, "Enter the task before reviewing this plan.");

            // A revision-0 binding without a file reports no saved plan.
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, ORIGIN);
            agent.workflow = plan_workflow("", ORIGIN, PlanStatus::Draft);
            let error = err_message(agent.read_plan());
            assert_eq!(error, "No saved plan.");
        }
    }

    #[test]
    fn plan_token_validation_denies_stale_missing_and_pending() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let token = agent.read_plan().unwrap().token;
            let queue = crate::agent::input::InputQueue::default();
            agent.validate_plan_token(&token, &queue).unwrap();

            // Pending input denies approval without touching any state.
            queue.begin_run().unwrap();
            queue
                .enqueue(
                    crate::agent::input::PendingKind::Steering,
                    "late question".into(),
                    None,
                    false,
                )
                .unwrap();
            let error = agent
                .validate_plan_token(&token, &queue)
                .unwrap_err()
                .to_string();
            assert_eq!(error, "Apply pending input before approving the plan.");
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Ready
            );
            assert!(queue.claim_steering(false).unwrap().len() == 1);

            // Changed bytes report the exact changed message without a model
            // request; approval itself imports the new revision so the next
            // ReadPlan merely refreshes the token.
            let edited = format!("{PLAN_CONTENT}\nexternal\n");
            write_plan_file(&agent, &edited);
            let error = agent
                .validate_plan_token(&token, &queue)
                .unwrap_err()
                .to_string();
            assert_eq!(
                error,
                "Plan changed; review the new revision before executing."
            );
            assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, 1);
            let error = err_message(agent.approve_plan(
                token.clone(),
                ExecutionContext::Keep,
                &queue,
                &CancellationToken::new(),
            ));
            assert_eq!(
                error,
                "Plan changed; review the new revision before executing."
            );
            assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, 2);
            let review = agent.read_plan().unwrap();
            assert_eq!(review.document.revision, 2);
            agent.validate_plan_token(&review.token, &queue).unwrap();

            // Stale session and stale revision tokens are no longer current.
            let mut stale = review.token.clone();
            stale.session_id = "other-session".into();
            assert_eq!(
                agent
                    .validate_plan_token(&stale, &queue)
                    .unwrap_err()
                    .to_string(),
                NOT_CURRENT
            );
            let mut stale = review.token.clone();
            stale.revision = 1;
            assert_eq!(
                agent
                    .validate_plan_token(&stale, &queue)
                    .unwrap_err()
                    .to_string(),
                NOT_CURRENT
            );

            // A deleted file blocks approval instead of using the snapshot.
            let id = agent.workflow.plan.as_ref().unwrap().id.clone();
            std::fs::remove_file(dir.path().join(".pp/plans").join(format!("{id}.md"))).unwrap();
            assert!(agent.validate_plan_token(&review.token, &queue).is_err());
            assert_eq!(agent.workflow.plan.as_ref().unwrap().revision, 2);

            // The cleanup gate blocks validation and ordinary tasks alike.
            agent.plan_cleanup_error = Some(
                "MCP servers could not be stopped for Plan mode: boom; run /plan again to retry."
                    .into(),
            );
            let error = agent
                .validate_plan_token(&review.token, &queue)
                .unwrap_err()
                .to_string();
            assert!(error.contains("run /plan again to retry"));
            let outcome = agent.run(
                "research task",
                &CancellationToken::new(),
                None,
                &mut |_| {},
            );
            assert_eq!(
                outcome.unwrap_err().to_string(),
                "MCP servers could not be stopped for Plan mode: boom; run /plan again to retry."
            );

            // A successful retry through /plan clears the gate.
            agent.enter_plan(&CancellationToken::new()).unwrap();
            assert!(agent.plan_cleanup_error.is_none());
        }
    }

    #[test]
    fn plan_enter_persists_authority_before_mcp_and_new_keeps_old_binding() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            seed_session(&mut agent, EXPLORATION);
            let session_id = agent.session_id().unwrap().to_owned();

            let workflow = agent.enter_plan(&CancellationToken::new()).unwrap();
            assert_eq!(workflow.mode, WorkMode::Plan);
            let document = workflow.plan.as_ref().unwrap();
            assert_eq!(document.revision, 0);
            assert_eq!(document.status, PlanStatus::Draft);
            assert_eq!(journal_workflow(dir.path(), &session_id), workflow);

            // Another plan keeps the old binding's file untouched.
            let old_id = document.id.clone();
            let workflow = agent.new_plan(&CancellationToken::new()).unwrap();
            let new_id = workflow.plan.as_ref().unwrap().id.clone();
            assert_ne!(old_id, new_id);
            assert!(
                dir.path()
                    .join(".pp/plans")
                    .join(format!("{old_id}.md"))
                    .exists()
                    || {
                        // Nothing was saved yet; the old binding simply disappears.
                        !dir.path().join(".pp/plans").exists()
                    }
            );
            assert_eq!(journal_workflow(dir.path(), &session_id), workflow);

            // Leaving with an unsaved binding clears it; replay stays None.
            let preparation = agent.leave_plan(&CancellationToken::new()).unwrap();
            assert_eq!(preparation.workflow.mode, WorkMode::Execute);
            assert!(preparation.workflow.plan.is_none());
            assert!(preparation.replay.is_none());
            assert_eq!(preparation.session_id.as_deref(), Some(session_id.as_str()));
            assert_eq!(
                journal_workflow(dir.path(), &session_id),
                preparation.workflow
            );

            // Leaving in Execute mode is refused.
            assert_eq!(
                err_message(agent.leave_plan(&CancellationToken::new())),
                NOT_PLAN
            );
        }
    }

    #[test]
    fn plan_keep_approval_appends_one_message_and_rolls_back_on_failure() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let session_id = agent.session_id().unwrap().to_owned();
            let token = agent.read_plan().unwrap().token;

            let preparation = agent
                .approve_plan(
                    token.clone(),
                    ExecutionContext::Keep,
                    &crate::agent::input::InputQueue::default(),
                    &CancellationToken::new(),
                )
                .unwrap();
            assert_eq!(preparation.workflow.mode, WorkMode::Execute);
            assert_eq!(
                preparation.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Executed
            );
            assert!(preparation.workflow.review.is_none());
            assert!(preparation.replay.is_none());
            assert_eq!(preparation.session_id.as_deref(), Some(session_id.as_str()));

            // Exactly one execution message was appended, containing the
            // origin and the full approved plan.
            let texts = user_texts(&agent);
            assert_eq!(texts.len(), 2);
            assert_eq!(texts[0], EXPLORATION);
            assert!(texts[1].contains(ORIGIN));
            assert!(texts[1].contains(PLAN_CONTENT));
            // Tool definitions and system instructions follow Execute.
            assert_eq!(
                session_tool_count(&agent),
                agent.tools.all_definitions(WorkMode::Execute).len()
            );
            let native = serde_json::to_value(&agent.session).unwrap();
            let instructions = match &native["request"]["instructions"] {
                serde_json::Value::String(text) => text.clone(),
                _ => native["request"]["messages"][0]["content"]
                    .as_str()
                    .unwrap()
                    .to_owned(),
            };
            assert!(instructions.contains("EXECUTE MODE"));
            assert!(!instructions.contains("PLAN MODE"));
            // The persisted state matches the handoff.
            assert_eq!(
                journal_workflow(dir.path(), &session_id),
                preparation.workflow
            );

            // A second approval is refused; the session token is spent.
            let error = err_message(agent.approve_plan(
                token,
                ExecutionContext::Keep,
                &crate::agent::input::InputQueue::default(),
                &CancellationToken::new(),
            ));
            assert_eq!(error, NOT_CURRENT);
            assert_eq!(user_texts(&agent).len(), 2);
            drop(preparation);

            // A journal write failure rolls back the whole handoff.
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let token = agent.read_plan().unwrap().token;
            let id = agent.session_id().unwrap().to_owned();
            let path = dir.path().join("sessions").join(format!("{id}.jsonl"));
            agent.journal.use_read_only_file(&path);
            let error = err_message(agent.approve_plan(
                token,
                ExecutionContext::Keep,
                &crate::agent::input::InputQueue::default(),
                &CancellationToken::new(),
            ));
            assert!(error.contains("persisting session"));
            assert_eq!(agent.workflow.mode, WorkMode::Plan);
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Ready
            );
            assert_eq!(user_texts(&agent), vec![EXPLORATION.to_owned()]);
            let id = agent.workflow.plan.as_ref().unwrap().id.clone();
            assert_eq!(
                std::fs::read(plan_path(dir.path(), &id)).unwrap(),
                PLAN_CONTENT.as_bytes()
            );
        }
    }

    #[test]
    fn plan_fresh_approval_starts_an_empty_session_and_keeps_source_readable() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            agent.workflow.review = Some(crate::plan::PlanReviewReport {
                plan_id: agent.workflow.plan.as_ref().unwrap().id.clone(),
                revision: 1,
                model_reference: "fixture/one".into(),
                content: "- [ ] review finding\n".into(),
            });
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let planning_id = agent.session_id().unwrap().to_owned();
            let token = agent.read_plan().unwrap().token;

            let preparation = agent
                .approve_plan(
                    token,
                    ExecutionContext::Fresh,
                    &crate::agent::input::InputQueue::default(),
                    &CancellationToken::new(),
                )
                .unwrap();
            // A fresh session with an empty replay was handed over.
            assert!(matches!(&preparation.replay, Some(replay) if replay.is_empty()));
            let execution_id = preparation.session_id.clone().unwrap();
            assert_ne!(execution_id, planning_id);
            assert_eq!(preparation.workflow.mode, WorkMode::Execute);
            assert_eq!(
                preparation.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Executed
            );
            assert!(preparation.workflow.review.is_none());

            // The fresh history carries only the execution prompt: no origin,
            // no planning chat, no review report.
            let texts = user_texts(&agent);
            assert_eq!(texts.len(), 1);
            assert!(texts[0].contains(PLAN_CONTENT));
            assert!(!texts[0].contains("ORIGIN-MARKER"));
            assert!(!texts[0].contains("EXPLORATION-MARKER"));
            assert!(!texts[0].contains("review finding"));
            assert_eq!(
                session_tool_count(&agent),
                agent.tools.all_definitions(WorkMode::Execute).len()
            );

            // Both journals exist: the planning session stays in Plan mode
            // with an Executed, read-only plan binding.
            let source = journal_workflow(dir.path(), &planning_id);
            assert_eq!(source.mode, WorkMode::Plan);
            assert_eq!(source.plan.as_ref().unwrap().status, PlanStatus::Executed);
            assert_eq!(
                journal_workflow(dir.path(), &execution_id),
                preparation.workflow
            );
            drop(preparation);

            // Source handoff failure keeps the planning session intact.
            let mut agent = runtime_input_agent(dir.path(), protocol, "127.0.0.1:1", "");
            agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            write_plan_file(&agent, PLAN_CONTENT);
            seed_session(&mut agent, EXPLORATION);
            let token = agent.read_plan().unwrap().token;
            let id = agent.session_id().unwrap().to_owned();
            let path = dir.path().join("sessions").join(format!("{id}.jsonl"));
            agent.journal.use_read_only_file(&path);
            let error = err_message(agent.approve_plan(
                token,
                ExecutionContext::Fresh,
                &crate::agent::input::InputQueue::default(),
                &CancellationToken::new(),
            ));
            assert!(error.contains("persisting session"));
            assert_eq!(agent.workflow.mode, WorkMode::Plan);
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Ready
            );
            assert_eq!(user_texts(&agent), vec![EXPLORATION.to_owned()]);
            let id = agent.workflow.plan.as_ref().unwrap().id.clone();
            assert_eq!(
                std::fs::read(plan_path(dir.path(), &id)).unwrap(),
                PLAN_CONTENT.as_bytes()
            );
        }
    }

    #[test]
    fn plan_progress_refresh_keeps_verified_snapshot_when_file_becomes_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let mut agent = runtime_input_agent(dir.path(), "completion", "127.0.0.1:1", "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Executed);
        agent.workflow.mode = WorkMode::Execute;
        write_plan_file(&agent, PLAN_CONTENT);
        seed_session(&mut agent, EXPLORATION);
        let progress = PLAN_CONTENT.replace("- [ ]", "- [x]");
        write_plan_file(&agent, &progress);
        let mut published = Vec::new();
        agent
            .refresh_plan_progress(&mut |event| published.push(event))
            .unwrap();
        assert!(
            matches!(published.as_slice(), [crate::events::AgentEvent::PlanProgressUpdated { revision: 2, content, .. }] if content == &progress)
        );
        let previous = agent.workflow.clone();
        let path = agent
            .tools
            .cwd()
            .join(agent.workflow.plan.as_ref().unwrap().path());
        std::fs::remove_file(&path).unwrap();
        published.clear();
        let error = agent
            .refresh_plan_progress(&mut |event| published.push(event))
            .unwrap_err();
        assert!(format!("{error:#}").contains("last synchronized progress retained"));
        assert_eq!(agent.workflow, previous);
        assert!(published.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn plan_resume_adapts_mcp_and_reports_replay() {
        // prepare_resume loads from the global sessions directory, so the
        // test re-executes itself with HOME pointed at a temporary home.
        const FIXTURE: &str = "PP_TEST_PLAN_RESUME";
        let Ok(root) = std::env::var(FIXTURE) else {
            let home = tempfile::tempdir().unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "agent::plan_workflow::tests::plan_resume_adapts_mcp_and_reports_replay",
                    "--nocapture",
                ])
                .env("HOME", home.path())
                .env(FIXTURE, home.path())
                .status()
                .unwrap();
            assert!(status.success());
            return;
        };
        let root = std::path::Path::new(&root);
        let global = root.join(".pp/agent");
        let sessions = global.join("sessions");
        std::fs::create_dir_all(&global).unwrap();
        for protocol in ["completion", "response"] {
            // A saved Plan-mode session and a saved Execute-mode session.
            let mut planner = fixture_agent(&root.join("plan"), &sessions, protocol);
            planner.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
            seed_session(&mut planner, EXPLORATION);
            let planning_id = planner.session_id().unwrap().to_owned();
            let planning_session = serde_json::to_value(&planner.session).unwrap();
            drop(planner);

            let mut executor = fixture_agent(&root.join("exec"), &sessions, protocol);
            seed_session(&mut executor, "ordinary work");
            let execute_id = executor.session_id().unwrap().to_owned();
            drop(executor);

            // Resuming a Plan session from Execute stops the managed registry
            // before the planning session takes over.
            let mut agent = fixture_agent(&root.join("current"), &sessions, protocol);
            seed_session(&mut agent, "current");
            let preparation = agent
                .resume(&planning_id, &CancellationToken::new())
                .unwrap();
            assert_eq!(preparation.workflow.mode, WorkMode::Plan);
            assert_eq!(
                preparation.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Ready
            );
            assert_eq!(
                preparation.session_id.as_deref(),
                Some(planning_id.as_str())
            );
            assert!(preparation.replay.is_some());
            // The history matches; the system prompt is freshly composed for
            // the current working directory and Plan mode.
            let mut native = serde_json::to_value(&agent.session).unwrap();
            match protocol {
                "response" => {
                    native["request"]["instructions"] =
                        planning_session["request"]["instructions"].clone();
                }
                _ => {
                    native["request"]["messages"][0]["content"] =
                        planning_session["request"]["messages"][0]["content"].clone();
                }
            }
            assert_eq!(native, planning_session);

            // Resuming an Execute session from Plan reconnects ordinary MCP.
            let preparation = agent
                .resume(&execute_id, &CancellationToken::new())
                .unwrap();
            assert_eq!(preparation.workflow.mode, WorkMode::Execute);
            assert!(preparation.workflow.plan.is_none());
            assert_eq!(
                session_tool_count(&agent),
                agent.tools.all_definitions(WorkMode::Execute).len()
            );
        }
    }

    /// An agent journaled into the fixture's global sessions directory, as
    /// production does, with its own working directory.
    fn fixture_agent(cwd: &std::path::Path, sessions: &std::path::Path, protocol: &str) -> Agent {
        std::fs::create_dir_all(cwd).unwrap();
        let global = sessions.parent().unwrap();
        let settings = global.join("settings.yml");
        let models = global.join("models.yml");
        std::fs::write(&settings, "default_model: fixture/one\n").unwrap();
        std::fs::write(
            &models,
            json!({"providers":{"fixture": { "api_key":"fixture-key", "protocol":protocol,
                "models":{"one": {"model":"one"}}
            }}})
            .to_string(),
        )
        .unwrap();
        Agent::new(
            ModelCatalog::from_files(&settings, &models).unwrap(),
            Tools::new(cwd.to_owned(), EditMode::Replace).unwrap(),
            sessions.to_owned(),
        )
        .unwrap()
    }
}
