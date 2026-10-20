mod agent;
mod cancellation;
mod commands;
mod config;
mod context;
mod events;
mod interaction;
mod jsonc;
mod llm;
mod logging;
mod mcp;
mod models;
mod project_instructions;
mod skills;
mod tools;
mod tui;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use std::{
    io::{self, BufRead, IsTerminal, Write},
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    version,
    about = "Minimal coding agent using OpenAI-compatible Responses or Chat Completions"
)]
struct Args {
    /// Run one complete task and exit; omit for an interactive session
    #[arg(short, long)]
    prompt: Option<String>,

    /// Resume a saved session by exact ID and wait for input
    #[arg(
        short = 'r',
        long = "resume",
        value_name = "SESSION_ID",
        conflicts_with = "continue_session"
    )]
    resume: Option<String>,

    /// Continue a saved session by exact ID, or the most recently modified one
    #[arg(
        short = 'c',
        long = "continue",
        value_name = "SESSION_ID",
        num_args = 0..=1,
        conflicts_with_all = ["resume", "prompt"]
    )]
    continue_session: Option<Option<String>>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let logging = match logging::init() {
        Ok(guard) => Some(guard),
        Err(_) => {
            let _ = writeln!(io::stderr().lock(), "warning: file logging unavailable");
            None
        }
    };
    let started = std::time::Instant::now();
    let pid = std::process::id();
    let mode = if args.prompt.is_some() {
        "prompt"
    } else if io::stdin().is_terminal() && io::stdout().is_terminal() {
        "tui"
    } else {
        "line"
    };
    let cwd = std::env::current_dir().ok();
    tracing::info!(
        event = "app_started",
        pid,
        mode,
        cwd = cwd
            .as_ref()
            .map(|path| tracing::field::display(path.display()))
    );
    let result = run(args);
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if result.is_ok() {
        tracing::info!(event = "app_stopped", pid, status = "ok", elapsed_ms);
    } else {
        tracing::error!(event = "app_stopped", pid, status = "error", elapsed_ms);
    }
    if logging
        .as_ref()
        .and_then(logging::LoggingGuard::take_error)
        .is_some()
    {
        let _ = writeln!(io::stderr().lock(), "warning: file logging unavailable");
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<()> {
    if let Some(prompt) = &args.prompt {
        ensure!(!prompt.trim().is_empty(), "prompt must not be blank");
    }
    let use_tui = args.prompt.is_none() && io::stdin().is_terminal() && io::stdout().is_terminal();
    let continue_requested = args.continue_session.is_some();
    let resume_id = args
        .resume
        .as_deref()
        .or_else(|| args.continue_session.as_ref().and_then(|id| id.as_deref()));
    if let Some(id) = resume_id {
        agent::persistence::validate_id(id)?;
    }
    // Restore failures are fatal even in interactive mode, unlike unavailable
    // configuration for a fresh `pp`. Load the journal before tools, skills, and
    // the TUI workspace so everything runs from the saved working directory.
    let global_dir = config::global_dir();
    // 通知设置独立于模型初始化；无效设置的诊断仍由既有启动路径呈现。
    let notification_options = global_dir
        .as_ref()
        .ok()
        .filter(|_| use_tui)
        .and_then(|dir| config::read_settings(&dir.join("settings.jsonc")).ok())
        .map(|settings| settings.notification_options())
        .unwrap_or_default();
    let file_commands = if args.prompt.is_none() {
        match &global_dir {
            Ok(dir) => commands::FileCommands::from_dir(dir),
            Err(error) => commands::FileCommands {
                diagnostics: vec![format!("{error:#}")],
                ..commands::FileCommands::default()
            },
        }
    } else {
        commands::FileCommands::default()
    };
    let restored = if args.resume.is_some() || continue_requested {
        let dir = global_dir
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        Some(agent::persistence::load(
            &dir.join("sessions"),
            resume_id,
            use_tui,
        )?)
    } else {
        None
    };
    let cwd = match &restored {
        Some(session) => {
            std::env::set_current_dir(session.cwd())
                .context("restoring session working directory")?;
            std::env::current_dir().context("restoring session working directory")?
        }
        None => std::env::current_dir().context("capturing startup working directory")?,
    };
    let skills = match &global_dir {
        Ok(dir) => skills::Skills::from_dir(dir),
        Err(error) => {
            let mut skills = skills::Skills::default();
            skills.diagnostics.push(format!("{error:#}"));
            skills
        }
    }
    .shared();
    let mut agent = match restored {
        Some(session) => {
            let global_dir = global_dir?;
            let catalog = config::ModelCatalog::from_files(
                &global_dir.join("settings.jsonc"),
                &global_dir.join("models.jsonc"),
            )
            .context("loading configuration for saved session")?;
            let mut tools = tools::Tools::new(cwd.clone(), session.edit_mode())?;
            tools.skills = skills.clone();
            Ok(agent::Agent::restore(catalog, tools, session)?)
        }
        None => global_dir.and_then(|dir| {
            config::ModelCatalog::from_files(&dir.join("settings.jsonc"), &dir.join("models.jsonc"))
                .and_then(|catalog| {
                    let mut tools = tools::Tools::new(cwd.clone(), catalog.edit_mode())?;
                    tools.skills = skills.clone();
                    agent::Agent::new(catalog, tools, dir.join("sessions"))
                })
        }),
    };
    if agent.is_err() {
        tracing::warn!(
            event = "configuration_unavailable",
            pid = std::process::id()
        );
    }
    if use_tui {
        return tui::run(
            agent,
            &cwd,
            skills,
            file_commands,
            continue_requested,
            notification_options,
        );
    }
    let resumed_id = if args.resume.is_some() || continue_requested {
        agent.as_ref().ok().and_then(|agent| agent.session_id())
    } else {
        None
    };
    let mut stderr = io::stderr();
    let mut stdout = io::stdout().lock();
    writeln!(
        stderr,
        "write/edit only modify files inside the startup directory; bash has no directory isolation, so run in a trusted or temporary directory."
    )?;
    writeln!(
        stderr,
        "bash runs with an execution timeout that kills the process group; Ctrl-C exits the whole program."
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
    if let Some(id) = &resumed_id {
        writeln!(stderr, "Resumed session: {id}")?;
    }
    if let Some(prompt) = args.prompt {
        run_cli_task(&mut agent?, Some(&prompt), &mut stdout, &mut stderr)?;
        return Ok(());
    }
    if continue_requested {
        let active = agent
            .as_mut()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        run_cli_task(active, None, &mut stdout, &mut stderr)?;
    }
    writeln!(stderr, "pp — use /help for commands")?;
    if let Err(error) = &agent {
        writeln!(stderr, "error: {error:#}")?;
        writeln!(
            stderr,
            "Configuration unavailable; model tasks are disabled. Fix the config files and restart pp. /quit still exits cleanly."
        )?;
    }
    let mut stdin = io::stdin().lock();
    let mut line = String::new();
    loop {
        write!(stderr, "> ")?;
        stderr.flush()?;
        line.clear();
        if stdin.read_line(&mut line).context("reading stdin")? == 0 {
            break;
        }
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        let command = match commands::parse(prompt, &file_commands) {
            Ok(command) => command,
            Err(error) => {
                writeln!(stderr, "error: {error:#}")?;
                continue;
            }
        };
        match command {
            Some(commands::Command::Quit) => break,
            Some(commands::Command::Skills(selection)) => {
                let result = (|| -> Result<()> {
                    let mut skills = skills
                        .lock()
                        .map_err(|_| anyhow::anyhow!("skill registry unavailable"))?;
                    if let Some((name, enabled)) = selection {
                        skills.save_selection(&[(name.to_owned(), enabled)])?;
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
            // 行模式没有会话选择界面；按 ID 恢复仍用启动参数。
            Some(commands::Command::Resume) => {
                writeln!(
                    stderr,
                    "/resume requires the TUI; use pp --resume SESSION_ID."
                )?;
                continue;
            }
            _ => {}
        }
        let agent = match &mut agent {
            Ok(agent) => agent,
            Err(error) => {
                writeln!(stderr, "error: {error:#}")?;
                continue;
            }
        };
        match command {
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
                match agent.switch_model(reference) {
                    Ok(()) => {
                        let choice = agent.current_model();
                        writeln!(
                            stderr,
                            "Current model: {} ({})",
                            choice.reference, choice.model
                        )?;
                    }
                    Err(error) => writeln!(stderr, "error: {error:#}")?,
                }
            }
            Some(
                commands::Command::Help
                | commands::Command::Quit
                | commands::Command::Skills(_)
                | commands::Command::Resume,
            ) => unreachable!(),
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
    Ok(())
}

// Display failures must not interrupt recording results of tools that already ran.
fn cli_write(
    writer: &mut impl Write,
    text: &str,
    broken: &mut bool,
    first_error: &mut Option<io::Error>,
) {
    if *broken {
        return;
    }
    if let Err(error) = writer
        .write_all(text.as_bytes())
        .and_then(|()| writer.flush())
    {
        *broken = true;
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

fn cli_context_event(
    event: events::AgentEvent,
    usage: &mut Option<context::ContextUsage>,
    stderr: &mut impl Write,
    broken: &mut bool,
    first_error: &mut Option<io::Error>,
) {
    match event {
        events::AgentEvent::ContextUpdated { usage: current } => *usage = Some(current),
        events::AgentEvent::CompactionStarted { .. } => {
            cli_write(stderr, "Compacting context...\n", broken, first_error)
        }
        events::AgentEvent::CompactionFinished {
            before_tokens,
            after_tokens,
        } => cli_write(
            stderr,
            &format!("Context compacted: ~{before_tokens} → ~{after_tokens} tokens (estimated)\n"),
            broken,
            first_error,
        ),
        _ => {}
    }
}

fn run_cli_compaction(agent: &mut agent::Agent, stderr: &mut impl Write) -> Result<()> {
    use agent::context::CompactionOutcome;
    let mut usage = None;
    let mut broken = false;
    let mut first_error = None;
    let result = agent.compact(&cancellation::CancellationToken::new(), &mut |event| {
        cli_context_event(event, &mut usage, stderr, &mut broken, &mut first_error);
    });
    match &result {
        Ok(CompactionOutcome::Unchanged) => cli_write(
            stderr,
            "Nothing to compact.\n",
            &mut broken,
            &mut first_error,
        ),
        Ok(CompactionOutcome::Cancelled) => cli_write(
            stderr,
            "Context compaction cancelled.\n",
            &mut broken,
            &mut first_error,
        ),
        _ => {}
    }
    if let Some(id) = agent.take_session_notice() {
        cli_write(
            stderr,
            &format!("Session: {id}\n"),
            &mut broken,
            &mut first_error,
        );
    }
    let usage = match usage {
        Some(usage) => usage,
        None => agent.context_usage()?,
    };
    cli_write(
        stderr,
        &usage.cli_status(&agent.current_model().reference),
        &mut broken,
        &mut first_error,
    );
    if let Some(error) = first_error {
        return Err(error).context("writing context output");
    }
    result.map(|_| ())
}

fn run_cli_task(
    agent: &mut agent::Agent,
    prompt: Option<&str>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<()> {
    use events::{AgentEvent, OutputStream};
    let mut first_error = None;
    let (mut out_broken, mut err_broken) = (false, false);
    let (mut visible, mut content, mut newline) = (false, false, true);
    let mut current_tool = None;
    let mut thinking_newline = true;
    let mut context_usage = None;
    let mut emit = |event: AgentEvent| {
        if !matches!(&event, AgentEvent::ThinkingDelta { .. }) && !thinking_newline {
            cli_write(stderr, "\n", &mut err_broken, &mut first_error);
            thinking_newline = true;
        }
        match event {
            event @ (AgentEvent::ContextUpdated { .. }
            | AgentEvent::CompactionStarted { .. }
            | AgentEvent::CompactionFinished { .. }) => cli_context_event(
                event,
                &mut context_usage,
                stderr,
                &mut err_broken,
                &mut first_error,
            ),
            AgentEvent::AssistantStarted => {
                visible = false;
                content = false;
                newline = true;
            }
            AgentEvent::ThinkingDelta { text } => {
                if !text.is_empty() {
                    thinking_newline = text.ends_with('\n');
                }
                cli_write(stderr, &text, &mut err_broken, &mut first_error);
            }
            AgentEvent::AssistantDelta {
                text,
                refusal: false,
            } => {
                if !text.is_empty() {
                    cli_write(stdout, &text, &mut out_broken, &mut first_error);
                    visible = true;
                    content |= !text.trim().is_empty();
                    newline = text.ends_with('\n');
                }
            }
            AgentEvent::AssistantDelta { refusal: true, .. } => {}
            AgentEvent::AssistantFinished { text } => {
                if !content && !text.is_empty() {
                    cli_write(stdout, &text, &mut out_broken, &mut first_error);
                    visible = true;
                    newline = text.ends_with('\n');
                }
                if visible && !newline {
                    cli_write(stdout, "\n", &mut out_broken, &mut first_error);
                }
                visible = false;
                newline = true;
            }
            AgentEvent::ToolStarted { id, name, .. } => {
                cli_write(
                    stderr,
                    &format!("tool {name}: running\n"),
                    &mut err_broken,
                    &mut first_error,
                );
                current_tool = Some((id, name));
            }
            AgentEvent::FileChanged { .. } => {}
            AgentEvent::ToolOutput { id, stream, text } => {
                let name = current_tool
                    .as_ref()
                    .filter(|(current_id, _)| current_id == &id)
                    .map_or("tool", |(_, name)| name.as_str());
                let label = match stream {
                    OutputStream::Stdout => "stdout",
                    OutputStream::Stderr => "stderr",
                };
                cli_write(
                    stderr,
                    &format!("[{name} {label}] {text}"),
                    &mut err_broken,
                    &mut first_error,
                );
            }
            AgentEvent::ToolFinished { id, result } => {
                if let Some((current_id, name)) = current_tool.take()
                    && current_id == id
                {
                    let status = if result["ok"] == true { "ok" } else { "failed" };
                    cli_write(
                        stderr,
                        &format!("\ntool {name}: {status}\n"),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
            }
        }
    };
    let cancellation = cancellation::CancellationToken::new();
    let result = match prompt {
        Some(prompt) => agent.run(prompt, &cancellation, &mut emit),
        None => agent.continue_session(&cancellation, &mut emit),
    }
    .map(|_| ());
    if !thinking_newline {
        cli_write(stderr, "\n", &mut err_broken, &mut first_error);
    }
    if visible && !newline {
        cli_write(stdout, "\n", &mut out_broken, &mut first_error);
    }
    // Report a newly created session even when the task itself failed; stdout
    // stays reserved for model answers.
    if let Some(id) = agent.take_session_notice() {
        cli_write(
            stderr,
            &format!("Session: {id}\n"),
            &mut err_broken,
            &mut first_error,
        );
    }
    if let Some(usage) = context_usage {
        cli_write(
            stderr,
            &usage.cli_status(&agent.current_model().reference),
            &mut err_broken,
            &mut first_error,
        );
    }
    if let Some(error) = first_error {
        return Err(error).context("writing task output");
    }
    result
}
