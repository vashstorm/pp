//! `write` 分支：Plan 模式转计划保存、基线提交与保存后诊断联动。

use super::*;

impl Tools {
    pub(in crate::tools) fn run_write(
        &self,
        policy: ToolPolicy<'_>,
        name: &str,
        arguments: &str,
        call_id: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<(Value, Option<PlanChange>, Option<todo::TodoChange>)> {
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
        let target = self.workspace.target(Path::new(&args.path), true)?;
        let normalized_path = self.workspace.key(Path::new(&args.path))?;
        let registry = self.lsp_save_registry();
        let absolute = registry
            .map(|registry| registry.cwd().join(&normalized_path))
            .unwrap_or_else(|| self.cwd.join(&normalized_path));
        // 旧诊断在新 mutation 的第一时间失效；必须早于写锁与客户端 operation。
        let ticket = registry.map(|registry| registry.diagnostics().mutate(&absolute));
        let guard = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        // 精确基线：可读的目标只在内容未变化时提交，绝不无条件覆盖外部改动。
        let baseline = match target.read_optional() {
            Ok(Some(bytes)) => WriteBaseline::Existing(bytes),
            Ok(None) => WriteBaseline::Missing,
            // 快照失败不阻止提交；diff 必须如实标记为不可用。
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
        // 记录与 diff 使用本次实际提交的内容。
        let original = match baseline {
            WriteBaseline::Existing(bytes) => diff::Original::Bytes(bytes),
            WriteBaseline::Missing => diff::Original::Missing,
            WriteBaseline::Unavailable => diff::Original::Unavailable,
        };
        self.record_committed_file(call_id, normalized_path, original, &final_bytes, emit);
        drop(guard);
        if cancellation.is_cancelled() {
            // 已提交的副作用保留，沿用现有取消链。
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
