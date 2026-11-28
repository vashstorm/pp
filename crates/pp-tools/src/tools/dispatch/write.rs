//! `write` branch: plan saving in Plan mode, baseline commits and post-save diagnostics linkage.

use super::*;

impl Tools {
    pub(in crate::tools) fn run_write(
        &self,
        call: &mut ExecuteCall<'_, '_>,
        permissions: pp_config::config::PermissionMode,
        sandbox: Option<pp_sandbox::SandboxView<'_>>,
        hook_ask: Option<&str>,
    ) -> Result<(Value, Option<PlanChange>, Option<todo::TodoChange>)> {
        let policy = call.policy;
        let name = call.name;
        let arguments = call.arguments;
        let call_id = call.call_id;
        let cancellation = call.cancellation;
        let emit = &mut *call.emit;
        if let ToolPolicy::Plan {
            document, input, ..
        } = policy
        {
            return plan::run(&self.workspace, document, input, name, arguments)
                .map(|(value, change)| (value, change, None));
        }
        let args: WriteArgs = serde_json::from_str(arguments).context("invalid write arguments")?;
        ensure!(!read::is_pipe_uri(&args.path), "pipe streams are read-only");
        ensure!(
            !args.path.starts_with("artifact://"),
            "artifacts are read-only"
        );
        ensure!(!args.path.trim().is_empty(), "path must not be blank");
        self.check_mutation_instructions("write", arguments)?;
        let MutationTarget {
            record_key: normalized_path,
            is_external,
            target,
        } = self.prepare_mutation_target(
            "write",
            &args.path,
            true,
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
        // Exact baseline: a readable target is committed only while its content is unchanged, never overwriting external edits unconditionally.
        let baseline = match target.read_optional() {
            Ok(Some(bytes)) => WriteBaseline::Existing(bytes),
            Ok(None) => WriteBaseline::Missing,
            // A snapshot failure does not block the commit; the diff must truthfully be marked unavailable.
            Err(_) => WriteBaseline::Unavailable,
        };
        pp_core::cancellation::check(cancellation)?;
        let created = matches!(&baseline, WriteBaseline::Missing);
        let saved_warning = match &baseline {
            WriteBaseline::Existing(bytes) => {
                target.save_if_unchanged(bytes, args.content.as_bytes())
            }
            WriteBaseline::Missing => {
                target.save_new_with_conflict(args.content.as_bytes(), WRITE_NEW_CONFLICT)
            }
            WriteBaseline::Unavailable => target.save(args.content.as_bytes()),
        }?;
        let final_bytes = args.content.into_bytes();
        let bytes_written = final_bytes.len();
        // Recording and diff use the content actually committed by this call.
        let original = match baseline {
            WriteBaseline::Existing(bytes) => diff::Original::Bytes(bytes),
            WriteBaseline::Missing => diff::Original::Missing,
            WriteBaseline::Unavailable => diff::Original::Unavailable,
        };
        self.record_committed_file(call_id, normalized_path, original, &final_bytes, emit);
        drop(guard);
        if cancellation.is_cancelled() {
            // Committed side effects are kept and the existing cancellation chain is reused.
            return Err(Cancelled.into());
        }
        let mut result = json!({"ok":true,"bytes_written":bytes_written});
        let mut warnings = Vec::new();
        if let Some(warning) = saved_warning {
            warnings.push(warning);
        }
        let diagnostics = registry.is_some_and(|registry| {
            registry.settings().diagnostics_on_write && registry.has_servers_for_file(&absolute)
        });
        warnings.extend(self.finish_lsp_save(
            call_id,
            "write",
            &absolute,
            created,
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
        Ok((result, None, None))
    }
}
