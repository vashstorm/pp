//! `edit` 分支：快照校验、写锁内提交与保存后诊断联动。

use super::*;

impl Tools {
    pub(in crate::tools) fn run_edit(
        &self,
        arguments: &str,
        call_id: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
        sandbox: Option<pp_sandbox::SandboxView<'_>>,
    ) -> Result<Value> {
        let request = edit::Request::parse(self.edit_mode, arguments)?;
        let path = request.path().to_owned();
        ensure!(!path.starts_with("artifact://"), "artifacts are read-only");
        self.check_mutation_instructions("edit", arguments)?;
        let normalized_path = self.workspace.key(Path::new(&path))?;
        let target_path = self.cwd.join(&normalized_path);
        ensure!(
            !self.approval.protects_path(&target_path),
            "approval rules must be managed with /permissions"
        );
        if let Some(sandbox) = sandbox
            && let Some(policy) = sandbox.policy()?
        {
            policy.check_write(&target_path)?;
        }
        let target = self.workspace.target(Path::new(&path), false)?;
        let registry = self.lsp_save_registry();
        let absolute = registry
            .map(|registry| registry.cwd().join(&normalized_path))
            .unwrap_or_else(|| self.cwd.join(&normalized_path));
        // 旧诊断在新 mutation 的第一时间失效；必须早于写锁与客户端 operation。
        let ticket = registry.map(|registry| registry.diagnostics().mutate(&absolute));
        let guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let bytes = target.read()?;
        let source = std::str::from_utf8(&bytes).context("edit requires a UTF-8 file")?;
        if let edit::Request::Hashline(patch) = &request {
            self.snapshots.borrow_mut().validate(
                &normalized_path,
                &patch.tag,
                source,
                &patch.edits,
            )?;
        }
        let edit::Staged { output, count } = request.stage(source)?;
        pp_core::cancellation::check(cancellation)?;
        let saved_warning = target.save_if_unchanged(&bytes, output.as_bytes())?;
        let final_bytes = output.into_bytes();
        let bytes_written = final_bytes.len();
        // 记录与 diff 使用本次实际提交的内容。
        self.record_committed_file(
            call_id,
            normalized_path,
            diff::Original::Bytes(bytes),
            &final_bytes,
            emit,
        );
        drop(guard);
        if cancellation.is_cancelled() {
            // 已提交的副作用保留，沿用现有取消链。
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
