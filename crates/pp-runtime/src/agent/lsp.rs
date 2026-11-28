//! LSP delivery happens only on the Agent's calling thread; display and the next normal model request acknowledge separately.
use super::*;

impl Agent {
    pub(crate) fn set_lsp_run_id(&mut self, run: Option<String>) {
        self.tools.lsp_owner.set_run_id(run);
    }

    pub(crate) fn cancel_lsp_diagnostics(&mut self) {
        self.tools.lsp_owner.cancel();
        self.lsp_injected.clear();
    }

    pub fn poll_lsp_diagnostics(&mut self, emit: &mut dyn FnMut(AgentEvent)) -> Result<()> {
        self.poll_lsp_diagnostics_with_run(&mut |_, event| emit(event))
    }

    pub(crate) fn poll_lsp_diagnostics_with_run(
        &mut self,
        emit: &mut dyn FnMut(Option<&str>, AgentEvent),
    ) -> Result<()> {
        if let Some(binding) = &self.subagent {
            binding.check_lsp_cleanup()?;
        }
        let Some(registry) = self.tools.lsp_registry() else {
            return Ok(());
        };
        for delivery in self.tools.lsp_owner.pending(registry)? {
            if delivery.presented {
                continue;
            }
            let event = AgentEvent::LspDiagnostics {
                id: None,
                report: delivery.report,
            };
            self.journal.event(&event);
            self.journal.check()?;
            emit(delivery.run.as_deref(), event);
            self.tools.lsp_owner.presented(delivery.id);
        }
        Ok(())
    }

    /// Call sites must be after a complete native exchange and before the next normal request.
    /// When the commit fails the injected marker is kept, so the next attempt does not duplicate the developer body.
    pub(super) fn receive_lsp_diagnostics(
        &mut self,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<bool> {
        self.poll_lsp_diagnostics(emit)?;
        let Some(registry) = self.tools.lsp_registry() else {
            return Ok(false);
        };
        let pending: Vec<_> = self
            .tools
            .lsp_owner
            .pending(registry)?
            .into_iter()
            .filter(|delivery| !delivery.model_consumed)
            .collect();
        if pending.is_empty() {
            return Ok(false);
        }
        let mut ids = Vec::with_capacity(pending.len());
        let mut text = String::from("LSP diagnostics (late):\n");
        for delivery in pending {
            ids.push(delivery.id);
            if self.lsp_injected.insert(delivery.id) {
                text.push('\n');
                text.push_str(&pp_lsp::diagnostics::render(
                    &delivery.report,
                    Some(pp_lsp::diagnostics::AUTO_MESSAGE_LIMIT),
                ));
                text.push('\n');
            }
        }
        let mut injected = false;
        if text != "LSP diagnostics (late):\n" {
            append_developer_to(&mut self.session, text);
            injected = true;
        }
        self.journal.commit((&self.session).into())?;
        self.tools.lsp_owner.consumed(&ids);
        for id in ids {
            self.lsp_injected.remove(&id);
        }
        Ok(injected)
    }

    pub(super) fn reset_lsp_diagnostics(&mut self) {
        self.tools.lsp_owner.reset();
        self.lsp_injected.clear();
    }
}
