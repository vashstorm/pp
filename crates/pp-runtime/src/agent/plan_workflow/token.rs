use super::super::Agent;
use super::super::ExecutionPreparation;
use super::{CHANGED, NOT_CURRENT, PENDING};
use crate::agent::input::InputQueue;
use anyhow::{Result, anyhow, bail, ensure};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::plan::PlanStatus;
use pp_core::plan::ReviewToken;
use pp_core::plan::WorkMode;

impl Agent {
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
        // After a rename/move/deletion the old path token can no longer authorize; compare
        // exactly against the actual path resolved from the registry (synced by ID).
        ensure!(
            document.path().is_some_and(|path| path == token.path),
            "{NOT_CURRENT}"
        );
        self.journal.check()?;
        ensure!(
            self.journal.id() == Some(token.session_id.as_str()),
            "{NOT_CURRENT}"
        );
        ensure!(input.is_empty()?, "{PENDING}");
        let content = self.tools.read_plan(document)?;
        Ok((content != token.content).then_some(content))
    }

    /// Shared by the isolated reviewer: a changed file is reported with the
    /// exact changed message so the caller refreshes via `read_plan`.
    pub(crate) fn validate_plan_token(
        &mut self,
        token: &ReviewToken,
        input: &InputQueue,
    ) -> Result<()> {
        // Reconcile by ID before validating: a registry rename/move invalidates the old path
        // token.
        self.reconcile_plan_location()?;
        ensure!(self.plan_token_status(token, input)?.is_none(), "{CHANGED}");
        Ok(())
    }

    /// Approval variant of the token check: a changed file is imported as the
    /// next revision before the exact changed message is returned, so the
    /// worker has regenerated the current token state and the UI only needs
    /// to read the refreshed snapshot.
    pub(super) fn check_plan_token_for_approval(
        &mut self,
        token: &ReviewToken,
        input: &InputQueue,
    ) -> Result<()> {
        self.reconcile_plan_location()?;
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

    /// Keep the draft after safely verifying the saved body, then leave Plan; no model request
    /// is made.
    pub fn save_plan_and_leave(
        &mut self,
        token: ReviewToken,
        input: &InputQueue,
        cancellation: &CancellationToken,
    ) -> Result<ExecutionPreparation> {
        cancellation::check(cancellation)?;
        ensure!(self.workflow.mode == WorkMode::Plan, "{NOT_CURRENT}");
        ensure!(input.is_empty()?, "{PENDING}");
        let review = self.read_plan()?;
        ensure!(review.token == token, "{CHANGED}");
        ensure!(
            matches!(
                review.document.status,
                PlanStatus::Draft | PlanStatus::Ready
            ),
            "{NOT_CURRENT}"
        );
        self.leave_plan(cancellation, None)
    }
}
