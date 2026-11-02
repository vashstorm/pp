use clap::Parser;
use std::io::Write;

use brush_core::{ExecutionExitCode, ExecutionResult, builtins};

/// Suspend the shell.
#[derive(Parser)]
pub(crate) struct SuspendCommand {
    /// Force suspend login shells.
    #[arg(short = 'f')]
    force: bool,
}

impl builtins::Command for SuspendCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        if context.shell.is_embedded() {
            // Suspending the shell would stop the hosting application, which is
            // never what an embedded command should be able to do.
            writeln!(
                context.stderr(),
                "suspend: not supported in embedded execution"
            )?;
            return Ok(ExecutionExitCode::InvalidUsage.into());
        }

        if context.shell.options().login_shell && !self.force {
            writeln!(context.stderr(), "login shell cannot be suspended")?;
            return Ok(ExecutionExitCode::InvalidUsage.into());
        }

        #[expect(clippy::cast_possible_wrap)]
        brush_core::sys::signal::kill_process(
            std::process::id() as i32,
            brush_core::traps::TrapSignal::Signal(nix::sys::signal::SIGSTOP),
        )?;

        Ok(ExecutionResult::success())
    }
}
