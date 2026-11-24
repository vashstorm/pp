//! `bash`／`bash_task` 分支：前台执行、后台托管与任务快照。

use super::*;

impl Tools {
    pub(in crate::tools) fn run_bash(
        &self,
        call: &mut ExecuteCall<'_, '_>,
        permissions: pp_config::config::PermissionMode,
        sandbox: pp_sandbox::SandboxView<'_>,
        hook_ask: Option<&str>,
    ) -> Result<Value> {
        let args: BashArgs =
            serde_json::from_str(call.arguments).context("invalid bash arguments")?;
        let call_id = call.call_id;
        let cancellation = call.cancellation;
        let emit = &mut *call.emit;
        ensure!(!args.command.trim().is_empty(), "command must not be blank");
        let timeout_seconds = args
            .timeout_seconds
            .unwrap_or(self.limits.bash_timeout_seconds);
        ensure!(
            (1..=self.limits.bash_max_timeout_seconds).contains(&timeout_seconds),
            "timeout_seconds must be an integer between 1 and {} seconds (bash_max_timeout_seconds); omit it to use {} seconds. Zero cannot disable the timeout.",
            self.limits.bash_max_timeout_seconds,
            self.limits.bash_timeout_seconds
        );
        let mut policy = sandbox.policy()?;
        let (extra_writable, extra_network) = if let Some(request) = args.sandbox_permissions {
            ensure!(
                matches!(call.policy, ToolPolicy::Execute { .. }),
                "sandbox_permissions requires Execute mode"
            );
            match policy.as_ref() {
                Some(base) => {
                    let writable = base.resolve_extra_writable(&request.writable)?;
                    let network = request.network && !base.network();
                    (writable, network)
                }
                None if permissions == pp_config::config::PermissionMode::DangerFullAccess => {
                    (Vec::new(), false)
                }
                None => bail!("sandbox is unavailable"),
            }
        } else {
            (Vec::new(), false)
        };
        let access = pp_permission::ToolAccess {
            tier: permission::Tier::Exec,
            sandboxed: policy.is_some(),
            network_enabled: policy.as_ref().is_some_and(|p| p.network()),
        };
        let approved = self.permission.authorize_bash(
            permissions,
            access,
            &self.cwd,
            call.arguments,
            &extra_writable,
            extra_network,
            hook_ask,
            cancellation,
        )?;
        if let Some(base) = policy.as_ref()
            && (!approved.writable.is_empty()
                || !approved.allow_protected.is_empty()
                || approved.network)
        {
            policy = Some(base.with_approved_scope(
                &approved.writable,
                &[],
                &approved.allow_protected,
                approved.network,
            )?);
        }
        pp_core::cancellation::check(cancellation)?;
        let cache = self.workspace.cache_dir()?;
        match args.mode {
            BashMode::Foreground => pp_bash::run(
                &self.cwd,
                &cache,
                &args.command,
                timeout_seconds,
                call_id,
                cancellation,
                emit,
                output::OutputSink::new(self.limits.output, self.artifacts.clone()),
                self.host_defaults,
                &self.pipe_store,
                policy,
            ),
            BashMode::Background => {
                let snapshot = self.background_tasks.start_with_output(
                    &self.cwd,
                    &cache,
                    &args.command,
                    call_id,
                    cancellation,
                    self.host_defaults,
                    &self.pipe_store,
                    output::OutputSink::new(self.limits.output, self.artifacts.clone()),
                    policy,
                )?;
                emit_background_snapshot(call_id, &snapshot, emit);
                Ok(snapshot)
            }
        }
    }

    pub(in crate::tools) fn run_bash_task(
        &self,
        arguments: &str,
        call_id: &str,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Value> {
        let args: BashTaskArgs =
            serde_json::from_str(arguments).context("invalid bash_task arguments")?;
        ensure!(!args.task_id.trim().is_empty(), "task_id must not be blank");
        let snapshot = match args.action {
            BashTaskAction::Status => self.background_tasks.status(&args.task_id)?,
            BashTaskAction::Stop => self.background_tasks.stop(&args.task_id)?,
        };
        emit_background_snapshot(call_id, &snapshot, emit);
        Ok(snapshot)
    }
}
