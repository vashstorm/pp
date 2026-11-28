//! LSP wiring and save linkage for `Tools`.
use anyhow::Result;
use async_openai::types::chat::FunctionObject;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::cancellation::CleanupError;
use pp_core::events::AgentEvent;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Instant;

use super::POST_SAVE_SYNC;
use super::Tools;

impl pp_lsp::tool::LspToolHost for Tools {
    fn lsp_registry(&self) -> Option<&pp_lsp::Registry> {
        self.lsp.as_ref()
    }

    fn lsp_owner_id(&self) -> &str {
        self.lsp_owner.id()
    }

    fn workspace(&self) -> &pp_workspace::save::Workspace {
        &self.workspace
    }
}

impl Tools {
    pub fn lsp_registry(&self) -> Option<&pp_lsp::Registry> {
        self.lsp.as_ref()
    }

    /// The root/standalone owner installs the Registry and keeps the exit cleanup right. With `enabled=false`
    /// it neither exposes the tool nor holds cleanup, skipping discovery and delivery entirely.
    pub fn install_lsp(&mut self, registry: pp_lsp::Registry) {
        if !registry.enabled() {
            self.lsp = None;
            self.lsp_cleanup = None;
            return;
        }
        let cleanup = registry.cleanup();
        // Reinstalling the same Registry (Plan↔Execute keeps clients) only transfers the cleanup right;
        // with a different Registry the old handle reclaims the old process when it leaves scope.
        if let Some(mut previous) = self.lsp_cleanup.take() {
            previous.supersede(&cleanup);
        }
        self.lsp_cleanup = Some(cleanup);
        self.lsp = Some(registry);
    }

    /// A subagent borrows the parent Registry: it shares the same clients but never obtains the right
    /// to stop the parent process, and Drop does not reclaim the parent server.
    pub fn install_shared_lsp(&mut self, registry: pp_lsp::Registry) {
        debug_assert!(
            self.lsp_cleanup.is_none(),
            "child tools must not own an LSP cleanup handle"
        );
        self.lsp = registry.enabled().then_some(registry);
    }

    /// Hands a new cleanup handle to an external holder such as the TUI; it exists only after the owner installs.
    /// Multiple handles for one Registry are handed over via `Cleanup::supersede`, so cleanup never runs twice.
    pub fn lsp_cleanup(&self) -> Option<pp_lsp::Cleanup> {
        self.lsp_cleanup.as_ref()?;
        self.lsp.as_ref().map(pp_lsp::Registry::cleanup)
    }

    pub fn set_lsp_readonly(&mut self, readonly: bool) {
        self.lsp_readonly = readonly;
    }

    /// LSP tool schema: present only once a Registry is installed; restricted modes trim both the
    /// schema and the execution-layer actions, so a hidden schema is never an authorization boundary.
    pub(super) fn lsp_definition(&self, read_only: bool) -> Option<FunctionObject> {
        self.lsp
            .as_ref()
            .map(|_| pp_lsp::tool::definition(read_only))
    }

    /// The LSP Registry this session can auto-link: neither a disabled nor a read-only session takes part in ordinary writes.
    pub(super) fn lsp_save_registry(&self) -> Option<&pp_lsp::Registry> {
        self.lsp
            .as_ref()
            .filter(|registry| registry.enabled() && !self.lsp_readonly)
    }

    /// Sync already-started clients after a commit: overlay/notification updates only, and failures are warnings only.
    fn sync_after_save(
        &self,
        path: &Path,
        created: bool,
        cancellation: &CancellationToken,
    ) -> Vec<String> {
        let Some(registry) = self.lsp_save_registry() else {
            return Vec::new();
        };
        pp_lsp::writethrough::sync_saved(
            registry,
            path,
            created,
            Instant::now() + POST_SAVE_SYNC,
            cancellation,
        )
    }

    /// After the final commit and once the diff is published: sync clients and collect inline diagnostics per settings.
    ///
    /// Returns ordinary warnings; `Cancelled`/`CleanupError` still propagate upward with their types.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_lsp_save(
        &self,
        call_id: &str,
        origin: &str,
        path: &Path,
        created: bool,
        ticket: Option<pp_lsp::writethrough::Ticket>,
        formatter: Option<pp_core::lsp_report::FormatterStatus>,
        diagnostics: bool,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
        result: &mut Value,
    ) -> Result<Vec<String>> {
        let Some(registry) = self.lsp_save_registry() else {
            return Ok(Vec::new());
        };
        let (Some(ticket), true) = (ticket, diagnostics) else {
            return Ok(self.sync_after_save(path, created, cancellation));
        };
        let mut warnings = Vec::new();
        match registry.diagnostics().collect_after_save(
            registry,
            &self.lsp_owner,
            ticket,
            origin,
            created,
            formatter,
        ) {
            Ok(Some(report)) => {
                // The diff was already published in the previous step; the inline diagnostics event only displays that same report.
                emit(AgentEvent::LspDiagnostics {
                    id: Some(call_id.to_owned()),
                    report: report.clone(),
                });
                result["diagnostics"] = json!(&report);
                result["content"] = json!(pp_lsp::diagnostics::render(
                    &report,
                    Some(pp_lsp::diagnostics::AUTO_MESSAGE_LIMIT),
                ));
            }
            Ok(None) => warnings.push(format!(
                "Diagnostics for {} are still running in the background; a late report will follow.",
                path.display()
            )),
            Err(error) if is_control_lsp_error(&error) => return Err(error),
            Err(error) => warnings.push(format!("Diagnostics were not collected: {error:#}")),
        }
        Ok(warnings)
    }
}

/// Control-flow errors must keep their type; ordinary diagnostic failures are only status notes.
fn is_control_lsp_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<Cancelled>() || cause.is::<CleanupError>())
}
