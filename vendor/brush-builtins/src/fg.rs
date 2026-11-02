use clap::Parser;
use std::io::Write;

use brush_core::{ExecutionResult, builtins, jobs, sys};

/// Move a specified job to the foreground.
#[derive(Parser)]
pub(crate) struct FgCommand {
    /// Job spec for the job to move to the foreground; if not specified, the current job is moved.
    job_spec: Option<String>,
}

impl builtins::Command for FgCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // Read interactive option before taking a mutable borrow on jobs.
        let is_interactive = context.shell.options().interactive;
        let embedded = context.shell.is_embedded();

        let spec = self.job_spec.clone().unwrap_or_else(|| "%".to_owned());

        let Some(handle) = context.shell.jobs().resolve_job_handle(&spec) else {
            if self.job_spec.is_some() {
                writeln!(
                    context.stdout(),
                    "{}: {}: no such job",
                    spec,
                    context.command_name
                )?;
            } else {
                writeln!(context.stdout(), "{}: no current job", context.command_name)?;
            }

            return Ok(ExecutionResult::general_error());
        };

        if matches!(handle.state, jobs::JobState::Stopped) {
            handle.target.continue_job(context.shell)?;
        }

        // An embedded shell never owns a terminal; the host keeps control of its
        // own foreground process group.
        if is_interactive
            && !embedded
            && let Some(pgid) = handle.target.pgid()
        {
            sys::terminal::move_to_foreground(pgid)?;
        }

        writeln!(context.stdout(), "{}", handle.command_line)?;

        let (result, stopped, formatted) = {
            let Some(job) = context.shell.jobs_mut().resolve_job_spec(&spec) else {
                return Ok(ExecutionResult::general_error());
            };

            let result = job.wait().await?;
            let stopped = job.is_stopped();
            let formatted = job.to_string();

            (result, stopped, formatted)
        };

        if is_interactive && !embedded {
            sys::terminal::move_self_to_foreground()?;
        }

        if stopped {
            // N.B. We use the '\r' to overwrite any ^Z output.
            writeln!(context.stderr(), "\r{formatted}")?;
        }

        Ok(result)
    }
}
