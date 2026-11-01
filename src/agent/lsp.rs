//! LSP 投递仅在 Agent 调用线程发生；显示与下一次普通模型请求各自确认。
use super::*;

impl Agent {
    pub(crate) fn set_lsp_run_id(&mut self, run: Option<String>) {
        self.tools.lsp_owner.set_run_id(run);
    }

    pub(crate) fn cancel_lsp_diagnostics(&mut self) {
        self.tools.lsp_owner.cancel();
        self.lsp_injected.clear();
    }

    pub(crate) fn poll_lsp_diagnostics(&mut self, emit: &mut dyn FnMut(AgentEvent)) -> Result<()> {
        self.poll_lsp_diagnostics_with_run(&mut |_, event| emit(event))
    }

    pub(crate) fn poll_lsp_diagnostics_with_run(
        &mut self,
        emit: &mut dyn FnMut(Option<&str>, AgentEvent),
    ) -> Result<()> {
        if let Some(binding) = &self.tools.subagent {
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

    /// 调用点必须在完整 native exchange 之后、下一次普通请求之前。
    /// 提交失败时保留已注入标记，下一次尝试不会复制 developer 正文。
    pub(super) fn receive_lsp_diagnostics(
        &mut self,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        self.poll_lsp_diagnostics(emit)?;
        let Some(registry) = self.tools.lsp_registry() else {
            return Ok(());
        };
        let pending: Vec<_> = self
            .tools
            .lsp_owner
            .pending(registry)?
            .into_iter()
            .filter(|delivery| !delivery.model_consumed)
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let mut ids = Vec::with_capacity(pending.len());
        let mut text = String::from("LSP diagnostics (late):\n");
        for delivery in pending {
            ids.push(delivery.id);
            if self.lsp_injected.insert(delivery.id) {
                text.push('\n');
                text.push_str(&crate::lsp::diagnostics::render(
                    &delivery.report,
                    Some(crate::lsp::diagnostics::AUTO_MESSAGE_LIMIT),
                ));
                text.push('\n');
            }
        }
        if text != "LSP diagnostics (late):\n" {
            append_developer_to(&mut self.session, text);
        }
        self.journal.commit((&self.session).into())?;
        self.tools.lsp_owner.consumed(&ids);
        for id in ids {
            self.lsp_injected.remove(&id);
        }
        Ok(())
    }

    pub(super) fn reset_lsp_diagnostics(&mut self) {
        self.tools.lsp_owner.reset();
        self.lsp_injected.clear();
    }
}
