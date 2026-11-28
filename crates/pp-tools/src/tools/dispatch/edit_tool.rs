//! `edit` branch: snapshot validation, commit under the write lock and post-save diagnostics linkage.

use super::*;

impl Tools {
    #[allow(clippy::too_many_arguments)]
    pub(in crate::tools) fn run_edit(
        &self,
        arguments: &str,
        call_id: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
        permissions: pp_config::config::PermissionMode,
        sandbox: Option<pp_sandbox::SandboxView<'_>>,
        hook_ask: Option<&str>,
    ) -> Result<Value> {
        let request = edit::Request::parse(self.edit_mode, arguments)?;
        let path = request.path().to_owned();
        ensure!(!path.starts_with("artifact://"), "artifacts are read-only");
        self.check_mutation_instructions("edit", arguments)?;
        let MutationTarget {
            record_key: normalized_path,
            is_external,
            target,
        } = self.prepare_mutation_target(
            "edit",
            &path,
            false,
            permissions,
            sandbox,
            hook_ask,
            cancellation,
        )?;
        let registry = (!is_external).then(|| self.lsp_save_registry()).flatten();
        let absolute = registry
            .map(|registry| registry.cwd().join(&normalized_path))
            .unwrap_or_else(|| self.cwd.join(&normalized_path));
        // Stale diagnostics are invalidated as soon as the new mutation starts; this must precede the write lock and the client operation.
        let ticket = registry.map(|registry| registry.diagnostics().mutate(&absolute));
        let guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = target.read()?;
        let source = std::str::from_utf8(&bytes).context("edit requires a UTF-8 file")?;
        if let edit::Request::Hashline(patch) = &request
            && let Err(error) = self.snapshots.borrow_mut().validate(
                &normalized_path,
                &patch.tag,
                source,
                &patch.edits,
            )
        {
            // Evict read dedup so a follow-up `read` requested by the error message returns fresh text.
            self.invalidate_read_dedup_path(&normalized_path);
            return Err(error);
        }
        let edit::Staged { output, count } = match request.stage(source) {
            Ok(staged) => staged,
            Err(error) => {
                // Evict read dedup so the model's follow-up `read` is never short-circuited by a stale cache hit.
                self.invalidate_read_dedup_path(&normalized_path);
                return Err(error);
            }
        };
        pp_core::cancellation::check(cancellation)?;
        let saved_warning = target.save_if_unchanged(&bytes, output.as_bytes())?;
        let final_bytes = output.into_bytes();
        let bytes_written = final_bytes.len();
        // Recording and diff use the content actually committed by this call.
        self.record_committed_file(
            call_id,
            normalized_path,
            diff::Original::Bytes(bytes),
            &final_bytes,
            emit,
        );
        drop(guard);
        if cancellation.is_cancelled() {
            // Committed side effects are kept and the existing cancellation chain is reused.
            return Err(Cancelled.into());
        }
        let mut result = json!({"ok":true,"edits_applied":count,"bytes_written":bytes_written});
        let mut warnings = Vec::new();
        if let Some(warning) = saved_warning {
            warnings.push(warning);
        }
        let diagnostics = registry.is_some_and(|registry| {
            registry.settings().diagnostics_on_edit && registry.has_servers_for_file(&absolute)
        });
        warnings.extend(self.finish_lsp_save(
            call_id,
            "edit",
            &absolute,
            false,
            ticket,
            None,
            diagnostics,
            cancellation,
            emit,
            &mut result,
        )?);
        if !warnings.is_empty() {
            result["warning"] = json!(warnings.join("; "));
        }
        Ok(result)
    }
}
