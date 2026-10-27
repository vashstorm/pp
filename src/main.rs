mod agent;
mod cancellation;
mod commands;
mod config;
mod context;
mod events;
mod interaction;
mod llm;
mod logging;
mod mcp;
mod models;
mod plan;
mod project_instructions;
mod shortcuts;
mod skills;
mod stats;
mod system_prompt;
mod tips;
mod tools;
mod tui;
mod yaml;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    io::{self, BufRead, IsTerminal, Write},
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    name = "pp",
    version,
    disable_version_flag = true,
    args_conflicts_with_subcommands = true,
    about = "Minimal coding agent using OpenAI-compatible Responses or Chat Completions",
    after_help = "Modes:\n  pp -p 'task'          run one task and exit\n  pp -c [SESSION_ID]    continue a saved session\n  pp                    interactive session (TUI on a terminal, line mode otherwise)\n\nUse `pp stats --help` for the stats subcommand, or `pp gc` to remove data older than 90 days."
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Print version information
    #[arg(short = 'v', short_alias = 'V', long, action = clap::ArgAction::Version)]
    version: (),

    /// Run one complete task and exit; omit for an interactive session
    #[arg(short, long)]
    prompt: Option<String>,

    /// 替换基础系统指令，参数可为正文或文件路径
    #[arg(long, value_name = "TEXT_OR_FILE", conflicts_with_all = ["system_prompt_template", "resume", "continue_session"])]
    system_prompt: Option<String>,

    /// 从文件读取 Handlebars 基础系统指令模板
    #[arg(long, value_name = "PATH", conflicts_with_all = ["resume", "continue_session"])]
    system_prompt_template: Option<String>,

    /// 追加系统指令，参数可为正文或文件路径
    #[arg(long, value_name = "TEXT_OR_FILE", conflicts_with_all = ["resume", "continue_session"])]
    append_system_prompt: Option<String>,

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

#[derive(Subcommand)]
enum Command {
    /// Remove logs and SQLite request data older than 90 days
    Gc,
    /// Start the local stats page server
    Stats {
        #[arg(long, default_value_t = 3848)]
        port: u16,
    },
}

fn main() -> ExitCode {
    let args = Args::parse();
    // gc reports the existing inventory without creating its own diagnostic log.
    let logging = if matches!(args.command, Some(Command::Gc)) {
        None
    } else {
        match logging::init() {
            Ok(guard) => Some(guard),
            Err(_) => {
                let _ = writeln!(io::stderr().lock(), "warning: file logging unavailable");
                None
            }
        }
    };
    let started = std::time::Instant::now();
    let pid = std::process::id();
    let mode = if args.command.is_some() {
        "stats"
    } else if args.prompt.is_some() {
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

fn run_gc() -> Result<()> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(90);
    let (requests_total, requests_removed) =
        stats::gc(cutoff.timestamp_millis()).context("cleaning statistics database")?;
    // Logs encode local calendar dates, not precise creation timestamps.
    let (logs_total, logs_removed) = logging::gc(cutoff.with_timezone(&chrono::Local).date_naive())
        .context("cleaning log files")?;
    let mut stdout = io::stdout().lock();
    writeln!(
        stdout,
        "Logs: total={logs_total}, removed={logs_removed}, remaining={}",
        logs_total - logs_removed
    )?;
    writeln!(
        stdout,
        "SQLite requests: total={requests_total}, removed={requests_removed}, remaining={}",
        requests_total - requests_removed
    )?;
    Ok(())
}

fn run(args: Args) -> Result<()> {
    if let Some(command) = args.command {
        return match command {
            Command::Gc => run_gc(),
            Command::Stats { port } => stats::server::run(port),
        };
    }
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
        .and_then(|dir| config::read_settings(&dir.join("settings.yml")).ok())
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
    if !use_tui
        && restored
            .as_ref()
            .is_some_and(|session| session.work_mode() == plan::WorkMode::Plan)
    {
        anyhow::bail!("Saved session is in Plan mode; resume it in the TUI.");
    }
    let cwd = match &restored {
        Some(session) => {
            std::env::set_current_dir(session.cwd())
                .context("restoring session working directory")?;
            std::env::current_dir().context("restoring session working directory")?
        }
        None => std::env::current_dir().context("capturing startup working directory")?,
    };
    // 提示词错误必须在交互兜底之前返回，不能静默禁用或替换用户设定。
    let prompt_source = if restored.is_none() {
        global_dir
            .as_ref()
            .ok()
            .map(|dir| {
                system_prompt::Snapshot::discover(
                    &cwd,
                    dir,
                    args.system_prompt.as_deref(),
                    args.system_prompt_template.as_deref(),
                    args.append_system_prompt.as_deref(),
                )
            })
            .transpose()?
    } else {
        None
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
                &global_dir.join("settings.yml"),
                &global_dir.join("models.yml"),
            )
            .context("loading configuration for saved session")?;
            let mut tools = tools::Tools::new(cwd.clone(), session.edit_mode())?;
            tools.skills = skills.clone();
            Ok(agent::Agent::restore(catalog, tools, session)?)
        }
        None => global_dir.and_then(|dir| {
            config::ModelCatalog::from_files(&dir.join("settings.yml"), &dir.join("models.yml"))
                .and_then(|catalog| {
                    let mut tools = tools::Tools::new(cwd.clone(), catalog.edit_mode())?;
                    tools.skills = skills.clone();
                    agent::Agent::new_with_prompt(
                        catalog,
                        tools,
                        dir.join("sessions"),
                        prompt_source.expect("global directory resolved"),
                    )
                })
        }),
    };
    if agent
        .as_ref()
        .err()
        .is_some_and(|error| error.is::<system_prompt::PromptFailure>())
    {
        return agent.map(|_| ());
    }
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
        "bash runs with an execution timeout that kills the process group; Ctrl + C exits the whole program."
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
        let command = match commands::parse(line.trim_end_matches(['\r', '\n']), &file_commands) {
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
            Some(commands::Command::Shell(command)) => {
                if let Err(error) = run_cli_shell(agent, command, &mut stderr) {
                    if error
                        .chain()
                        .any(|cause| cause.is::<cancellation::CleanupError>())
                    {
                        return Err(error);
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
                | commands::Command::Tips
                | commands::Command::Quit
                | commands::Command::Skills(_)
                | commands::Command::Resume
                | commands::Command::Tree
                | commands::Command::Changes
                | commands::Command::Plan(_),
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

fn run_cli_shell(agent: &mut agent::Agent, command: &str, stderr: &mut impl Write) -> Result<()> {
    use events::{AgentEvent, OutputStream};
    let mut broken = false;
    let mut first_error = None;
    let mut newline = true;
    let result = agent.run_shell(
        command,
        "local-shell-cli",
        &cancellation::CancellationToken::new(),
        &mut |event| match event {
            AgentEvent::ToolStarted { .. } => {
                cli_write(stderr, "shell: running\n", &mut broken, &mut first_error)
            }
            AgentEvent::ToolOutput { stream, text, .. } => {
                let label = match stream {
                    OutputStream::Stdout => "stdout",
                    OutputStream::Stderr => "stderr",
                };
                cli_write(
                    stderr,
                    &format!("[shell {label}] {text}"),
                    &mut broken,
                    &mut first_error,
                );
                newline = text.ends_with('\n');
            }
            AgentEvent::ToolFinished { result, .. } => {
                if !newline {
                    cli_write(stderr, "\n", &mut broken, &mut first_error);
                }
                let status = if result["cancelled"] == true {
                    "shell: cancelled".to_owned()
                } else if result["timed_out"] == true {
                    "shell: timed out".to_owned()
                } else if let Some(code) = result["exit_code"].as_i64() {
                    format!("shell: exit {code}")
                } else {
                    "shell: failed".to_owned()
                };
                cli_write(
                    stderr,
                    &format!("{status}\n"),
                    &mut broken,
                    &mut first_error,
                );
                for key in ["error", "warning"] {
                    if let Some(message) = result[key].as_str() {
                        cli_write(
                            stderr,
                            &format!("shell: {message}\n"),
                            &mut broken,
                            &mut first_error,
                        );
                    }
                }
                if let Some(warnings) = result["warnings"].as_array() {
                    for warning in warnings.iter().filter_map(serde_json::Value::as_str) {
                        cli_write(
                            stderr,
                            &format!("shell: {warning}\n"),
                            &mut broken,
                            &mut first_error,
                        );
                    }
                }
                if result["stdout_truncated"] == true
                    || result["stderr_truncated"] == true
                    || result["truncated"] == true
                {
                    cli_write(
                        stderr,
                        "shell: output truncated\n",
                        &mut broken,
                        &mut first_error,
                    );
                }
            }
            _ => {}
        },
    );
    if result.as_ref().is_err_and(|error| {
        error
            .chain()
            .any(|cause| cause.is::<cancellation::CleanupError>())
    }) {
        return result;
    }
    if let Some(error) = first_error {
        return Err(error).context("writing shell output");
    }
    result
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
        // 运行时指标事件只服务 TUI，CLI 不输出，也不得触发思考换行。
        if matches!(
            &event,
            AgentEvent::ModelRequestStarted
                | AgentEvent::ModelOutputProgress { .. }
                | AgentEvent::ModelRequestFinished { .. }
                | AgentEvent::UserInputAccepted { .. }
                | AgentEvent::PlanProgressUpdated { .. }
        ) {
            return;
        }
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
                if current_tool.as_ref().is_some_and(|(current_id, name)| {
                    current_id == &id && matches!(name.as_str(), "grep" | "glob")
                }) && let Some(content) = result["content"]
                    .as_str()
                    .or_else(|| result["error"].as_str())
                {
                    cli_write(
                        stderr,
                        &format!("{content}\n"),
                        &mut err_broken,
                        &mut first_error,
                    );
                    if let Some(warnings) = result["warnings"].as_array() {
                        for warning in warnings.iter().filter_map(serde_json::Value::as_str) {
                            cli_write(
                                stderr,
                                &format!("Warning: {warning}\n"),
                                &mut err_broken,
                                &mut first_error,
                            );
                        }
                    }
                }
                if result["kind"] == "image"
                    && let Some(content) = result["content"].as_str()
                {
                    cli_write(
                        stderr,
                        &format!("{content}\n"),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
                if let (Some(task_id), Some(status)) =
                    (result["task_id"].as_str(), result["status"].as_str())
                {
                    cli_write(
                        stderr,
                        &format!("task {task_id}: {status}\n"),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
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
            AgentEvent::RateLimited { attempt, wait_ms } => {
                cli_write(
                    stderr,
                    &format!(
                        "rate limited; retrying in {} (attempt {attempt})\n",
                        crate::tui::format_wait(wait_ms)
                    ),
                    &mut err_broken,
                    &mut first_error,
                );
            }
            AgentEvent::ModelRequestStarted
            | AgentEvent::ModelOutputProgress { .. }
            | AgentEvent::ModelRequestFinished { .. }
            | AgentEvent::UserInputAccepted { .. }
            | AgentEvent::PlanProgressUpdated { .. } => {}
        }
    };
    let cancellation = cancellation::CancellationToken::new();
    let result = match prompt {
        Some(prompt) => agent.run(prompt, &cancellation, None, &mut emit),
        None => agent.continue_session(&cancellation, None, &mut emit),
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
