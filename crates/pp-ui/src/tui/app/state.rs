use crate::tui::{
    App, CancellationToken, ExecutionContext, Instant, Level, ModelChoice, PlanCommand,
    PlanEnterAction, PlanReview, PlanStatus, ReviewToken, State, ThinkLevel, Transcript, WorkMode,
    WorkerCommand, WorkflowState, editor, plan, plan_leave,
};

impl App {
    /// Reinstalling for the same Registry only transfers cleanup ownership; when the
    /// Registry changes, the old guard is dropped and reaped on assignment.
    pub(in crate::tui) fn install_lsp_cleanup(&mut self, cleanup: pp_lsp::Cleanup) {
        if let Some(previous) = &mut self.lsp_cleanup {
            previous.supersede(&cleanup);
        }
        self.lsp_cleanup = Some(cleanup);
    }

    pub(in crate::tui) fn notify(&mut self, level: Level, message: String) {
        if level != Level::Error || message.trim().is_empty() {
            return;
        }
        self.show_welcome = false;
        self.conversation.append_error(&message);
        if let Some(review) = &mut self.review_transcript {
            review.append_error(&message);
        }
        // Full-screen panels keep their draft and selection; failures are reported only in
        // the existing hint area.
        if let Some(panel) = &mut self.session_panel {
            panel.set_feedback(message.clone());
        }
        if let Some(panel) = &mut self.tree_panel {
            panel.set_error(message.clone());
        }
        if let Some(panel) = &mut self.context_panel {
            panel.set_error(message.clone());
        }
        if let Some(panel) = &mut self.settings_panel {
            panel.set_error(message.clone());
        }
        if let Some(panel) = &mut self.roles_panel {
            panel.runtime_status(message.clone());
        }
        if let Some(panel) = &mut self.plan_panel {
            panel.set_error(message.clone());
        }
        if let Some(panel) = &mut self.themes_panel {
            panel.set_error(message.clone());
        }
        if let Some(panel) = &mut self.skill_panel {
            panel.set_error(message);
        }
    }

    pub(in crate::tui) fn append(&mut self, label: &str, text: &str) {
        self.show_welcome = false;
        self.conversation.append(label, text);
    }

    /// Initializes the model fields when adopting a startup resume, then takes over the
    /// visible conversation.
    pub(in crate::tui) fn adopt_session(
        &mut self,
        replay: Vec<pp_runtime::agent::persistence::ReplayRecord>,
        choices: Vec<ModelChoice>,
        current_model: ModelChoice,
        think_level: ThinkLevel,
        default_think_level: ThinkLevel,
    ) -> Option<String> {
        self.choices = choices;
        self.current_model = Some(current_model);
        self.think_level = think_level;
        self.default_think_level = default_think_level;
        self.adopt_history(replay)
    }

    pub(in crate::tui) fn adopt_history(
        &mut self,
        replay: Vec<pp_runtime::agent::persistence::ReplayRecord>,
    ) -> Option<String> {
        self.close_ask_on_exit();
        self.subagent_autowake = false;
        self.token_speed.clear();
        self.retry_finished();
        self.task_status = self.task_status_view.snapshot();
        self.history_browser = false;
        self.todo_panel.reset_view();
        self.conversation = self.conversation.fresh();
        // Persistent input recall is decoupled from the visible session: resume and tree
        // navigation neither write input records nor clear recall.
        let error = self.conversation.replay(replay);
        self.resources.cancel();
        self.resources.set_images(Vec::new());
        self.resources.ready = None;
        self.input = editor();
        self.shell_mode = false;
        self.history_index = None;
        self.history_draft = None;
        self.history_waiting_up = false;
        self.model_picker_draft = None;
        self.model_panel = None;
        self.settings_panel = None;
        self.roles_panel = None;
        self.context_panel = None;
        self.file_paths.clear();
        self.file_directory.clear();
        self.file_query = None;
        self.menu = None;
        self.menu_dismissed = true;
        self.show_welcome = false;
        self.refresh_candidates(false);
        // Unbind the old observation epoch after a root/history switch; drafts and control
        // ACKs of the same root are kept.
        self.hub_sync_root();
        error
    }

    pub(in crate::tui) fn fail_worker(&mut self) {
        self.close_ask_on_exit();
        self.retry_finished();
        if self.token_speed.is_active() {
            self.token_speed.clear();
        }
        let message = "Execution unavailable: agent worker channel closed; restart pp.";
        self.state = State::Unavailable;
        self.queue_paused = true;
        let _ = self.input_queue.close();
        self.active_cancellation = None;
        self.secondary_status = None;
        // The desired state has no consumer left: the worker is gone, so effort stays frozen
        // at the last applied value.
        self.pending_think = None;
        self.commands = None;
        self.events = None;
        self.unavailable = Some(message.into());
        self.tree_panel = None;
        self.context_panel = None;
        self.roles_panel = None;
        self.plan_list_panel = None;
        self.plan_list_pending = None;
        self.plan_reenter_standby = false;
        self.plan_leave_confirmation = None;
        self.plan_panel = None;
        self.reopen_plan_after_discussion = false;
        self.plan_after_enter = None;
        self.review_transcript = None;
        self.plan_review_pending = None;
        self.escape_tap.reset();
        // With the worker dead there is no resume reply to wait for; the session panel must
        // not keep swallowing keys.
        self.session_panel = None;
        self.close_model_panel();
        self.conversation.fail();
        self.notify(Level::Error, message.into());
    }

    /// Applies a workflow snapshot published by the worker; progress is recomputed only
    /// when the body changes.
    pub(in crate::tui) fn set_workflow(&mut self, workflow: WorkflowState) {
        if workflow.mode != WorkMode::Plan || workflow.plan.is_none() {
            self.reopen_plan_after_discussion = false;
        }
        if let Some(document) = &workflow.plan {
            self.update_plan_progress_cache(document);
        } else {
            self.plan_panel = None;
            self.plan_progress = (0, 0);
        }
        self.todo_panel
            .update(pp_core::events::TodoState::from_workflow(&workflow));
        self.workflow = workflow;
        if let (Some(panel), Some(document)) = (&mut self.plan_panel, &self.workflow.plan) {
            panel.sync_document(document);
        }
    }

    /// Progress (completed, total) is recomputed only when the body changes; every caller
    /// runs before writing `document` into `self.workflow.plan`, so it compares directly
    /// against the current binding.
    pub(in crate::tui) fn update_plan_progress_cache(
        &mut self,
        document: &pp_core::plan::PlanDocument,
    ) {
        if self
            .workflow
            .plan
            .as_ref()
            .is_none_or(|plan| plan.content != document.content)
        {
            self.plan_progress = pp_core::plan::task_progress(&document.content);
        }
    }

    /// Execution-time progress event: updates the matching plan snapshot and the read-only
    /// panel without appending to Transcript.
    /// Body revision/content changes recompute progress; name/position/status fields missing
    /// from an old event keep the receiver's current values, while pure state changes such as
    /// archiving take effect immediately even when the body is unchanged.
    pub(in crate::tui) fn apply_plan_progress(
        &mut self,
        plan_id: &str,
        revision: u64,
        content: String,
        file_name: Option<String>,
        location: Option<pp_core::plan::PlanLocation>,
        status: Option<pp_core::plan::PlanStatus>,
    ) {
        let Some(document) = self.workflow.plan.as_ref() else {
            return;
        };
        if document.id != plan_id {
            return;
        }
        let body_changed = document.revision != revision || document.content != content;
        let state_changed = file_name
            .as_ref()
            .is_some_and(|file_name| Some(file_name) != document.file_name.as_ref())
            || location.is_some_and(|location| location != document.location)
            || status.is_some_and(|status| status != document.status);
        if !body_changed && !state_changed {
            return;
        }
        let mut document = self.workflow.plan.take().expect("plan checked above");
        document.revision = revision;
        document.content = content;
        if let Some(file_name) = file_name {
            document.file_name = Some(file_name);
        }
        if let Some(location) = location {
            document.location = location;
        }
        if let Some(status) = status {
            document.status = status;
        }
        self.update_plan_progress_cache(&document);
        if let Some(panel) = &mut self.plan_panel {
            panel.set_progress(&document);
        }
        self.workflow.plan = Some(document);
    }

    pub(in crate::tui) fn open_plan_panel(&mut self, review: PlanReview) {
        // ReadPlan's registry result is the authoritative snapshot; sync the document and
        // report fields in the workflow.
        let PlanReview {
            token: _,
            document,
            report,
        } = &review;
        self.update_plan_progress_cache(document);
        self.workflow.plan = Some(document.clone());
        self.workflow.review = report.clone();
        match &mut self.plan_panel {
            Some(panel) => panel.set_review(review),
            None => self.plan_panel = Some(plan::Panel::review(review)),
        }
        self.menu = None;
        self.menu_dismissed = true;
        self.file_query = None;
    }

    /// Read-only progress view while Execute runs: uses the last synchronized body already
    /// cached by the UI and sends no worker command.
    pub(in crate::tui) fn open_plan_progress_view(&mut self) {
        let Some(document) = self.workflow.plan.clone() else {
            self.notify(Level::Normal, "No saved plan.".into());
            return;
        };
        if document.content.is_empty() {
            self.notify(Level::Normal, "No saved plan.".into());
            return;
        }
        match &mut self.plan_panel {
            Some(panel) if panel.document().id == document.id => panel.set_progress(&document),
            _ => self.plan_panel = Some(plan::Panel::progress(document)),
        }
    }

    pub(in crate::tui) fn request_plan_read(&mut self) {
        if !matches!(self.state, State::Ready) {
            return;
        }
        self.send(WorkerCommand::ReadPlan, State::ReadingPlan);
    }

    pub(in crate::tui) fn append_execution_echo(&mut self) {
        let text = match &self.workflow.plan {
            Some(plan) => {
                let path = plan.path().unwrap_or_else(|| "Not saved yet".to_owned());
                format!("Execute the approved plan {path} (r{}).", plan.revision)
            }
            None => "Execute the approved plan.".to_owned(),
        };
        self.append("You", &text);
    }

    /// Mode switching/approval is allowed only in Ready with no pending work and no other
    /// modal; while busy it reports an explicit error and does not queue.
    pub(in crate::tui) fn plan_transition_ready(&mut self, pending_message: &str) -> bool {
        if !matches!(self.state, State::Ready) {
            self.notify(
                Level::Error,
                "Cancel the current task before changing Plan mode.".into(),
            );
            return false;
        }
        if self.has_pending() {
            self.notify(Level::Error, pending_message.into());
            return false;
        }
        if self.ask_panel.is_some()
            || self.permission_overlay.is_some()
            || self.skill_panel.is_some()
            || self.skills_pending
            || self.session_panel.is_some()
            || self.tree_panel.is_some()
            || self.context_panel.is_some()
            || self.plan_list_panel.is_some()
            || self.model_panel.is_some()
            || self.settings_panel.is_some()
            || self.roles_panel.is_some()
            || self.themes_panel.is_some()
            || self.queue_menu
            || self.plan_leave_confirmation.is_some()
        {
            self.notify(
                Level::Error,
                "Close the open panel before changing Plan mode.".into(),
            );
            return false;
        }
        true
    }

    /// Shared entry point for Plan mode switching: on a successful send it shows the
    /// matching progress text, whose wording the caller supplies per enter/leave direction.
    pub(in crate::tui) fn start_plan_transition(
        &mut self,
        command: impl FnOnce(CancellationToken) -> WorkerCommand,
        state: State,
        status: &'static str,
    ) {
        let cancellation = CancellationToken::new();
        if self.send(command(cancellation.clone()), state) {
            self.active_cancellation = Some(cancellation);
            self.secondary_status = Some(status.into());
        }
    }

    pub(in crate::tui) fn apply_plan_command(&mut self, command: PlanCommand<'_>) {
        const PENDING_MODE: &str = "Apply pending input before changing Plan mode.";
        match command {
            // Bare /plan toggle: Execute enters Plan standby; already in Plan exits and
            // unbinds.
            PlanCommand::Toggle => {
                if !self.plan_transition_ready(PENDING_MODE) {
                    return;
                }
                if self.workflow.mode == WorkMode::Execute || self.plan_retry_enter {
                    // Execute entering Plan, or retrying entry after the previous attempt
                    // failed.
                    self.plan_after_enter = Some(PlanEnterAction::Standby);
                    self.start_plan_transition(
                        |cancellation| WorkerCommand::EnterPlan { cancellation },
                        State::ChangingPlan,
                        "Entering Plan mode...",
                    );
                    return;
                }
                self.request_plan_leave(false);
            }
            PlanCommand::Prompt(prompt) => {
                if !self.plan_transition_ready(PENDING_MODE) {
                    return;
                }
                self.plan_after_enter = Some(PlanEnterAction::Prompt(
                    prompt.to_owned(),
                    self.resources.ready.take(),
                ));
                self.start_plan_transition(
                    |cancellation| WorkerCommand::EnterPlan { cancellation },
                    State::ChangingPlan,
                    "Entering Plan mode...",
                );
            }
            PlanCommand::View => {
                if matches!(self.state, State::Running | State::Cancelling) {
                    // Special case while running: read-only progress view, no blocking read
                    // sent to the executing worker.
                    self.open_plan_progress_view();
                    return;
                }
                if !self.plan_transition_ready(PENDING_MODE) {
                    return;
                }
                if self.has_saved_plan() {
                    self.request_plan_read();
                } else {
                    self.notify(Level::Normal, "No saved plan.".into());
                }
            }
        }
    }

    /// A plan that was never saved once must be abandoned explicitly; the confirmation
    /// applies only to the current binding.
    pub(in crate::tui) fn request_plan_leave(&mut self, reenter: bool) {
        if let Some(document) = &self.workflow.plan
            && document.revision == 0
            && !document.origin_prompt.trim().is_empty()
        {
            self.plan_leave_confirmation =
                Some(plan_leave::Panel::new(document.id.clone(), reenter));
            self.menu = None;
            self.file_query = None;
            return;
        }
        self.start_plan_leave(None, reenter);
    }

    pub(in crate::tui) fn start_plan_leave(
        &mut self,
        discard_plan_id: Option<String>,
        reenter: bool,
    ) {
        if reenter {
            self.close_plan_list();
        }
        self.plan_reenter_standby = reenter;
        self.start_plan_transition(
            |cancellation| WorkerCommand::LeavePlan {
                cancellation,
                discard_plan_id,
            },
            State::ChangingPlan,
            "Leaving Plan mode...",
        );
    }

    pub(in crate::tui) fn apply_plan_leave_action(&mut self, action: plan_leave::Action) {
        match action {
            plan_leave::Action::None => {}
            plan_leave::Action::Cancel => {
                self.plan_leave_confirmation = None;
            }
            plan_leave::Action::Discard => {
                let Some(panel) = self.plan_leave_confirmation.take() else {
                    return;
                };
                if !matches!(self.state, State::Ready) || self.has_pending() {
                    self.notify(
                        Level::Error,
                        "Finish the current task and pending input before leaving Plan mode."
                            .into(),
                    );
                    return;
                }
                if self.workflow.mode != WorkMode::Plan
                    || self
                        .workflow
                        .plan
                        .as_ref()
                        .is_none_or(|plan| plan.id != panel.plan_id || plan.revision != 0)
                {
                    self.notify(Level::Error, "Plan changed; confirm leaving again.".into());
                    return;
                }
                self.start_plan_leave(Some(panel.plan_id), panel.reenter);
            }
        }
    }

    /// Precondition shared by approval/review: Ready, no pending work, document Ready and a
    /// held token, with the input queue paused; on success returns the token and the
    /// cancellation token for this operation.
    pub(in crate::tui) fn plan_action_guard(
        &mut self,
        action: &str,
        ready_only: bool,
    ) -> Option<(ReviewToken, CancellationToken)> {
        if !matches!(self.state, State::Ready) {
            self.notify(
                Level::Error,
                format!("Cancel the current task before {action} the plan."),
            );
            return None;
        }
        if self.has_pending() {
            self.notify(
                Level::Error,
                format!("Apply pending input before {action} the plan."),
            );
            return None;
        }
        let token = {
            let panel = self.plan_panel.as_ref()?;
            if ready_only && panel.document().status != PlanStatus::Ready {
                return None;
            }
            panel.token().cloned()?
        };
        if let Err(error) = self.input_queue.begin_run() {
            self.queue_paused = true;
            self.notify(Level::Error, error.to_string());
            return None;
        }
        Some((token, CancellationToken::new()))
    }

    /// Approval action: sends only in Ready with no pending work and a Ready document;
    /// failures are safely refreshed through ReadPlan.
    pub(in crate::tui) fn approve_plan(&mut self, context: ExecutionContext) {
        let Some((token, cancellation)) = self.plan_action_guard("executing", true) else {
            return;
        };
        if self.send(
            WorkerCommand::ApprovePlan {
                token,
                context,
                cancellation: cancellation.clone(),
                input: self.input_queue.clone(),
            },
            State::PreparingExecution,
        ) {
            self.active_cancellation = Some(cancellation);
            self.secondary_status = Some(
                if context == ExecutionContext::Compact {
                    "Compacting context before execution..."
                } else {
                    "Preparing execution..."
                }
                .into(),
            );
        } else {
            let _ = self.input_queue.close();
        }
    }

    pub(in crate::tui) fn save_plan_and_leave(&mut self) {
        let Some((token, cancellation)) = self.plan_action_guard("saving", false) else {
            return;
        };
        if self.send(
            WorkerCommand::SavePlanAndLeave {
                token,
                cancellation: cancellation.clone(),
                input: self.input_queue.clone(),
            },
            State::ChangingPlan,
        ) {
            self.active_cancellation = Some(cancellation);
            // Leaving Plan needs safe cleanup and flushing to disk, so show visible progress
            // meanwhile; the WorkflowChanged / ExecutionPrepared branches clear it.
            self.secondary_status = Some("Saving plan...".into());
        } else {
            let _ = self.input_queue.close();
        }
    }

    /// Item 4: the standalone context reviewer. Sends only in Ready with no pending work and
    /// a Ready document; progress goes only to the standalone review transcript, and on
    /// completion or cancellation a safe ReadPlan refresh returns to the menu.
    pub(in crate::tui) fn review_plan(&mut self) {
        let Some((token, cancellation)) = self.plan_action_guard("reviewing", true) else {
            return;
        };
        if self.send(
            WorkerCommand::ReviewPlan {
                token,
                cancellation: cancellation.clone(),
                input: self.input_queue.clone(),
            },
            State::ReviewingPlan,
        ) {
            self.active_cancellation = Some(cancellation);
            // The standalone review tab replaces the review panel; the main draft, history and
            // session stay unchanged.
            self.plan_panel = None;
            self.plan_list_panel = None;
            self.plan_list_pending = None;
            self.menu_dismissed = true;
            self.secondary_status = Some("Reviewing plan...".into());
            let mut review = self.conversation.fresh();
            review.begin_run();
            self.review_transcript = Some(review);
        } else {
            let _ = self.input_queue.close();
        }
    }

    pub(in crate::tui) fn review_transcript_tick(&mut self, now: Instant) -> bool {
        self.review_transcript
            .as_mut()
            .is_some_and(|review| review.tick(now))
    }

    /// A plan with at least one registered revision: the precondition for /plan to open
    /// review.
    pub(in crate::tui) fn has_saved_plan(&self) -> bool {
        self.workflow
            .plan
            .as_ref()
            .is_some_and(|plan| plan.revision >= 1)
    }

    /// Plan/Ready: a plan awaiting review suspends automatic dispatch and needs an explicit
    /// review or an unpaused queue to continue.
    pub(in crate::tui) fn plan_awaiting_review(&self) -> bool {
        self.workflow.mode == WorkMode::Plan
            && self
                .workflow
                .plan
                .as_ref()
                .is_some_and(|plan| plan.status == PlanStatus::Ready)
    }

    /// The currently displayed transcript: the standalone review tab wins over the main
    /// conversation.
    pub(in crate::tui) fn needs_alternate(&self) -> bool {
        self.mcp_panel.is_some()
            || self.auth_panel.is_some()
            || self.settings_panel.is_some()
            || self.roles_panel.is_some()
            || self.subagent_panel.is_some()
            || self.history_browser
            || self.themes_panel.is_some()
            || self.skill_panel.is_some()
            || self.skills_pending
            || self.session_panel.is_some()
            || self.context_panel.is_some()
            // Keep the main screen before the read completes; no screen switch is needed when
            // the history is empty or the read failed.
            || self.tree_panel.as_ref().is_some_and(|panel| !panel.is_loading())
            || self.plan_list_panel.is_some()
            || self.plan_leave_confirmation.is_some()
            || self.plan_panel.is_some()
            || self.review_transcript.is_some()
            || self.ask_panel.is_some()
            || self.permission_overlay.is_some()
            || self.menu.is_some()
            || self.model_panel.is_some()
    }

    pub(in crate::tui) fn active_transcript(&mut self) -> &mut Transcript {
        self.review_transcript
            .as_mut()
            .unwrap_or(&mut self.conversation)
    }
}
