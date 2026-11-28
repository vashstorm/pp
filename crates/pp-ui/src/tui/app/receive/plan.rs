//! Plan-family events: /plan-list management, approval transitions, review terminal
//! states and the execution-preparation handoff.

use crate::tui::{
    App, Level, PlanEnterAction, PlanLocation, PlanOpenIntent, State, WorkMode, WorkerCommand,
    WorkerEvent, plan_list, plan_location_from_path,
};

impl App {
    /// Plan event family; the order of snapshot adoption and ACK decides when the worker
    /// continues.
    pub(super) fn on_plan_event(&mut self, event: WorkerEvent) -> bool {
        let mut changed = false;
        match event {
            WorkerEvent::PlansListed(result) => {
                self.state = State::Ready;
                if let Some(panel) = &mut self.plan_list_panel {
                    match result {
                        Ok(listing) => panel.set_listing(listing),
                        Err(error) => panel.set_error(format!("{error:#}")),
                    }
                }
                // The panel closed while loading: drop the late list reply immediately.
                changed = true;
            }
            WorkerEvent::ListedPlanRead(result) => {
                self.state = State::Ready;
                let pending = self.plan_list_pending.take();
                match (result, pending) {
                    (Ok(selection), Some(plan_list::PendingOp::Preview))
                        if self.plan_list_panel.is_some() =>
                    {
                        // Panel phase reset: once the preview closes, the list must keep
                        // receiving keys.
                        if let Some(panel) = &mut self.plan_list_panel {
                            panel.set_phase(plan_list::Phase::Ready);
                        }
                        if plan_location_from_path(&selection.path) == Some(PlanLocation::Done) {
                            self.open_plan_list_preview(&selection);
                        } else {
                            self.open_listed_plan_selection(selection, PlanOpenIntent::Execute);
                        }
                    }
                    (Ok(selection), Some(plan_list::PendingOp::Open(intent)))
                        if self.plan_list_panel.is_some() =>
                    {
                        self.open_listed_plan_selection(selection, intent);
                    }
                    (Ok(selection), Some(plan_list::PendingOp::Rename(file_name)))
                        if self.plan_list_panel.is_some() =>
                    {
                        if self.send(
                            WorkerCommand::RenameListedPlan {
                                selection,
                                file_name,
                            },
                            State::RenamingPlan,
                        ) && let Some(panel) = &mut self.plan_list_panel
                        {
                            panel.set_phase(plan_list::Phase::Renaming);
                        }
                    }
                    (Ok(selection), Some(plan_list::PendingOp::Delete))
                        if self.plan_list_panel.is_some() =>
                    {
                        if self.send(
                            WorkerCommand::DeleteListedPlan { selection },
                            State::DeletingPlan,
                        ) && let Some(panel) = &mut self.plan_list_panel
                        {
                            panel.set_phase(plan_list::Phase::Deleting);
                        }
                    }
                    // The panel closed while reading: drop the late selection and continue
                    // no operation.
                    (Ok(_), _) => {}
                    (Err(error), _) => {
                        if let Some(panel) = &mut self.plan_list_panel {
                            panel.set_error(format!("{error:#}"));
                        }
                        self.notify(Level::Error, format!("{error:#}"));
                    }
                }
                changed = true;
            }

            WorkerEvent::ListedPlanOpened(result) => {
                self.state = State::Ready;
                self.secondary_status = None;
                let execute_review = matches!(
                    self.plan_list_pending.take(),
                    Some(plan_list::PendingOp::Open(PlanOpenIntent::Execute))
                );
                match result {
                    Ok(preparation) => {
                        // Displayed after adopting the workflow/replay/todo snapshots; the model
                        // is never requested automatically.
                        let path = preparation
                            .workflow
                            .plan
                            .as_ref()
                            .and_then(|plan| plan.path())
                            .unwrap_or_else(|| "Not saved yet".to_owned());
                        self.plan_panel = None;
                        self.set_workflow(preparation.workflow);
                        let mut note = None;
                        if let Some(records) = preparation.replay {
                            note = self.adopt_history(records);
                        }
                        // The checklist is already installed in its fixed area by the workflow;
                        // the legacy display event is only consumed and does not generate a
                        // duplicate Preview card in the new session's conversation.
                        if let Some(snapshot) = preparation.todo_snapshot {
                            self.conversation.progress(snapshot);
                        }
                        self.session_id = preparation.session_id;
                        self.usage = None;
                        self.task_status = self.task_status_view.snapshot();
                        self.token_speed.clear();
                        if execute_review {
                            // Pre-execution review: re-read safely and issue a token; the list
                            // stays open so closing the menu returns to it. Reset the panel
                            // phase first, otherwise Opening swallows later keys.
                            if let Some(panel) = &mut self.plan_list_panel {
                                panel.set_phase(plan_list::Phase::Ready);
                            }
                            self.request_plan_read();
                        } else {
                            // Continue planning: close the list and hint for more input.
                            self.plan_list_panel = None;
                            self.plan_list_pending = None;
                            if let Some(error) = note {
                                self.notify(Level::Error, error);
                            }
                            self.notify(
                                Level::Normal,
                                format!(
                                    "Opened plan draft {path}; describe the next planning input."
                                ),
                            );
                        }
                    }
                    Err(error) => {
                        if let Some(panel) = &mut self.plan_list_panel {
                            panel.set_error(format!("{error:#}"));
                        }
                        self.notify(Level::Error, format!("{error:#}"));
                    }
                }
                changed = true;
            }
            WorkerEvent::ListedPlanRenamed(result) => {
                self.state = State::Ready;
                if let Some(panel) = &mut self.plan_list_panel {
                    match result {
                        Ok(listing) => panel.set_listing(listing),
                        Err(error) => panel.set_error(format!("{error:#}")),
                    }
                }
                changed = true;
            }
            WorkerEvent::ListedPlanDeleted(result) => {
                self.state = State::Ready;
                if let Some(panel) = &mut self.plan_list_panel {
                    match result {
                        Ok(listing) => panel.set_listing(listing),
                        Err(error) => panel.set_error(format!("{error:#}")),
                    }
                }
                // The following Workflow snapshot synchronizes binding, progress and TODO at
                // once.
                changed = true;
            }

            WorkerEvent::WorkflowChanged(result) => {
                let from_approval = matches!(self.state, State::PreparingExecution);
                let from_enter = matches!(self.state, State::ChangingPlan);
                if from_enter || from_approval {
                    let _ = self.input_queue.close();
                    self.active_cancellation = None;
                    self.secondary_status = None;
                }
                let after_enter = self.plan_after_enter.take();
                // An enter transition carries after_enter; an exit transition keeps it None,
                // which is how an enter failure (retry entering) is told apart from an exit
                // failure (keep the leave direction).
                let was_enter = after_enter.is_some();
                self.state = State::Ready;
                match result {
                    Ok(workflow) => {
                        self.plan_retry_enter = false;
                        self.set_workflow(workflow);
                        // Re-entry after `n` unbinds in Plan mode: once leaving succeeds,
                        // re-enter Plan standby immediately without sending a model request.
                        let reentered =
                            self.plan_reenter_standby && self.workflow.mode == WorkMode::Execute;
                        if reentered {
                            self.plan_reenter_standby = false;
                            self.plan_after_enter = Some(PlanEnterAction::Standby);
                            self.start_plan_transition(
                                |cancellation| WorkerCommand::EnterPlan { cancellation },
                                State::ChangingPlan,
                                "Entering Plan mode...",
                            );
                        }
                        if !reentered {
                            match after_enter {
                                // A successful enter only hints standby; the first user task is
                                // what establishes the binding.
                                Some(PlanEnterAction::Standby) => self.notify(
                                    Level::Normal,
                                    "Plan mode is active. Describe the task to research and plan."
                                        .into(),
                                ),
                                Some(PlanEnterAction::Prompt(prompt, prepared)) => {
                                    let display = prepared.as_ref().map_or_else(
                                        || prompt.clone(),
                                        |input| input.display.clone(),
                                    );
                                    self.resources.ready = prepared;
                                    self.queue_paused = false;
                                    if self.send_task(Some(prompt.clone())) {
                                        self.append("You", &display);
                                    }
                                }
                                None => {}
                            }
                        }
                    }
                    Err(error) => {
                        self.plan_reenter_standby = false;
                        self.notify(Level::Error, format!("{error:#}"));
                        if let Some(PlanEnterAction::Prompt(prompt, prepared)) = after_enter
                            && self.input.is_empty()
                        {
                            if let Some(prepared) = prepared {
                                self.resources.set_images(prepared.images);
                            }
                            self.input.insert_str(format!("/plan {prompt}"));
                            self.resources.bind_text(&self.raw_input_text());
                            self.refresh_candidates(true);
                        }
                        // A failure may still have persisted Plan (the following Workflow
                        // snapshot corrects the display); only an enter failure makes the next
                        // /plan retry entering first, while an exit failure keeps the leave
                        // direction so the retry is not mistaken for another enter.
                        if from_enter && was_enter {
                            self.plan_retry_enter = true;
                        }
                        if from_approval || (from_enter && self.plan_panel.is_some()) {
                            // Approval or save-and-exit failures safely refresh the body and the
                            // new token.
                            self.request_plan_read();
                        }
                    }
                }
                changed = true;
            }

            WorkerEvent::PlanRead(result) => {
                self.state = State::Ready;
                match result {
                    Ok(review) => {
                        let reviewed = self.plan_review_pending.take().is_some();
                        self.open_plan_panel(review);
                        // Refresh after the review completes: focus lands on the Review tab of
                        // the body; for an external revision the panel marks the report stale.
                        // Execution is never started automatically.
                        if reviewed && let Some(panel) = &mut self.plan_panel {
                            panel.focus_review_body();
                        }
                    }
                    Err(error) => {
                        if let Some(report) = self.plan_review_pending.take() {
                            // The report exists but the safe read failed: show the final report
                            // plus the error; a token-less read-only panel disables the stale
                            // execution token until a safe read succeeds again.
                            if let Some(document) = self.workflow.plan.clone() {
                                self.plan_panel =
                                    Some(crate::tui::plan::Panel::degraded(document, report));
                            }
                        }
                        self.notify(Level::Error, format!("{error:#}"));
                    }
                }
                changed = true;
            }

            WorkerEvent::PlanReviewed(result) => {
                // Review terminal state: keep Plan/Ready and return to the five-item menu
                // whether it succeeded or failed; the report is replaced only on success,
                // while failures/cancellation keep the last complete report.
                self.state = State::Ready;
                self.active_cancellation = None;
                self.secondary_status = None;
                self.review_transcript = None;
                let _ = self.input_queue.close();
                match result {
                    Ok(report) => {
                        // The report enters the UI snapshot first, then ReadPlan safely refreshes
                        // the body/token/report binding.
                        self.workflow.review = Some(report.clone());
                        self.plan_review_pending = Some(report);
                        self.request_plan_read();
                    }
                    Err(error) => {
                        self.notify(Level::Error, format!("{error:#}"));
                        self.request_plan_read();
                    }
                }
                changed = true;
            }
            WorkerEvent::Workflow(workflow) => {
                self.set_workflow(workflow);
                changed = true;
            }

            WorkerEvent::ExecutionPrepared {
                workflow,
                session_id,
                usage,
                replay,
                todo_snapshot,
                reply,
            } => {
                // One-shot handoff: adopt the workflow/replay/todo snapshots and update the
                // session first, then ACK; only after the ACK may the worker continue_session.
                self.plan_panel = None;
                self.set_workflow(workflow);
                let mut note = None;
                if let Some(records) = replay {
                    // None: Keep/off does not redraw; Some(empty): Fresh new session;
                    // Some(records): resume.
                    note = self.adopt_history(records);
                }
                // The checklist was installed in its fixed area when the workflow was
                // replaced; the legacy display snapshot no longer generates conversation
                // cards, and the event is still consumed before the ACK to keep the handoff
                // order.
                if let Some(snapshot) = todo_snapshot {
                    self.conversation.progress(snapshot);
                }
                self.session_id = session_id;
                self.usage = usage;
                self.task_status = self.task_status_view.snapshot();
                self.token_speed.clear();
                let confirmed = reply.send(()).is_ok();
                match self.state {
                    State::PreparingExecution if confirmed => {
                        self.secondary_status = None;
                        self.state = State::Running;
                        self.conversation.begin_run();
                        self.append_execution_echo();
                        // List approval enters execution: close the list and the pending
                        // operation.
                        self.plan_list_panel = None;
                        self.plan_list_pending = None;
                    }
                    State::Resuming => {
                        self.secondary_status = None;
                        self.resume_note = note;
                        self.state = State::Ready;
                    }
                    State::ChangingPlan => {
                        let _ = self.input_queue.close();
                        self.active_cancellation = None;
                        self.secondary_status = None;
                        if self.workflow.mode == WorkMode::Execute {
                            self.plan_list_panel = None;
                            self.plan_list_pending = None;
                        }
                        self.state = State::Ready;
                    }
                    _ => {
                        self.secondary_status = None;
                        self.state = State::Ready;
                    }
                }
                changed = true;
            }
            _ => {}
        }
        changed
    }
}
