use clap::Parser;

use brush_core::{ExecutionResult, builtins, commands};

/// Exec the provided command.
#[derive(Parser)]
pub(crate) struct ExecCommand {
    /// Pass given name as zeroth argument to command.
    #[arg(short = 'a', value_name = "NAME")]
    name_for_argv0: Option<String>,

    /// Exec command with an empty environment.
    #[arg(short = 'c')]
    empty_environment: bool,

    /// Exec command as a login shell.
    #[arg(short = 'l')]
    exec_as_login: bool,

    /// Command and args.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

impl builtins::Command for ExecCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<ExecutionResult, Self::Error> {
        if self.args.is_empty() {
            // When no arguments are present, then there's nothing for us to execute -- but we need
            // to ensure that any redirections setup for this builtin get applied to the calling
            // shell instance. Only the logical descriptor table changes; the host's own
            // descriptors are never duplicated or closed.
            #[allow(clippy::needless_collect)]
            let fds: Vec<_> = context.iter_fds().collect();

            context.shell.replace_open_files(fds.into_iter());
            return Ok(ExecutionResult::success());
        }

        let mut argv0 = self
            .name_for_argv0
            .clone()
            .unwrap_or_else(|| self.args[0].clone());

        if self.exec_as_login {
            argv0 = std::format!("-{argv0}");
        }

        commands::run_exec_command(
            context,
            self.args[0].as_str(),
            argv0.as_str(),
            &self.args[1..],
            self.empty_environment,
        )
        .await
    }
}
