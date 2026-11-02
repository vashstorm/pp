use clap::Parser;
use std::io::Write;

use brush_core::traps::TrapSignal;
use brush_core::{ExecutionExitCode, ExecutionResult, builtins};

/// Signal a job or process.
#[derive(Parser)]
pub(crate) struct KillCommand {
    /// Name of the signal to send.
    #[arg(short = 's', value_name = "SIG_NAME")]
    signal_name: Option<String>,

    /// Number of the signal to send.
    #[arg(short = 'n', value_name = "SIG_NUM")]
    signal_number: Option<usize>,

    /// List known signal names.
    #[arg(short = 'l', short_alias = 'L')]
    list_signals: bool,

    // Interpretation of these depends on whether -l is present.
    #[arg(allow_hyphen_values = true)]
    args: Vec<String>,
}

impl builtins::Command for KillCommand {
    type Error = brush_core::Error;

    async fn execute<SE: brush_core::ShellExtensions>(
        &self,
        context: brush_core::ExecutionContext<'_, SE>,
    ) -> Result<brush_core::ExecutionResult, Self::Error> {
        // Default signal is SIGKILL.
        let mut trap_signal = TrapSignal::Signal(nix::sys::signal::Signal::SIGKILL);

        // Try parsing the signal name (if specified).
        if let Some(signal_name) = &self.signal_name {
            if let Ok(parsed_trap_signal) = TrapSignal::try_from(signal_name.as_str()) {
                trap_signal = parsed_trap_signal;
            } else {
                writeln!(
                    context.stderr(),
                    "{}: invalid signal name: {}",
                    context.command_name,
                    signal_name
                )?;
                return Ok(ExecutionExitCode::InvalidUsage.into());
            }
        }

        // Try parsing the signal number (if specified).
        if let Some(signal_number) = &self.signal_number {
            #[expect(clippy::cast_possible_truncation)]
            #[expect(clippy::cast_possible_wrap)]
            if let Ok(parsed_trap_signal) = TrapSignal::try_from(*signal_number as i32) {
                trap_signal = parsed_trap_signal;
            } else {
                writeln!(
                    context.stderr(),
                    "{}: invalid signal number: {}",
                    context.command_name,
                    signal_number
                )?;
                return Ok(ExecutionExitCode::InvalidUsage.into());
            }
        }

        // Look through the remaining args for a pid/job spec or a -sigspec style option.
        let mut pid_or_job_spec = None;
        for arg in &self.args {
            // See if this is -sigspec syntax.
            if let Some(possible_sigspec) = arg.strip_prefix("-") {
                // `-sigspec` accepts both names and numbers; numeric `-0` is the
                // standard existence probe and must not be treated as a name.
                if let Ok(parsed_trap_signal) = possible_sigspec.parse::<TrapSignal>() {
                    trap_signal = parsed_trap_signal;
                } else {
                    writeln!(
                        context.stderr(),
                        "{}: invalid signal name",
                        context.command_name
                    )?;
                    return Ok(ExecutionExitCode::InvalidUsage.into());
                }
            } else if pid_or_job_spec.is_none() {
                pid_or_job_spec = Some(arg);
            } else {
                writeln!(
                    context.stderr(),
                    "{}: too many jobs or processes specified",
                    context.command_name
                )?;
                return Ok(ExecutionExitCode::InvalidUsage.into());
            }
        }

        if self.list_signals {
            return print_signals(&context, self.args.as_ref());
        }

        // A failing signal specification must not turn into a bare `kill(0)`.
        let signum = match trap_signal {
            TrapSignal::Signal(signal) => signal as i32,
            TrapSignal::Exit => 0,
            TrapSignal::Debug | TrapSignal::Err | TrapSignal::Return => {
                writeln!(
                    context.stderr(),
                    "{}: invalid signal specification",
                    context.command_name
                )?;
                return Ok(ExecutionExitCode::InvalidUsage.into());
            }
        };

        let Some(pid_or_job_spec) = pid_or_job_spec else {
            writeln!(context.stderr(), "{}: invalid usage", context.command_name)?;
            return Ok(ExecutionExitCode::InvalidUsage.into());
        };

        let signal_result = if pid_or_job_spec.starts_with('%') {
            // It's a job spec.
            match context.shell.jobs().resolve_job_handle(pid_or_job_spec) {
                Some(handle) => handle.target.signal(context.shell, signum),
                None => {
                    writeln!(
                        context.stderr(),
                        "{}: {}: no such job",
                        context.command_name,
                        pid_or_job_spec
                    )?;
                    return Ok(ExecutionResult::general_error());
                }
            }
        } else {
            // It's a pid (possibly negative, addressing a process group).
            let pid: i32 = brush_core::int_utils::parse(pid_or_job_spec.as_str(), 10)?;

            context.shell.signal_process(pid, signum)
        };

        if let Err(err) = signal_result {
            writeln!(context.stderr(), "{}: {err}", context.command_name)?;
            return Ok(ExecutionResult::general_error());
        }

        Ok(ExecutionResult::success())
    }
}

fn print_signals(
    context: &brush_core::ExecutionContext<'_, impl brush_core::ShellExtensions>,
    signals: &[String],
) -> Result<ExecutionResult, brush_core::Error> {
    let mut exit_code = ExecutionResult::success();
    if !signals.is_empty() {
        for s in signals {
            // If the user gives us a code, we print the name; if they give a name, we print its
            // code.
            enum PrintSignal {
                Name(&'static str),
                Num(i32),
            }

            let signal = if let Ok(n) = s.parse::<i32>() {
                // bash compatibility. `SIGHUP` -> `HUP`
                TrapSignal::try_from(n).map(|s| {
                    PrintSignal::Name(s.as_str().strip_prefix("SIG").unwrap_or(s.as_str()))
                })
            } else {
                TrapSignal::try_from(s.as_str()).map(|sig| {
                    i32::try_from(sig).map_or(PrintSignal::Name(sig.as_str()), PrintSignal::Num)
                })
            };

            match signal {
                Ok(PrintSignal::Num(n)) => {
                    writeln!(context.stdout(), "{n}")?;
                }
                Ok(PrintSignal::Name(s)) => {
                    writeln!(context.stdout(), "{s}")?;
                }
                Err(e) => {
                    writeln!(context.stderr(), "{e}")?;
                    exit_code = ExecutionResult::general_error();
                }
            }
        }
    } else {
        return brush_core::traps::format_signals(
            context.stdout(),
            TrapSignal::iterator().filter(|s| !matches!(s, TrapSignal::Exit)),
        )
        .map(|()| ExecutionResult::success());
    }

    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use brush_core::extensions::DefaultShellExtensions;
    use brush_core::{ProfileLoadBehavior, RcLoadBehavior, Shell, SourceInfo};

    /// Runs one `kill` invocation in a fresh test shell.
    async fn run_kill(command: &str) -> ExecutionResult {
        let mut shell = Shell::builder()
            .builtins(crate::default_builtins::<DefaultShellExtensions>(
                crate::BuiltinSet::BashMode,
            ))
            .profile(ProfileLoadBehavior::Skip)
            .rc(RcLoadBehavior::Skip)
            .build()
            .await
            .expect("build test shell");
        let params = shell.default_exec_params();
        shell
            .run_string(command.to_owned(), &SourceInfo::default(), &params)
            .await
            .expect("run kill")
    }

    #[tokio::test]
    async fn kill_zero_sigspec_queries_existence() {
        let host_pid = std::process::id();

        // `-0` must parse as signal number 0 (an existence probe), not as a
        // signal name.
        let probe = run_kill(&format!("kill -0 {host_pid}")).await;
        assert!(
            probe.is_success(),
            "kill -0 <host pid> must be an existence probe"
        );

        // A numeric sigspec still selects the numbered signal; the probe of a
        // definitely nonexistent process fails with a general error rather than
        // the invalid-usage error reserved for unknown signal specs.
        let missing = run_kill("kill -0 2147483647").await;
        assert!(
            matches!(missing.exit_code, ExecutionExitCode::GeneralError),
            "kill -0 must send signal 0, not be rejected as an invalid signal name"
        );

        let unknown = run_kill("kill -NOSUCHSIGNAL 1").await;
        assert!(
            matches!(unknown.exit_code, ExecutionExitCode::InvalidUsage),
            "unknown signal specs must still be rejected"
        );
    }

    #[tokio::test]
    async fn kill_destructive_sigspec_still_refuses_host() {
        let host_pid = std::process::id();

        let refused = run_kill(&format!("kill -TERM {host_pid}")).await;
        assert!(
            !refused.is_success(),
            "destructive signals aimed at the host must be refused"
        );
        assert!(
            matches!(refused.exit_code, ExecutionExitCode::GeneralError),
            "the refusal must be a general error"
        );

        // Reaching this assertion at all proves the host was not signalled; the
        // existence probe then succeeds against the still-running host.
        let probe = run_kill(&format!("kill -0 {host_pid}")).await;
        assert!(probe.is_success(), "the host must still be alive");
    }
}
