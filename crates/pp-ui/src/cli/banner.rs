//! Line-mode startup and exit: line-by-line stdin reads, LSP reports, the startup
//! banner and resume hints.

use anyhow::{Context, Result};
use crossterm::style::Stylize;
use std::{
    io::{self, BufRead, IsTerminal, Write},
    sync::mpsc,
    thread,
    time::Duration,
};

use super::events::cli_write;
use crate::commands;
use pp_config::skills;
use pp_core::cancellation;
use pp_runtime::agent;

/// Result of a line read from line-mode stdin; the dedicated thread only does I/O,
/// commands are still dispatched on the main thread.
pub(crate) enum CliInput {
    Line(String),
    Eof,
    Error(io::Error),
}

/// Idle poll interval: drives late LSP diagnostic display and keeps the main thread
/// responsive to input.
pub(super) const CLI_IDLE_TICK: Duration = Duration::from_millis(50);

pub(crate) fn cli_input_lines() -> Result<mpsc::Receiver<CliInput>> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("pp-stdin".into())
        .spawn(move || {
            let stdin = io::stdin();
            loop {
                let mut line = String::new();
                match stdin.lock().read_line(&mut line) {
                    Ok(0) => {
                        let _ = sender.send(CliInput::Eof);
                        break;
                    }
                    Ok(_) => {
                        if sender.send(CliInput::Line(line)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(CliInput::Error(error));
                        break;
                    }
                }
            }
        })
        .context("starting stdin reader")?;
    Ok(receiver)
}

/// Every CLI normal/error exit path explicitly reaps subagents and language servers;
/// the results are merged through core's shared cleanup, `CleanupError` keeps its type
/// and the original semantic exit code, so Drop alone is not enough.
pub fn stop_cli_lsp(agent: Option<&agent::Agent>, result: Result<()>) -> Result<()> {
    let subagents = match agent {
        Some(agent) => agent.shutdown_subagents(),
        None => Ok(()),
    };
    let result = cancellation::merge_cleanup_result(result, subagents, "stopping subagents");
    let stop = match agent {
        Some(agent) => agent.stop_lsp(),
        None => Ok(()),
    };
    cancellation::merge_cleanup_result(result, stop, "stopping language servers")
}

/// LSP report on stderr; the body is separate from assistant output and control
/// characters are display-filtered first.
pub(crate) fn cli_lsp_report(
    report: &pp_core::lsp_report::DiagnosticReport,
    late: bool,
    stderr: &mut impl Write,
    broken: &mut bool,
    first_error: &mut Option<io::Error>,
) {
    let body = pp_lsp::diagnostics::render(report, Some(pp_lsp::diagnostics::AUTO_MESSAGE_LIMIT));
    let label = if late {
        "LSP diagnostics (late)"
    } else {
        "LSP diagnostics"
    };
    cli_write(
        stderr,
        &format!("{label}\n{}\n", pp_core::text::display_text(&body)),
        broken,
        first_error,
    );
}

/// Non-TUI startup banner: usage hints, skill/command diagnostics and the resumed
/// session hint.
pub fn prelude(
    skills: &skills::SharedSkills,
    file_commands: &commands::FileCommands,
    resumed_id: Option<&str>,
    stderr: &mut impl Write,
) -> Result<()> {
    writeln!(
        stderr,
        "write/edit only modify files inside the startup directory; bash has no directory isolation, so run in a trusted or temporary directory."
    )?;
    writeln!(
        stderr,
        "bash uses an embedded shell; timeouts reclaim managed external processes and cooperatively stop the interpreter. Ctrl + C exits the whole program."
    )?;
    for diagnostic in &skills
        .lock()
        .map_err(|_| anyhow::anyhow!("skill registry unavailable"))?
        .diagnostics
    {
        writeln!(stderr, "skill: {diagnostic}")?;
    }
    for diagnostic in &file_commands.diagnostics {
        writeln!(stderr, "command: {diagnostic}")?;
    }
    if let Some(id) = resumed_id {
        writeln!(stderr, "Resumed session: {id}")?;
    }
    Ok(())
}

pub fn write_resume_hint(
    stderr: &mut (impl Write + IsTerminal),
    session_id: Option<&str>,
) -> io::Result<()> {
    if let Some(id) = session_id {
        if stderr.is_terminal() {
            let hint = format!(
                "{}\x1b[0m\n",
                format!("Resume with: pp --resume {id}").dark_grey()
            );
            stderr.write_all(hint.as_bytes())?;
        } else {
            writeln!(stderr, "Resume with: pp --resume {id}")?;
        }
    }
    Ok(())
}
