use pp_config::agent_store;
use pp_config::config;
use pp_core::cancellation;
use pp_runtime::agent;
use pp_runtime::host_defaults;
use pp_ui::cli;
use pp_ui::commands;

use pp_core::logging;
use pp_core::plan;

use pp_config::skills;
use pp_runtime::system_prompt;
use pp_stats as stats;

use pp_runtime::tools;
use pp_ui::tui;

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
};

#[derive(Parser)]
#[command(
    name = "pp",
    version,
    disable_version_flag = true,
    args_conflicts_with_subcommands = true,
    about = "Minimal coding agent using OpenAI-compatible Responses or Chat Completions",
    after_help = "Modes:\n  pp -p 'task'          run one task and exit\n  pp -r [SESSION_ID]    open the resume picker, or resume a saved session by ID\n  pp -c [SESSION_ID]    continue a saved session\n  pp /COMMAND           run a slash command immediately after startup\n  pp                    interactive session (TUI on a terminal, line mode otherwise)\n\nUse `/login` and `/logout` in the TUI to manage subscription credentials, `pp stats --help` for statistics, or `pp gc [DAYS]` to remove data older than the given days (default 90)."
)]
struct Args {
    #[command(subcommand)]
    subcommand: Option<Command>,

    /// 启动后立即执行的一条斜杠命令（例如 /commit）；与 -p、-r、-c 互斥
    #[arg(
        value_name = "COMMAND",
        num_args = 1..,
        conflicts_with_all = ["prompt", "resume", "continue_session"]
    )]
    command: Vec<String>,

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

    /// Open the resume session picker, or resume a saved session by exact ID
    #[arg(
        short = 'r',
        long = "resume",
        value_name = "SESSION_ID",
        num_args = 0..=1,
        conflicts_with = "continue_session"
    )]
    resume: Option<Option<String>>,

    /// Continue a saved session by exact ID, or the most recently modified one
    #[arg(
        short = 'c',
        long = "continue",
        value_name = "SESSION_ID",
        num_args = 0..=1,
        conflicts_with_all = ["resume", "prompt"]
    )]
    continue_session: Option<Option<String>>,

    /// Override tools.approvalMode from settings.yml for this run
    #[arg(long, value_enum, value_name = "MODE")]
    approval_mode: Option<ApprovalModeCli>,

    /// 本次运行使用的模型，写法为 [provider/]model[:effort]；不写入偏好
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ApprovalModeCli {
    Yolo,
    Write,
    AlwaysAsk,
}

impl From<ApprovalModeCli> for pp_config::config::ApprovalMode {
    fn from(value: ApprovalModeCli) -> Self {
        match value {
            ApprovalModeCli::Yolo => Self::Yolo,
            ApprovalModeCli::Write => Self::Write,
            ApprovalModeCli::AlwaysAsk => Self::AlwaysAsk,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// 清理超过指定天数（默认 90 天，上限 36500）的日志、SQLite 请求数据和未被占用的 session 锁文件
    Gc {
        // 100 年上限：远低于 chrono DateTime 可表示的年代下限，避免 cutoff 运算溢出。
        #[arg(
            default_value_t = 90,
            value_parser = clap::value_parser!(u32).range(1..=36_500)
        )]
        days: u32,
    },
    /// Start the stats page server
    Stats {
        /// Listen on a specific IPv4 or IPv6 address
        #[arg(long, default_value = "127.0.0.1", value_name = "IP")]
        host: std::net::IpAddr,
        #[arg(long, default_value_t = 3848)]
        port: u16,
        /// 仅启动服务，不自动打开浏览器
        #[arg(long)]
        no_open: bool,
    },
}

fn main() -> ExitCode {
    pp_runtime::raise_file_limit();
    host_defaults::install(host_defaults::capture());
    let args = Args::parse();
    // gc reports the existing inventory without creating its own diagnostic log.
    let logging = if matches!(args.subcommand, Some(Command::Gc { .. })) {
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
    let mode = if args.subcommand.is_some() {
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
    } else if let Err(error) = &result {
        tracing::error!(
            event = "app_stopped",
            pid,
            status = "error",
            elapsed_ms,
            error = logging::diagnostic_text(&format!("{error:#}"))
        );
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

fn run_gc(days: u32) -> Result<()> {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(days as i64);
    let (requests_total, requests_removed) =
        stats::gc(cutoff.timestamp_millis()).context("cleaning statistics database")?;
    // Logs encode local calendar dates, not precise creation timestamps.
    let (logs_total, logs_removed) = logging::gc(cutoff.with_timezone(&chrono::Local).date_naive())
        .context("cleaning log files")?;
    let (locks_total, locks_removed) =
        agent::persistence::locks::gc(cutoff.into()).context("cleaning session lock files")?;
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
    writeln!(
        stdout,
        "Session locks: total={locks_total}, removed={locks_removed}, remaining={}",
        locks_total - locks_removed
    )?;
    Ok(())
}

fn run(args: Args) -> Result<()> {
    if let Some(subcommand) = args.subcommand {
        return match subcommand {
            Command::Gc { days } => run_gc(days),
            Command::Stats {
                host,
                port,
                no_open,
            } => stats::server::run(host, port, !no_open),
        };
    }
    // 位置参数按单空格拼回一条斜杠命令；参数校验先于配置与网络，脚本能立即拿到失败。
    let initial_command = {
        let joined = args.command.join(" ");
        let trimmed = joined.trim();
        if trimmed.is_empty() {
            None
        } else {
            ensure!(
                trimmed.starts_with('/') || trimmed.starts_with('!'),
                "the positional COMMAND must start with '/' or '!' (use -p TEXT for a one-shot prompt)"
            );
            Some(trimmed.to_string())
        }
    };
    #[cfg(debug_assertions)]
    let _mock_server = {
        let server = pp_runtime::mock_ai::MockServer::start()?;
        pp_runtime::mock_ai::install_endpoint(server.endpoint())?;
        server
    };
    if let Some(prompt) = &args.prompt {
        ensure!(!prompt.trim().is_empty(), "prompt must not be blank");
    }
    let use_tui = args.prompt.is_none() && io::stdin().is_terminal() && io::stdout().is_terminal();
    let continue_requested = args.continue_session.is_some();
    let resume_id = args
        .resume
        .as_ref()
        .and_then(|id| id.as_deref())
        .or_else(|| args.continue_session.as_ref().and_then(|id| id.as_deref()));
    // `pp -r` without an ID opens the TUI directly on the resume session picker;
    // it still requires an interactive terminal like any other TUI entry.
    let resume_picker = args.resume.is_some() && resume_id.is_none();
    if resume_picker && (args.prompt.is_some() || !use_tui) {
        anyhow::bail!(
            "`-r` without a session ID opens the session picker and requires an interactive terminal; use `pp -r SESSION_ID` to resume directly."
        );
    }
    if let Some(id) = resume_id {
        agent::persistence::validate_id(id)?;
    }
    // Restore failures are fatal even in interactive mode, unlike unavailable
    // configuration for a fresh `pp`. Load the journal before tools, skills, and
    // the TUI workspace so everything runs from the saved working directory.
    let global_dir = config::global_dir();
    let migration_error = global_dir.as_ref().ok().and_then(|dir| {
        agent_store::initialize(dir, &cancellation::CancellationToken::new()).err()
    });
    // 显示设置独立于模型初始化；无效设置的诊断仍由既有启动路径呈现。
    let tui_settings = global_dir
        .as_ref()
        .ok()
        .filter(|_| use_tui)
        .and_then(|dir| config::read_settings(&dir.join("settings.yml")).ok())
        .map(|settings| settings.tui)
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
    let restored = if resume_id.is_some() || continue_requested {
        let dir = global_dir
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        Some(agent::persistence::load(
            &dir.join("sessions"),
            &agent::persistence::locks::directory()?,
            resume_id,
            // 加载记录用于会话级去重，CLI 恢复同样需要有效回放。
            true,
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
    let bootstrap = global_dir.as_ref().ok().map(|dir| agent::Bootstrap {
        dir: dir.clone(),
        cwd: cwd.clone(),
        skills: skills.clone(),
        prompt: prompt_source
            .clone()
            .unwrap_or_else(system_prompt::Snapshot::bundled),
        approval: args.approval_mode.map(Into::into),
    });
    let mut agent = if let Some(error) = migration_error {
        Err(error)
    } else {
        match restored {
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
            None => bootstrap
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Unable to locate global configuration directory"))
                .and_then(agent::Bootstrap::build),
        }
    };
    if agent
        .as_ref()
        .err()
        .is_some_and(|error| error.is::<system_prompt::PromptFailure>())
    {
        return agent.map(|_| ());
    }
    if let Err(error) = &agent {
        tracing::warn!(
            event = "configuration_unavailable",
            pid = std::process::id(),
            error = logging::diagnostic_text(&format!("{error:#}")),
        );
    }
    if let (Some(mode), Ok(agent)) = (args.approval_mode, agent.as_mut()) {
        agent.set_approval_override(mode.into());
    }
    // `--model` 只在本次运行生效；显式给出却无法解析时必须致命，不静默回退默认模型。
    // 配置本身不可用时 `agent` 为 Err，此时没有 catalog 可校验，沿用既有交互修复流程。
    if let (Some(query), Ok(agent)) = (args.model.as_deref(), agent.as_mut()) {
        let selection = agent.select_model(query)?;
        if !use_tui {
            writeln!(
                io::stderr().lock(),
                "Model: {} (effort {})",
                selection.reference,
                selection.effort.as_str()
            )?;
        }
    }
    if use_tui {
        return tui::run(
            agent,
            &cwd,
            skills,
            file_commands,
            continue_requested,
            resume_picker,
            tui_settings,
            bootstrap,
            initial_command,
        );
    }
    if let Ok(agent) = &mut agent {
        // 去重状态已从有效回放重建；CLI 不保留展示正文。
        drop(agent.take_replay());
    }
    let resumed_id = if resume_id.is_some() || continue_requested {
        agent.as_ref().ok().and_then(|agent| agent.session_id())
    } else {
        None
    };
    let mut stderr = io::stderr();
    cli::prelude(&skills, &file_commands, resumed_id, &mut stderr)?;
    if let Some(prompt) = args.prompt {
        let mut stdout = io::stdout().lock();
        let mut agent = agent?;
        // -p 不启动空闲轮询，也不等待迟到诊断；任务结束即显式回收服务器。
        let task = cli::run_cli_task(&mut agent, Some(&prompt), &mut stdout, &mut stderr);
        let hint =
            cli::write_resume_hint(&mut stderr, agent.session_id()).map_err(anyhow::Error::from);
        let result = match (task, hint) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        };
        return cli::stop_cli_lsp(Some(&agent), result);
    }
    if continue_requested {
        let mut stdout = io::stdout().lock();
        let active = agent
            .as_mut()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        if let Err(error) = cli::run_cli_task(active, None, &mut stdout, &mut stderr) {
            return cli::stop_cli_lsp(Some(&*active), Err(error));
        }
    }
    cli::run_line_mode(agent, skills, file_commands, initial_command)
}
