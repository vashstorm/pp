//! 行模式 CLI：stdin 逐行读取、事件 → stdout/stderr 输出、`/command` 分发与退出收尾。

mod banner;
mod events;
mod execution;
mod pipes;

pub(crate) use banner::cli_lsp_report;
pub use banner::prelude;
pub use banner::stop_cli_lsp;
pub use banner::write_resume_hint;
use banner::{CLI_IDLE_TICK, CliInput, cli_input_lines};
use events::{cli_write, run_cli_compaction, run_cli_shell};
#[cfg(debug_assertions)]
pub use execution::run_cli_mock_test;
pub use execution::run_cli_task;
use pipes::cli_pipes_list;

use anyhow::{Context, Result};
use std::{
    io::{self, Write},
    sync::mpsc,
};

use crate::commands;
use crate::tips;
use pp_config::skills;
use pp_core::cancellation;
use pp_core::events as core_events;
use pp_runtime::agent;

/// 行模式交互循环：逐行读取 stdin、分发 `/command` 并输出结果。
///
/// `initial_command` 是 CLI 位置参数给出的启动命令：只合成一行、不打印提示符；
/// 未知命令或解析错误使本次运行失败（脚本可见非零退出码），真实 stdin 行保持原有语义。
pub fn run_line_mode(
    mut agent: Result<agent::Agent>,
    skills: skills::SharedSkills,
    file_commands: commands::FileCommands,
    initial_command: Option<String>,
) -> Result<()> {
    let mut stderr = io::stderr();
    let mut stdout = io::stdout().lock();
    writeln!(stderr, "pp — use /help for commands")?;
    if let Err(error) = &agent {
        writeln!(stderr, "error: {error:#}")?;
        writeln!(
            stderr,
            "Configuration unavailable; model tasks are disabled. Fix the config files and restart pp. /quit still exits cleanly."
        )?;
    }
    let input = match cli_input_lines() {
        Ok(input) => input,
        Err(error) => return stop_cli_lsp(agent.as_ref().ok(), Err(error)),
    };
    let mut initial = initial_command;
    let mut stats = None;
    let mut prompt_shown = false;
    loop {
        // 启动命令只合成一行：不打印提示符，执行后继续等待真实 stdin。
        let initial_line = initial.take();
        let synthetic = initial_line.is_some();
        if synthetic {
            prompt_shown = false;
        } else if !prompt_shown {
            write!(stderr, "> ")?;
            stderr.flush()?;
            prompt_shown = true;
        }
        let line = match initial_line {
            Some(line) => line,
            None => match input.recv_timeout(CLI_IDLE_TICK) {
                Ok(CliInput::Line(line)) => {
                    prompt_shown = false;
                    line
                }
                Ok(CliInput::Eof) => break,
                Ok(CliInput::Error(error)) => {
                    return stop_cli_lsp(agent.as_ref().ok(), Err(error).context("reading stdin"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // 空闲时只展示迟到诊断，不续跑模型、不自动继续会话。
                    let polled = match agent.as_mut() {
                        Ok(active) => {
                            let mut reports = Vec::new();
                            let polled =
                                active.poll_lsp_diagnostics(&mut |event| reports.push(event));
                            let mut broken = false;
                            let mut first_error = None;
                            if !reports.is_empty() {
                                if prompt_shown {
                                    cli_write(&mut stderr, "\n", &mut broken, &mut first_error);
                                }
                                for event in reports {
                                    if let core_events::AgentEvent::LspDiagnostics {
                                        report, ..
                                    } = event
                                    {
                                        cli_lsp_report(
                                            &report,
                                            true,
                                            &mut stderr,
                                            &mut broken,
                                            &mut first_error,
                                        );
                                    }
                                }
                                if prompt_shown {
                                    write!(stderr, "> ")?;
                                    stderr.flush()?;
                                }
                            }
                            match (polled, first_error) {
                                (Ok(()), None) => Ok(()),
                                (Ok(()), Some(error)) => {
                                    Err(error).context("writing LSP diagnostics")
                                }
                                (Err(error), _) => Err(error),
                            }
                        }
                        Err(_) => Ok(()),
                    };
                    if let Err(error) = polled {
                        return stop_cli_lsp(agent.as_ref().ok(), Err(error));
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        let command = match commands::parse(line.trim_end_matches(['\r', '\n']), &file_commands) {
            Ok(command) => command,
            Err(error) => {
                if synthetic {
                    return stop_cli_lsp(agent.as_ref().ok(), Err(error));
                }
                writeln!(stderr, "error: {error:#}")?;
                continue;
            }
        };
        // 未知命令静默忽略，与 TUI 一致；显式启动命令例外，必须给出非零退出码。
        if matches!(command, Some(commands::Command::Unknown)) {
            if synthetic {
                return stop_cli_lsp(
                    agent.as_ref().ok(),
                    Err(anyhow::anyhow!("unknown command: {prompt}; use /help")),
                );
            }
            continue;
        }
        if matches!(command, Some(commands::Command::Subagents)) {
            if let Ok(agent) = agent.as_mut() {
                agent.ensure_subagents()?;
                if let Some(binding) = agent.subagents() {
                    writeln!(
                        stderr,
                        "{}",
                        serde_json::to_string_pretty(&binding.snapshots())?
                    )?;
                } else {
                    writeln!(
                        stderr,
                        "No subagents; use /Roles or configured .pp/agents and ~/.pp/agent/agents definitions."
                    )?;
                }
            }
            continue;
        }
        match command {
            Some(commands::Command::Roles) => {
                writeln!(
                    stderr,
                    "/Roles requires an interactive terminal; configure model_roles in settings.yml instead."
                )?;
                continue;
            }
            Some(commands::Command::Quit) => break,
            Some(commands::Command::Mcp(action)) => {
                if matches!(action, pp_runtime::mcp::Action::Help) {
                    writeln!(stderr, "{}", pp_runtime::mcp::help())?;
                } else {
                    match agent.as_mut() {
                        Ok(agent) => match agent
                            .manage_mcp(&action, &pp_core::cancellation::CancellationToken::new())
                        {
                            Ok(view) => writeln!(stderr, "{}", view.text())?,
                            Err(error) => {
                                if error.is::<pp_core::cancellation::CleanupError>() {
                                    return stop_cli_lsp(Some(&*agent), Err(error));
                                }
                                writeln!(stderr, "error: {error:#}")?;
                            }
                        },
                        Err(_) => writeln!(
                            stderr,
                            "MCP management unavailable: fix the startup config first"
                        )?,
                    }
                }
                continue;
            }
            Some(commands::Command::Skills(selection)) => {
                let result = (|| -> Result<()> {
                    let mut skills = skills
                        .lock()
                        .map_err(|_| anyhow::anyhow!("skill registry unavailable"))?;
                    match selection {
                        commands::SkillCommand::List | commands::SkillCommand::Sources => {}
                        commands::SkillCommand::Toggle(name, enabled) => {
                            skills.save_selection(&[(name.to_owned(), enabled)])?
                        }
                        commands::SkillCommand::Source(source, enabled) => {
                            skills.save_source(source, enabled)?
                        }
                    }
                    for source in skills::SkillSource::ALL {
                        writeln!(
                            stderr,
                            "[{}] source:{} ({})",
                            if skills.sources.contains(&source) {
                                "x"
                            } else {
                                " "
                            },
                            source.name(),
                            source.roots().join(", ")
                        )?;
                    }
                    for skill in &skills.entries {
                        writeln!(
                            stderr,
                            "[{}] {} — {} ({})",
                            if skill.enabled { "x" } else { " " },
                            skill.name,
                            skill.description,
                            skill.path.display()
                        )?;
                    }
                    for diagnostic in &skills.diagnostics {
                        writeln!(stderr, "skill: {diagnostic}")?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    writeln!(stderr, "error: {error:#}")?;
                }
                continue;
            }
            Some(commands::Command::Help) => {
                writeln!(stderr, "{}", commands::help(&file_commands))?;
                continue;
            }
            Some(commands::Command::Tips) => {
                writeln!(stderr, "{}", tips::text())?;
                continue;
            }
            Some(commands::Command::Stats(arguments)) => {
                let result = (|| -> Result<String> {
                    if stats.is_none() {
                        stats = Some(crate::stats::Worker::new()?);
                    }
                    let worker = stats.as_ref().context("statistics launcher unavailable")?;
                    worker.open(arguments)?;
                    worker
                        .replies
                        .recv()
                        .context("statistics launcher stopped")?
                })();
                match result {
                    Ok(message) => writeln!(stderr, "{message}")?,
                    Err(error) => writeln!(stderr, "error: {error:#}")?,
                }
                continue;
            }
            // 行模式没有会话选择界面；按 ID 恢复仍用启动参数。
            Some(commands::Command::Resume) => {
                writeln!(
                    stderr,
                    "/resume requires the TUI; use pp --resume SESSION_ID."
                )?;
                continue;
            }
            Some(commands::Command::Tree) => {
                writeln!(stderr, "/tree requires the TUI; run pp in a terminal.")?;
                continue;
            }
            Some(commands::Command::Changes) => {
                writeln!(stderr, "/changes requires the TUI; run pp in a terminal.")?;
                continue;
            }
            Some(commands::Command::Plan(_)) => {
                writeln!(stderr, "/plan requires the TUI; run pp in a terminal.")?;
                continue;
            }
            Some(commands::Command::Login | commands::Command::Logout) => {
                writeln!(
                    stderr,
                    "/login and /logout require the TUI; run pp in a terminal."
                )?;
                continue;
            }
            Some(commands::Command::PlanList) => {
                writeln!(stderr, "/plan-list requires the TUI; run pp in a terminal.")?;
                continue;
            }
            _ => {}
        }
        // `/test-*` 不依赖真实配置：启动失败时仍使用内嵌 mock server。
        #[cfg(debug_assertions)]
        if let Some(commands::Command::MockTest(test)) = &command {
            if let Err(error) = run_cli_mock_test(*test, &mut stdout, &mut stderr) {
                if error
                    .chain()
                    .any(|cause| cause.is::<cancellation::CleanupError>())
                {
                    return stop_cli_lsp(agent.as_ref().ok(), Err(error));
                }
                writeln!(stderr, "error: {error:#}")?;
            }
            continue;
        }
        let agent = match &mut agent {
            Ok(agent) => agent,
            Err(error) => {
                writeln!(stderr, "error: {error:#}")?;
                continue;
            }
        };
        match command {
            Some(commands::Command::Pipes(run)) => {
                let listed = cli_pipes_list(&agent.pipe_store(), run);
                write!(stderr, "{listed}")?;
            }
            Some(commands::Command::Shell(command)) => {
                if let Err(error) = run_cli_shell(agent, command, &mut stderr) {
                    if error
                        .chain()
                        .any(|cause| cause.is::<cancellation::CleanupError>())
                    {
                        return stop_cli_lsp(Some(&*agent), Err(error));
                    }
                    writeln!(stderr, "error: {error:#}")?;
                }
            }
            Some(commands::Command::Think(level)) => {
                if let Some(level) = level
                    && let Err(error) = agent.set_think_level(level)
                {
                    writeln!(stderr, "error: {error:#}")?;
                    continue;
                }
                write!(
                    stderr,
                    "Thinking level: {} (options: ",
                    agent.think_level().label()
                )?;
                for (index, level) in agent.current_model().thinking.levels().enumerate() {
                    if index != 0 {
                        write!(stderr, ", ")?;
                    }
                    write!(stderr, "{}", level.as_str())?;
                }
                writeln!(stderr, ")")?;
            }
            Some(commands::Command::Compact) => {
                if let Err(error) = run_cli_compaction(agent, &mut stderr) {
                    writeln!(stderr, "error: {error:#}")?;
                }
            }
            Some(commands::Command::New) => {
                agent.reset();
                writeln!(stderr, "Session reset.")?;
            }
            Some(commands::Command::Model(None)) => {
                if let Err(error) = agent.reload_models() {
                    writeln!(stderr, "error: {error:#}")?;
                }
                for choice in agent.model_choices() {
                    let marker = if choice.reference == agent.current_model().reference {
                        "*"
                    } else {
                        " "
                    };
                    writeln!(
                        stderr,
                        "{marker} {} ({}) [{}]",
                        choice.reference, choice.model, choice.protocol
                    )?;
                }
                writeln!(stderr, "Use /model provider/model to switch.")?;
            }
            Some(commands::Command::Model(Some(reference))) => {
                match agent
                    .reload_models()
                    .and_then(|()| agent.switch_model(reference))
                {
                    Ok(()) => {
                        let choice = agent.current_model();
                        writeln!(
                            stderr,
                            "Current model: {} ({})\nSource: {}",
                            choice.reference,
                            choice.model,
                            agent.model_source().describe()
                        )?;
                    }
                    Err(error) => writeln!(stderr, "error: {error:#}")?,
                }
            }
            Some(
                commands::Command::Help
                | commands::Command::Tips
                | commands::Command::Stats(_)
                | commands::Command::Quit
                | commands::Command::Skills(_)
                | commands::Command::Mcp(_)
                | commands::Command::Resume
                | commands::Command::Tree
                | commands::Command::Changes
                | commands::Command::Roles
                | commands::Command::Subagents
                | commands::Command::PlanList
                | commands::Command::Login
                | commands::Command::Logout
                | commands::Command::Plan(_),
            ) => unreachable!(),
            Some(commands::Command::Unknown) => unreachable!(),
            #[cfg(debug_assertions)]
            Some(commands::Command::MockTest(_)) => unreachable!(),
            Some(commands::Command::Prompt(prompt)) => {
                if let Err(error) = run_cli_task(agent, Some(&prompt), &mut stdout, &mut stderr) {
                    writeln!(stderr, "error: {error:#}")?;
                }
            }
            None => match run_cli_task(agent, Some(prompt), &mut stdout, &mut stderr) {
                Ok(()) => {}
                Err(error) => writeln!(stderr, "error: {error:#}")?,
            },
        }
    }
    let hint = match agent.as_ref().ok().and_then(|agent| agent.session_id()) {
        Some(id) => writeln!(stderr)
            .and_then(|()| write_resume_hint(&mut stderr, Some(id)))
            .map_err(anyhow::Error::from),
        None => Ok(()),
    };
    stop_cli_lsp(agent.as_ref().ok(), hint)
}
