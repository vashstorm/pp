//! 行模式启动与退出：stdin 逐行读取、LSP 报告、启动横幅与恢复提示。

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

/// 行模式 stdin 的逐行读取结果；专用线程只负责 I/O，命令仍在主线程分发。
pub(crate) enum CliInput {
    Line(String),
    Eof,
    Error(io::Error),
}

/// 空闲轮询周期：既驱动迟到 LSP 诊断展示，也让主线程保持对输入的响应。
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

/// CLI 各正常/错误退出边界显式回收 language server；`CleanupError` 保持类型并
/// 合并进返回错误，不能只依赖 Tools 的 Drop 兜底。
pub fn stop_cli_lsp(agent: Option<&agent::Agent>, result: Result<()>) -> Result<()> {
    let stop = match agent {
        Some(agent) => agent.stop_lsp(),
        None => Ok(()),
    };
    match (result, stop) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) | (Err(error), Ok(())) => Err(error),
        (Err(error), Err(stop)) => {
            if stop.is::<cancellation::CleanupError>() {
                Err(stop.context(format!("{error:#}")))
            } else {
                Err(error.context(format!("stopping language servers: {stop:#}")))
            }
        }
    }
}

/// stderr 上的 LSP 报告；正文与助手输出分开，控制字符先经显示过滤。
pub(crate) fn cli_lsp_report(
    report: &pp_core::lsp_report::DiagnosticReport,
    late: bool,
    stderr: &mut impl Write,
    broken: &mut bool,
    first_error: &mut Option<io::Error>,
) {
    let body = pp_runtime::lsp::diagnostics::render(
        report,
        Some(pp_runtime::lsp::diagnostics::AUTO_MESSAGE_LIMIT),
    );
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

/// 非 TUI 启动横幅：使用提示、skills/command 诊断与恢复会话提示。
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
