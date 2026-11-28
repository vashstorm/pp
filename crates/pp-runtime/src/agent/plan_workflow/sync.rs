use super::super::Agent;
use super::super::plan_sync::sync_plan_todos;
use super::{NO_PLAN, TASKS_CHANGED};
use anyhow::{Result, anyhow, ensure};
use pp_core::plan;
use pp_core::plan::PlanLocation;
use pp_core::plan::PlanStatus;
use pp_core::plan::WorkMode;
use pp_core::plan::WorkflowState;
use pp_tools::plan_catalog;

impl Agent {
    /// Persist a workflow change; on failure restore the whole `previous`
    /// state so callers keep one transactional rollback idiom.
    pub(in crate::agent) fn commit_workflow(&mut self, previous: WorkflowState) -> Result<()> {
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error);
        }
        Ok(())
    }

    pub(in crate::agent) fn refresh_plan_progress(
        &mut self,
        emit: &mut dyn FnMut(pp_core::events::AgentEvent),
    ) -> Result<()> {
        if self.workflow.mode != WorkMode::Execute
            || !self
                .workflow
                .plan
                .as_ref()
                .is_some_and(|plan| plan.status == PlanStatus::Executed)
        {
            return Ok(());
        }
        sync_plan_todos(
            &self.tools,
            &mut self.journal,
            &mut self.workflow,
            emit,
            false,
        )
    }

    /// Automatic archival after a successful finish: only Execute + Executed + Plans may enter.
    /// After safely reading the current on-disk file and syncing progress it requires total > 0
    /// with completed == total, and the current task text vector must exactly equal the
    /// approved_tasks frozen at approval time; deleted, replaced, added or reordered tasks all
    /// fail. When not everything is checked or there are zero tasks it is skipped silently
    /// (`Ok(false)`) and Executed/Plans is kept; when the task set differs from the approved set
    /// it returns `Plan tasks changed; re-plan before completing.` and likewise does not move.
    /// The move reuses the two-phase protocol (lock order PlanLease → registry lock, the task
    /// lease held by finish_task): publish the Done link and CAS-update the registration →
    /// commit workflow Completed (a pure path/status change, no body revision bump) → remove
    /// the plans source link. If the registration was updated but the workflow commit fails, the
    /// source link is kept and the real error is returned, and the next ID reconciliation lands
    /// on Done/Completed; cleanup warnings after the commit do not revoke the state. Returns
    /// whether archival completed.
    pub(in crate::agent) fn archive_completed_plan(
        &mut self,
        emit: &mut dyn FnMut(pp_core::events::AgentEvent),
    ) -> Result<bool> {
        if self.workflow.mode != WorkMode::Execute {
            return Ok(false);
        }
        let Some(document) = self.workflow.plan.as_ref() else {
            return Ok(false);
        };
        if document.status != PlanStatus::Executed || document.location != PlanLocation::Plans {
            return Ok(false);
        }
        // Safely read the current on-disk file and sync progress before archiving; on failure
        // keep Executed/Plans.
        self.refresh_plan_progress(emit)?;
        let Some(document) = self.workflow.plan.as_ref().filter(|document| {
            document.status == PlanStatus::Executed && document.location == PlanLocation::Plans
        }) else {
            return Ok(false);
        };
        let (completed, total) = plan::task_progress(&document.content);
        if total == 0 || completed != total {
            return Ok(false);
        }
        let tasks: Vec<String> = plan::plan_tasks(&document.content)
            .into_iter()
            .map(|task| task.text)
            .collect();
        // The migration baseline for old records was established by refresh_plan_progress;
        // still missing or inconsistent with the current task set is an archival-contract
        // violation, so the approved set is not guessed.
        ensure!(
            document.approved_tasks.as_deref() == Some(tasks.as_slice()),
            "{TASKS_CHANGED}"
        );
        let file_name = document
            .file_name
            .clone()
            .ok_or_else(|| anyhow!("{NO_PLAN}"))?;
        let id = document.id.clone();
        let expected = document.content.as_bytes().to_vec();
        // Publish the Done link and atomically update the registry; on failure keep
        // Executed/Plans and the source link.
        let warning = plan_catalog::publish_plan_move(
            self.tools.plan_workspace(),
            &id,
            (file_name.as_str(), PlanLocation::Plans),
            (file_name.as_str(), PlanLocation::Done),
            &expected,
        )?;
        if let Some(warning) = warning {
            tracing::warn!(event = "plan_archive_warning", warning = %warning);
        }
        // The registration now points at Done: commit the Completed binding; on failure keep the
        // source link and return the real error, and the next ID reconciliation finishes the
        // cleanup without claiming a rollback.
        let previous = self.workflow.clone();
        {
            let document = self.workflow.plan.as_mut().expect("executed plan bound");
            document.status = PlanStatus::Completed;
            document.location = PlanLocation::Done;
        }
        if let Err(error) = self.journal.write_workflow(&self.workflow) {
            self.workflow = previous;
            return Err(error.context(
                "Plan archival committed to the registry but the session state was not updated; the source link remains.",
            ));
        }
        // Status committed: remove the plans source link; a directory sync failure is only a
        // warning and does not revoke it.
        if let Err(error) = plan_catalog::commit_plan_move(
            self.tools.plan_workspace(),
            (file_name.as_str(), PlanLocation::Plans),
            (file_name.as_str(), PlanLocation::Done),
            &expected,
        ) {
            tracing::warn!(event = "plan_move_cleanup_failed", error = %error);
        }
        let document = self.workflow.plan.as_ref().expect("completed plan bound");
        emit(pp_core::events::AgentEvent::PlanProgressUpdated {
            plan_id: document.id.clone(),
            revision: document.revision,
            content: document.content.clone(),
            file_name: document.file_name.clone(),
            location: Some(document.location),
            status: Some(document.status),
        });
        Ok(true)
    }
}
