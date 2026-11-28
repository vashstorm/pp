use super::super::Agent;
use super::super::plan_sync::{import_plan_content, reconcile_plan_state, sync_plan_todos};
use super::{NO_ORIGIN, NO_PLAN};
use anyhow::{Result, anyhow, bail};
use pp_core::plan::PlanReview;
use pp_core::plan::ReviewToken;

impl Agent {
    /// Restore the coordination entry point: sync the actual path by ID before plan
    /// read/write, review, approval, progress and restore access. revision-0/unbound passes
    /// straight through; a bound document never clones its whole body.
    pub(crate) fn reconcile_plan_location(&mut self) -> Result<()> {
        reconcile_plan_state(
            self.tools.plan_workspace(),
            &mut self.journal,
            &mut self.workflow,
        )
    }

    /// Register on-disk plan bytes as a new revision. Returns `false` when the
    /// file matches the registered snapshot. Draft/Ready documents become
    /// Ready only when the imported content has a genuine Markdown task list;
    /// Executed/Abandoned documents remain read-only after importing progress.
    pub(super) fn import_plan_revision(
        &mut self,
        content: String,
        new_title: &str,
    ) -> Result<bool> {
        import_plan_content(&mut self.journal, &mut self.workflow, content, new_title)
    }

    /// Read the plan through the NOFOLLOW workspace handle, import external revisions and issue
    /// an approval token bound to the current session, plan, path and exact body. A missing or
    /// unreadable file is an error and never falls back to a journal snapshot, while a missing
    /// archive releases the session binding. Executed/Abandoned may refresh but never regain
    /// execution authorization; Plan only syncs the body and only Execute refreshes the task
    /// projection, so a detached plain list is not replaced.
    pub fn read_plan(&mut self) -> Result<PlanReview> {
        self.journal.check()?;
        let document = self
            .workflow
            .plan
            .clone()
            .ok_or_else(|| anyhow!("{NO_PLAN}"))?;
        let report = self.workflow.review.clone();
        if document.revision == 0 {
            // A revision-0 binding is not saved yet: there is no resolvable file path, and
            // nothing is ever imported from a historical snapshot or a guessed path.
            bail!("{NO_PLAN}");
        }
        // Reconcile the registered path by ID and import external revisions; Plan produces no
        // TODO.
        // Tombstones, moves and first registrations surface here.
        sync_plan_todos(
            &self.tools,
            &mut self.journal,
            &mut self.workflow,
            &mut |_| {},
            true,
        )?;
        let document = self
            .workflow
            .plan
            .clone()
            .ok_or_else(|| anyhow!("Archived plan file is missing."))?;
        let session_id = self
            .journal
            .id()
            .ok_or_else(|| anyhow!("{NO_ORIGIN}"))?
            .to_owned();
        Ok(PlanReview {
            token: ReviewToken {
                session_id,
                path: document.path().ok_or_else(|| anyhow!("{NO_PLAN}"))?,
                plan_id: document.id.clone(),
                revision: document.revision,
                content: document.content.clone(),
            },
            document,
            report,
        })
    }
}
