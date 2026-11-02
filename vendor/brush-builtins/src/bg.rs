use clap::Parser;
use std::io::Write;

use brush_core::{ExecutionResult, builtins, error, jobs};

/// Moves a job to run in the background.
#[derive(Parser)]
pub(crate) struct BgCommand {
    /// List of job specs to move to background.
    job_specs: Vec<String>,
}

impl BgCommand {
    fn resume(
        context: &mut brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
        job_spec: &str,
    ) -> Result<ExecutionResult, brush_core::Error> {
        let Some(handle) = context.shell.jobs().resolve_job_handle(job_spec) else {
            writeln!(
                context.stderr(),
                "{}: {}: no such job",
                context.command_name,
                job_spec
            )?;
            return Ok(ExecutionResult::general_error());
        };

        if !matches!(handle.state, jobs::JobState::Stopped) {
            return error::unimp("move job to background");
        }

        handle.target.continue_job(context.shell)?;

        if let Some(job) = context.shell.jobs_mut().resolve_job_spec(job_spec) {
            job.mark_running();
        }

        Ok(ExecutionResult::success())
    }
}

impl builtins::Command for BgCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        mut context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        let mut exit_code = ExecutionResult::success();

        if !self.job_specs.is_empty() {
            for job_spec in &self.job_specs {
                let result = Self::resume(&mut context, job_spec)?;
                if !result.is_success() {
                    exit_code = result;
                }
            }
        } else {
            if context.shell.jobs().resolve_job_handle("%").is_none() {
                writeln!(context.stderr(), "{}: no current job", context.command_name)?;
                return Ok(ExecutionResult::general_error());
            }

            exit_code = Self::resume(&mut context, "%")?;
        }

        Ok(exit_code)
    }
}
