//! `bash`／`bash_task` 分支：前台执行、后台托管与任务快照。

use super::*;

impl Tools {
    pub(in crate::tools) fn run_bash(
        &self,
        arguments: &str,
        call_id: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Value> {
        let args: BashArgs = serde_json::from_str(arguments).context("invalid bash arguments")?;
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
        let cache = self.workspace.cache_dir()?;
        let policy = self.sandbox.policy()?;
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
