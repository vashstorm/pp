use crate::tui::{
    App, CancellationToken, Level, PlanEnterAction, PlanLocation, PlanOpenIntent, PlanSelection,
    PlanStatus, State, WorkMode, WorkerCommand, context_panel, plan, plan_list,
    plan_location_from_path, tree,
};

impl App {
    // Session history must not change while work is pending; shared by /tree and the
    // New/Resume/Tree entry points of submit.
    pub(in crate::tui) fn pending_blocks_session_change(&mut self) -> bool {
        if !self.has_pending() {
            return false;
        }
        self.notify(
            Level::Error,
            "Recall pending messages before changing session history.".into(),
        );
        true
    }

    pub(in crate::tui) fn open_tree(&mut self) {
        self.escape_tap.reset();
        if self.pending_blocks_session_change() {
            return;
        }
        if !matches!(self.state, State::Ready)
            || self.skills_pending
            || self.active_settings_tab().is_some()
            || self.session_panel.is_some()
            || self.ask_panel.is_some()
            || self.tree_panel.is_some()
            || self.context_panel.is_some()
        {
            return;
        }
        self.menu = None;
        self.file_query = None;
        self.tree_panel = Some(tree::Panel::new());
        self.send(WorkerCommand::ReadTree, State::ReadingTree);
    }

    pub(in crate::tui) fn open_context(&mut self) {
        self.escape_tap.reset();
        if !matches!(
            self.state,
            State::Ready | State::Running | State::Cancelling
        ) || self.skills_pending
            || self.active_settings_tab().is_some()
            || self.session_panel.is_some()
            || self.ask_panel.is_some()
            || self.tree_panel.is_some()
            || self.context_panel.is_some()
        {
            return;
        }
        self.menu = None;
        self.file_query = None;
        self.menu_dismissed = true;
        self.context_panel = Some(context_panel::Panel::new());
        if matches!(self.state, State::Running | State::Cancelling) {
            self.read_context_snapshot();
        } else if !self.send(WorkerCommand::InspectContext, State::InspectingContext) {
            self.context_panel = None;
        }
    }

    fn read_context_snapshot(&mut self) {
        let cached_policy = self
            .cached_settings
            .as_ref()
            .map(|s| (s.auto_compact, s.auto_compact_threshold_percent));
        let Some(panel) = &mut self.context_panel else {
            return;
        };
        match self
            .context_snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.read())
        {
            Some(Ok(report)) => {
                let mut report = (*report).clone();
                if let Some((auto_compact, threshold_percent)) = cached_policy {
                    report.auto_compact = auto_compact;
                    report.threshold_percent = threshold_percent;
                }
                panel.set_report(report);
            }
            Some(Err(error)) => panel.set_error(error),
            None => panel
                .set_error("Context diagnostics are not available yet; press r to refresh.".into()),
        }
    }

    pub(in crate::tui) fn close_context(&mut self) {
        self.context_panel = None;
        self.menu_dismissed = true;
    }

    pub(in crate::tui) fn apply_context_action(&mut self, action: context_panel::Action) {
        match action {
            context_panel::Action::None => {}
            context_panel::Action::Close => self.close_context(),
            context_panel::Action::Refresh => {
                if matches!(self.state, State::Running | State::Cancelling) {
                    self.read_context_snapshot();
                } else if matches!(self.state, State::Ready)
                    && !self.send(WorkerCommand::InspectContext, State::InspectingContext)
                    && let Some(panel) = &mut self.context_panel
                {
                    panel.set_error("Agent worker unavailable.".into());
                }
            }
        }
    }

    /// Precondition for opening /plan-list: Ready, no pending work and no other modal; the
    /// input is kept when rejected.
    pub(in crate::tui) fn plan_list_entry_ready(&mut self) -> bool {
        if !matches!(self.state, State::Ready) {
            self.notify(
                Level::Error,
                "Cancel the current task before changing Plan mode.".into(),
            );
            return false;
        }
        if self.has_pending() {
            self.notify(
                Level::Error,
                "Apply pending input before changing Plan mode.".into(),
            );
            return false;
        }
        if self.ask_panel.is_some()
            || self.active_settings_tab().is_some()
            || self.skills_pending
            || self.session_panel.is_some()
            || self.tree_panel.is_some()
            || self.context_panel.is_some()
            || self.plan_panel.is_some()
            || self.model_panel.is_some()
            || self.queue_menu
        {
            self.notify(
                Level::Error,
                "Close the open panel before changing Plan mode.".into(),
            );
            return false;
        }
        true
    }

    pub(in crate::tui) fn open_plan_list(&mut self) {
        self.escape_tap.reset();
        self.file_query = None;
        self.menu = None;
        self.menu_dismissed = true;
        self.plan_list_pending = None;
        self.plan_list_panel = Some(plan_list::Panel::new());
        if !self.send(WorkerCommand::ListPlans, State::ListingPlans) {
            self.plan_list_panel = None;
        }
    }

    pub(in crate::tui) fn close_plan_list(&mut self) {
        self.plan_list_panel = None;
        self.plan_list_pending = None;
        self.menu_dismissed = true;
    }

    pub(in crate::tui) fn apply_plan_list_action(&mut self, action: plan_list::Action) {
        match action {
            plan_list::Action::None => {}
            plan_list::Action::Close => self.close_plan_list(),
            plan_list::Action::Refresh => {
                if matches!(self.state, State::Ready)
                    && let Some(panel) = &mut self.plan_list_panel
                {
                    panel.set_loading();
                    self.send(WorkerCommand::ListPlans, State::ListingPlans);
                }
            }
            plan_list::Action::Preview(id) => {
                if !matches!(self.state, State::Ready) {
                    return;
                }
                self.plan_list_pending = Some(plan_list::PendingOp::Preview);
                if !self.send(
                    WorkerCommand::ReadListedPlan { id },
                    State::ReadingListedPlan,
                ) {
                    self.plan_list_pending = None;
                } else if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_reading();
                }
            }
            plan_list::Action::Open(id, intent) => {
                if !matches!(self.state, State::Ready) {
                    return;
                }
                self.plan_list_pending = Some(plan_list::PendingOp::Open(intent));
                if !self.send(
                    WorkerCommand::ReadListedPlan { id },
                    State::ReadingListedPlan,
                ) {
                    self.plan_list_pending = None;
                } else if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_reading();
                }
            }
            plan_list::Action::Rename(id, file_name) => {
                if !matches!(self.state, State::Ready) {
                    return;
                }
                self.plan_list_pending = Some(plan_list::PendingOp::Rename(file_name));
                if !self.send(
                    WorkerCommand::ReadListedPlan { id },
                    State::ReadingListedPlan,
                ) {
                    self.plan_list_pending = None;
                } else if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_reading();
                }
            }
            plan_list::Action::Delete(id) => {
                if !matches!(self.state, State::Ready) {
                    return;
                }
                self.plan_list_pending = Some(plan_list::PendingOp::Delete);
                if !self.send(
                    WorkerCommand::ReadListedPlan { id },
                    State::ReadingListedPlan,
                ) {
                    self.plan_list_pending = None;
                } else if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_reading();
                }
            }
            plan_list::Action::NewPlan => self.plan_list_new_plan(),
            plan_list::Action::Message(message) => {
                if let Some(panel) = &mut self.plan_list_panel {
                    panel.set_error(message);
                }
            }
        }
    }

    pub(in crate::tui) fn open_listed_plan_selection(
        &mut self,
        selection: PlanSelection,
        intent: PlanOpenIntent,
    ) {
        let cancellation = CancellationToken::new();
        if self.send(
            WorkerCommand::OpenListedPlan {
                selection,
                intent,
                cancellation: cancellation.clone(),
            },
            State::OpeningListedPlan,
        ) {
            self.plan_list_pending = Some(plan_list::PendingOp::Open(intent));
            self.active_cancellation = Some(cancellation);
            if let Some(panel) = &mut self.plan_list_panel {
                panel.set_opening();
            }
        }
    }

    /// `n` creates a new plan: keeps saved drafts, closes the list and enters Plan+None standby;
    /// it allocates no ID, writes no body and calls no model, and an unsaved draft reuses the
    /// bare /plan abandon confirmation.
    pub(in crate::tui) fn plan_list_new_plan(&mut self) {
        if self.workflow.mode == WorkMode::Plan
            && let Some(plan) = &self.workflow.plan
            && plan.revision == 0
            && !plan.origin_prompt.trim().is_empty()
        {
            self.request_plan_leave(true);
            return;
        }
        self.close_plan_list();
        if self.workflow.mode == WorkMode::Execute || self.plan_retry_enter {
            self.plan_after_enter = Some(PlanEnterAction::Standby);
            self.start_plan_transition(
                |cancellation| WorkerCommand::EnterPlan { cancellation },
                State::ChangingPlan,
                "Entering Plan mode...",
            );
            return;
        }
        // Already in Plan mode: release the current binding first (LeavePlan->Execute+None), and
        // on success WorkflowChanged re-enters Plan standby immediately.
        self.request_plan_leave(true);
    }

    /// Done read-only preview: builds a display-only document from the selection and issues no
    /// approval token.
    pub(in crate::tui) fn open_plan_list_preview(&mut self, selection: &PlanSelection) {
        let location = plan_location_from_path(&selection.path).unwrap_or(PlanLocation::Drafts);
        let file_name = selection
            .path
            .rsplit('/')
            .next()
            .unwrap_or("plan.md")
            .to_owned();
        let status = match location {
            PlanLocation::Drafts => PlanStatus::Draft,
            PlanLocation::Plans => PlanStatus::Ready,
            PlanLocation::Done => PlanStatus::Completed,
        };
        let document = pp_core::plan::PlanDocument {
            id: selection.id.clone(),
            revision: 1,
            title: file_name.trim_end_matches(".md").to_owned(),
            content: selection.content.clone(),
            status,
            origin_prompt: String::new(),
            file_name: Some(file_name),
            location,
            approved_tasks: None,
        };
        self.plan_progress = pp_core::plan::task_progress(&document.content);
        match &mut self.plan_panel {
            Some(panel) if panel.document().id == document.id => panel.set_progress(&document),
            _ => self.plan_panel = Some(plan::Panel::progress(document)),
        }
        self.menu = None;
        self.menu_dismissed = true;
        self.file_query = None;
    }
}
