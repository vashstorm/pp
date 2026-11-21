//! `bash`／`bash_task` 分支：前台执行、后台托管与任务快照。

use super::*;

impl Tools {
    pub(in crate::tools) fn run_bash(
        &self,
        call: &mut ExecuteCall<'_, '_>,
        permissions: pp_config::config::PermissionMode,
        sandbox: pp_sandbox::SandboxView<'_>,
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
        if let Some(request) = args.sandbox_permissions {
            ensure!(
                matches!(call.policy, ToolPolicy::Execute { .. }),
                "sandbox_permissions requires Execute mode"
            );
            match policy.as_ref() {
                Some(base) => {
                    let writable = base.resolve_extra_writable(&request.writable)?;
                    let network = request.network && !base.network();
                    if network || !writable.is_empty() {
                        // 只要沙盒策略仍启用（含 YOLO 叠加显式沙盒覆盖），扩权都必须经过审批。
                        self.permission.authorize_sandbox(
                            pp_permission::SandboxGrant {
                                workspace: Some(self.permission.workspace(&self.cwd).to_owned()),
                                cwd: Some(self.cwd.clone()),
                                command: args.command.clone(),
                                writable: writable.clone(),
                                network,
                            },
                            cancellation,
                        )?;
                        policy = Some(base.with_command_grant(&writable, network)?);
                    }
                }
                // 仅当切到 YOLO 且沙盒确实关闭时忽略旧调用中遗留的扩权参数。
                None if permissions == pp_config::config::PermissionMode::Yolo => {}
                None => bail!("sandbox is unavailable"),
            }
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
