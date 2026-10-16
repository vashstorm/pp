mod agent;
mod commands;
mod config;
mod events;
mod jsonc;
mod llm;
mod models;
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
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
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
    let cwd = std::env::current_dir().context("capturing startup working directory")?;
    let mut agent =
        config::ModelCatalog::from_files(&cwd.join("settings.jsonc"), &cwd.join("models.jsonc"))
            .and_then(|catalog| {
                let edit_mode = catalog.edit_mode();
                agent::Agent::new(catalog, tools::Tools::new(cwd.clone(), edit_mode)?)
            });
    if use_tui {
        return tui::run(agent, &cwd);
    }
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
    if let Some(prompt) = args.prompt {
        run_cli_task(&mut agent?, &prompt, &mut stdout, &mut stderr)?;
        return Ok(());
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
        let command = match commands::parse(prompt) {
            Ok(command) => command,
            Err(error) => {
                writeln!(stderr, "error: {error:#}")?;
                continue;
            }
        };
        match command {
            Some(commands::Command::Quit) => break,
            Some(commands::Command::Help) => {
                writeln!(stderr, "{}", commands::help())?;
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
                if let Some(level) = level {
                    agent.set_think_level(level);
                }
                writeln!(
                    stderr,
                    "Thinking level: {} (options: {})",
                    agent.think_level().as_str(),
                    config::ThinkLevel::OPTIONS
                )?;
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
            Some(commands::Command::Help | commands::Command::Quit) => unreachable!(),
            None => match run_cli_task(agent, prompt, &mut stdout, &mut stderr) {
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

fn run_cli_task(
    agent: &mut agent::Agent,
    prompt: &str,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<()> {
    use events::{AgentEvent, OutputStream};
    let mut first_error = None;
    let (mut out_broken, mut err_broken) = (false, false);
    let (mut visible, mut content, mut newline) = (false, false, true);
    let mut current_tool = None;
    let result = agent.run(prompt, &mut |event| match event {
        AgentEvent::AssistantStarted => {
            visible = false;
            content = false;
            newline = true;
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
        AgentEvent::ToolOutput { stream, text, .. } => {
            let label = match stream {
                OutputStream::Stdout => "stdout",
                OutputStream::Stderr => "stderr",
            };
            cli_write(
                stderr,
                &format!("[bash {label}] {text}"),
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
    });
    if visible && !newline {
        cli_write(stdout, "\n", &mut out_broken, &mut first_error);
    }
    if let Some(error) = first_error {
        return Err(error).context("writing task output");
    }
    result
}
