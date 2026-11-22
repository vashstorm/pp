use pp_config::agent_store;
use pp_config::config;
use pp_core::cancellation;
use pp_runtime::agent;
use pp_ui::cli;
use pp_ui::commands;
use pp_workspace::host_defaults;

use pp_core::logging;
use pp_core::plan;

use pp_config::skills;
use pp_runtime::system_prompt;
use pp_stats as stats;

use pp_ui::tui;

use anyhow::{Context, Result, ensure};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
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
    after_help = "Modes:\n  pp -p 'task'          run one task non-interactively and exit\n  pp -p -r ID 'task'    resume session ID and run a new non-interactive prompt\n  pp -r [SESSION_ID]    open the resume picker, or resume a saved session by ID\n  pp -c [SESSION_ID]    continue a saved session\n  pp /COMMAND           run a slash command immediately after startup\n  pp                    interactive session (TUI on a terminal, line mode otherwise)\n\nUse `/login` and `/logout` in the TUI to manage subscription credentials, `pp stats --help` for statistics, or `pp gc [DAYS]` to remove data older than the given days (default 90)."
)]
struct Args {
    #[command(subcommand)]
    subcommand: Option<Command>,

    /// Run a slash command immediately after startup (e.g. /commit), or provide the prompt in -p mode
    #[arg(
        value_name = "COMMAND_OR_PROMPT",
        num_args = 1..,
        trailing_var_arg = true,
        allow_hyphen_values = true,
        conflicts_with = "continue_session"
    )]
    command: Vec<String>,

    /// Print version information
    #[arg(short = 'v', short_alias = 'V', long, action = clap::ArgAction::Version)]
    version: (),

    /// Run one complete task non-interactively and exit; omit for an interactive session
    #[arg(short = 'p', long = "print", alias = "prompt", num_args = 0..=1, value_name = "PROMPT")]
    prompt: Option<Option<String>>,

    /// Output format for non-interactive -p runs (text, json, stream-json)
    #[arg(long, value_enum, value_name = "MODE", default_value_t = OutputModeCli::Text)]
    mode: OutputModeCli,

    /// Comma-separated tools allowed in this run (non-interactive -p defaults to "Read, Grep, Glob"; include Task to enable subagents)
    #[arg(
        long = "allowed-tools",
        alias = "allowed_tools",
        value_name = "TOOLS",
        action = clap::ArgAction::Append
    )]
    allowed_tools: Vec<String>,

    /// Replace the base system prompt with inline text or a file path
    #[arg(long, value_name = "TEXT_OR_FILE", conflicts_with_all = ["system_prompt_template", "resume", "continue_session"])]
    system_prompt: Option<String>,

    /// Read a Handlebars base system prompt template from a file
    #[arg(long, value_name = "PATH", conflicts_with_all = ["resume", "continue_session"])]
    system_prompt_template: Option<String>,

    /// Append to the system prompt from inline text or a file path
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

    /// Select a permission preset for this run (always-ask, accept-edits, yolo)
    #[arg(long, value_name = "MODE", conflicts_with = "sandbox")]
    permission_mode: Option<pp_config::config::PermissionMode>,

    /// Override tools.sandbox.mode from settings.yml for this run
    #[arg(long, value_enum, value_name = "MODE")]
    sandbox: Option<SandboxModeCli>,

    /// Select the model for this run as [provider/]model[:effort] without saving preferences
    #[arg(long, value_name = "MODEL")]
    model: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
enum OutputModeCli {
    #[default]
    Text,
    Json,
    StreamJson,
}

impl From<OutputModeCli> for cli::OutputMode {
    fn from(value: OutputModeCli) -> Self {
        match value {
            OutputModeCli::Text => Self::Text,
            OutputModeCli::Json => Self::Json,
            OutputModeCli::StreamJson => Self::StreamJson,
        }
    }
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum SandboxModeCli {
    Auto,
    WorkspaceWrite,
    ReadOnly,
    Off,
}

impl From<SandboxModeCli> for pp_config::config::SandboxMode {
    fn from(value: SandboxModeCli) -> Self {
        match value {
            SandboxModeCli::Auto => Self::Auto,
            SandboxModeCli::WorkspaceWrite => Self::WorkspaceWrite,
            SandboxModeCli::ReadOnly => Self::ReadOnly,
            SandboxModeCli::Off => Self::Off,
        }
    }
}

#[derive(Subcommand)]
enum Command {
    /// Remove logs, SQLite request data, and unused session lock files older than the given days (default 90, max 36500)
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
        /// Start the server without opening a browser
        #[arg(long)]
        no_open: bool,
    },
}

#[derive(Default)]
struct ErrorContext {
    session_id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    result_written: bool,
}

fn parse_args() -> Args {
    let raw: Vec<std::ffi::OsString> = std::env::args_os().collect();
    let initial = match Args::try_parse_from(&raw) {
        Ok(args) => args,
        Err(error) => error.exit(),
    };
    if initial.prompt.is_some() {
        let cmd = Args::command().mut_arg("command", |arg| {
            arg.trailing_var_arg(false)
                .allow_hyphen_values(false)
                .num_args(0..=1)
        });
        match cmd.try_get_matches_from(&raw) {
            Ok(matches) => match Args::from_arg_matches(&matches) {
                Ok(args) => return args,
                Err(error) => error.exit(),
            },
            Err(error) => error.exit(),
        }
    }
    initial
}

fn main() -> ExitCode {
    logging::perf::mark("start");
    pp_runtime::raise_file_limit();
    host_defaults::install(host_defaults::capture());
    let args = parse_args();
    let output_mode: cli::OutputMode = args.mode.into();
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
    let mut error_ctx = ErrorContext::default();
    let result = run(args, &mut error_ctx);
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
            if output_mode.is_structured() && !error_ctx.result_written {
                let mut stdout = io::stdout().lock();
                let _ = cli::write_error_result(
                    &mut stdout,
                    &error,
                    error_ctx.session_id.as_deref(),
                    error_ctx.model.as_deref(),
                    error_ctx.effort.as_deref(),
                    elapsed_ms,
                );
            }
            let _ = writeln!(
                io::stderr().lock(),
                "error: {}",
                logging::diagnostic_text(&format!("{error:#}"))
            );
            ExitCode::FAILURE
        }
    }
}

/// Traces 列表的会话标题取自 history.db 的派生索引；`pp stats` 只读摘要。
/// TUI 只在打开会话选择器时增量同步索引，所以首次取标题时补一次同步，
/// 否则最新会话没有摘要可显示；同步失败不阻止服务，已索引的摘要仍可用。
fn session_titles() -> impl FnMut() -> Vec<(String, String)> {
    let mut store: Option<pp_runtime::history::Store> = None;
    let mut unavailable = false;
    move || {
        let Ok(dir) = config::global_dir() else {
            return Vec::new();
        };
        let path = dir.join("history.db");
        if store.is_none() {
            if !std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                return Vec::new();
            }
            let opened = pp_runtime::history::Store::open(&path).map(|mut opened| {
                if opened
                    .sync_sessions(
                        &dir.join("sessions"),
                        &cancellation::CancellationToken::new(),
                    )
                    .is_err()
                {
                    tracing::warn!(event = "stats_session_titles_sync_failed");
                }
                opened
            });
            match opened {
                Ok(opened) => store = Some(opened),
                Err(_) => {
                    if !unavailable {
                        unavailable = true;
                        tracing::warn!(event = "stats_session_titles_unavailable");
                    }
                    return Vec::new();
                }
            }
        }
        let Some(store) = store.as_ref() else {
            return Vec::new();
        };
        match store.search_sessions(None, "") {
            Ok(sessions) => sessions
                .into_iter()
                .map(|session| (session.id, session.summary))
                .collect(),
            Err(_) => {
                if !unavailable {
                    unavailable = true;
                    tracing::warn!(event = "stats_session_titles_unavailable");
                }
                Vec::new()
            }
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

fn run(args: Args, error_ctx: &mut ErrorContext) -> Result<()> {
    if let Some(subcommand) = args.subcommand {
        ensure!(
            args.mode == OutputModeCli::Text,
            "--mode json and --mode stream-json require -p (non-interactive mode)"
        );
        return match subcommand {
            Command::Gc { days } => run_gc(days),
            Command::Stats {
                host,
                port,
                no_open,
            } => stats::server::run(host, port, !no_open, session_titles()),
        };
    }
    let output_mode: cli::OutputMode = args.mode.into();
    let print_mode = args.prompt.is_some();
    if output_mode.is_structured() {
        ensure!(
            print_mode,
            "--mode json and --mode stream-json require -p (non-interactive mode)"
        );
    }

    let (non_interactive_prompt, initial_command) = if let Some(flag_prompt) = args.prompt {
        let positional = match args.command.len() {
            0 => None,
            1 => Some(args.command.into_iter().next().expect("length checked")),
            _ => anyhow::bail!("unexpected extra positional arguments in -p mode"),
        };
        let prompt = match (flag_prompt, positional) {
            (Some(_), Some(_)) => {
                anyhow::bail!("unexpected extra positional prompt when -p already has a value")
            }
            (Some(text), None) | (None, Some(text)) => text,
            (None, None) => anyhow::bail!("prompt is required in -p mode"),
        };
        ensure!(!prompt.trim().is_empty(), "prompt must not be blank");
        (Some(prompt), None)
    } else {
        if args.resume.is_some() && !args.command.is_empty() {
            anyhow::bail!("unexpected positional COMMAND with -r without -p");
        }
        // 位置参数按单空格拼回一条斜杠命令；参数校验先于配置与网络，脚本能立即拿到失败。
        let joined = args.command.join(" ");
        let trimmed = joined.trim();
        let initial = if trimmed.is_empty() {
            None
        } else {
            ensure!(
                trimmed.starts_with('/') || trimmed.starts_with('!'),
                "the positional COMMAND must start with '/' or '!' (use -p TEXT for a one-shot prompt)"
            );
            Some(trimmed.to_string())
        };
        (None, initial)
    };

    let allowed_tools = if !args.allowed_tools.is_empty() {
        Some(pp_tools::AllowedTools::parse_cli(&args.allowed_tools)?)
    } else if print_mode {
        Some(pp_tools::AllowedTools::default_non_interactive())
    } else {
        None
    };
    let subagents_enabled = allowed_tools
        .as_ref()
        .is_none_or(pp_tools::AllowedTools::enables_subagents);

    #[cfg(debug_assertions)]
    let _mock_server = {
        let server = pp_mock_ai::MockServer::start()?;
        pp_mock_ai::install_endpoint(server.endpoint())?;
        server
    };
    let use_tui = !print_mode && io::stdin().is_terminal() && io::stdout().is_terminal();
    let continue_requested = args.continue_session.is_some();
    let resume_id = args
        .resume
        .as_ref()
        .and_then(|id| id.as_deref())
        .or_else(|| args.continue_session.as_ref().and_then(|id| id.as_deref()));
    // `pp -r` without an ID opens the TUI directly on the resume session picker;
    // it still requires an interactive terminal like any other TUI entry.
    let resume_picker = args.resume.is_some() && resume_id.is_none();
    if resume_picker && (print_mode || !use_tui) {
        anyhow::bail!(
            "`-r` without a session ID opens the session picker and requires an interactive terminal; use `pp -r SESSION_ID` to resume directly."
        );
    }
    if let Some(id) = resume_id {
        agent::persistence::validate_id(id)?;
        error_ctx.session_id = Some(id.to_owned());
    }
    // Restore failures are fatal even in interactive mode, unlike unavailable
    // configuration for a fresh `pp`. Load the journal before tools, skills, and
    // the TUI workspace so everything runs from the saved working directory.
    let global_dir = config::global_dir();
    // 进程级统计库句柄：请求、工具、hook 与标题复用同一个连接；
    // 解析失败不在这里致命，由既有配置错误路径呈现。
    let stats_database = pp_stats::database_path().map(pp_stats::Database::new);
    let mut initialized_store = match &global_dir {
        Ok(dir) => {
            match agent_store::initialize_store(dir, &cancellation::CancellationToken::new()) {
                Ok(store) => (Some(store), None),
                Err(error) => (None, Some(error)),
            }
        }
        Err(_) => (None, None),
    };
    let migration_error = initialized_store.1;
    logging::perf::mark("agent_db_ready");
    // 启动期只解析一次 settings.yml：TUI 外观、费用显示、技能发现与模型目录共用同一份快照。
    // 一次性迁移已在上一步完成（旧字段已从文件剥离），所以这里读到的是迁移后的配置；
    // 读取失败仍按既有方式呈现：外观回落默认值，错误进入技能 diagnostics 与配置错误路径。
    let settings = match &global_dir {
        Ok(dir) => {
            config::read_settings(&dir.join("settings.yml")).map(|settings| (dir.clone(), settings))
        }
        Err(error) => Err(anyhow::anyhow!("{error:#}")),
    };
    let (tui_settings, cost_display) = settings
        .as_ref()
        .map(|(_, settings)| (settings.tui, settings.cost.clone()))
        .unwrap_or_default();
    logging::perf::mark("settings_ready");
    let file_commands = if !print_mode {
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
            stats_database
                .as_ref()
                .map_err(|error| anyhow::anyhow!("{error:#}"))?
                .path(),
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
    // 进程级 MCP 单例：根 Agent、子 Agent 与恢复会话共享同一实例，所有模式返回后统一收尾。
    let mcp = std::sync::Arc::new(match global_dir.as_ref() {
        Ok(dir) => pp_mcp::McpManager::new(dir.join("mcp.json"), cwd.clone()),
        Err(_) => pp_mcp::McpManager::empty(),
    });
    // 提示词错误必须在交互兜底之前返回，不能静默禁用或替换用户设定。
    let manager = mcp.clone();
    // 模型目录启动期只装载一次：迁移与 settings 解析都已完成，这里复用同一份快照，
    // 不再重读 settings.yml、也不再打开 agent.db 做第二遍迁移检查。
    // 装载失败不在这里呈现，交给既有启动路径（`Bootstrap::model_state`）重新加载并上报同一错误。
    let models = pp_config::model_state::SharedModels::default();
    if migration_error.is_none()
        && let Ok((dir, catalog_settings)) = &settings
        && let Some(store) = initialized_store.0.take()
        && let Ok(catalog) = config::ModelCatalog::from_migrated_settings_with_store(
            catalog_settings.clone(),
            &dir.join("settings.yml"),
            &dir.join("models.yml"),
            store,
        )
    {
        models.install(pp_config::model_state::ModelState::shared(catalog));
    }
    let result = (move || -> Result<()> {
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
        let skills = match &settings {
            Ok((dir, settings)) => skills::Skills::from_settings(settings, dir),
            Err(error) => {
                let mut skills = skills::Skills::default();
                skills.diagnostics.push(format!("{error:#}"));
                skills
            }
        }
        .shared();
        logging::perf::mark("skills_ready");
        let sandbox = pp_sandbox::SharedSandbox::default();
        let sandbox_override = args.sandbox.map(Into::into);
        let bootstrap = global_dir.as_ref().ok().map(|dir| {
            let permission = pp_permission::Gate::for_process(
                dir,
                args.permission_mode,
                sandbox_override.is_some(),
            );
            if let Ok((_, loaded_settings)) = &settings {
                permission.set_config(loaded_settings.tools.clone());
            }
            if let Some(allowed) = &allowed_tools {
                allowed.apply_permission_grants(&permission);
            }
            agent::Bootstrap {
                dir: dir.clone(),
                cwd: cwd.clone(),
                skills: skills.clone(),
                prompt: prompt_source
                    .clone()
                    .unwrap_or_else(system_prompt::Snapshot::bundled),
                permission: std::sync::Arc::new(permission),
                sandbox_override,
                sandbox: sandbox.clone(),
                mcp: manager.clone(),
                models: models.clone(),
                allowed_tools: allowed_tools.clone(),
                subagents_enabled,
            }
        });
        let mut agent = if let Some(error) = migration_error {
            Err(error)
        } else {
            match restored {
                Some(session) => {
                    let bootstrap = bootstrap.as_ref().ok_or_else(|| {
                        anyhow::anyhow!("Unable to locate global configuration directory")
                    })?;
                    let models = bootstrap
                        .model_state()
                        .context("loading configuration for saved session")?;
                    let mut tools = pp_tools::Tools::with_mcp(
                        cwd.clone(),
                        session.edit_mode(),
                        manager.clone(),
                    )?;
                    tools.skills = skills.clone();
                    tools.permission = bootstrap.permission.as_ref().clone();
                    tools.allowed_tools = allowed_tools.clone();
                    let sbx = sandbox.get_or_init_with_override(
                        models.catalog_snapshot().sandbox_settings(),
                        &cwd,
                        sandbox_override,
                    );
                    tools.set_sandbox(sbx);
                    let mut restored_agent = agent::Agent::restore(models, tools, session)?;
                    restored_agent.set_subagents_enabled(subagents_enabled);
                    Ok(restored_agent)
                }
                None => bootstrap
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Unable to locate global configuration directory")
                    })
                    .and_then(|bootstrap| {
                        bootstrap.build_with_stats(
                            stats_database
                                .as_ref()
                                .map_err(|error| anyhow::anyhow!("{error:#}"))?
                                .clone(),
                        )
                    }),
            }
        };
        logging::perf::mark("bootstrap_ready");
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
        if !use_tui && let Ok(agent) = &agent {
            let status = agent.sandbox_status();
            if let Some(warning) = status.warning() {
                tracing::warn!(event = "sandbox_unavailable", %warning);
                writeln!(io::stderr().lock(), "warning: {warning}")?;
            }
        }
        if let Ok(agent) = &agent {
            if let Some(id) = agent.session_id() {
                error_ctx.session_id = Some(id.to_owned());
            }
            error_ctx.model = Some(agent.current_model().reference.clone());
            error_ctx.effort = Some(agent.think_level().as_str().to_owned());
        }
        // `--model` 只在本次运行生效；显式给出却无法解析时必须致命，不静默回退默认模型。
        // 配置本身不可用时 `agent` 为 Err，此时没有 catalog 可校验，沿用既有交互修复流程。
        if let (Some(query), Ok(agent)) = (args.model.as_deref(), agent.as_mut()) {
            let selection = agent.select_model(query)?;
            error_ctx.model = Some(selection.reference.clone());
            error_ctx.effort = Some(selection.effort.as_str().to_owned());
            if !use_tui && !output_mode.is_structured() {
                writeln!(
                    io::stderr().lock(),
                    "Model: {} (effort {})",
                    selection.reference,
                    selection.effort.as_str()
                )?;
            }
        }
        if use_tui {
            if agent.is_err()
                && let (Some(bootstrap), Ok((_, loaded_settings))) = (&bootstrap, &settings)
            {
                bootstrap.apply_tools_settings(&loaded_settings.tools);
            }
            return tui::run(
                agent,
                &cwd,
                skills,
                file_commands,
                continue_requested,
                resume_picker,
                tui_settings,
                cost_display,
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
        let is_resumed = resumed_id.is_some();
        let mut stderr = io::stderr();
        if !output_mode.is_structured() {
            cli::prelude(&skills, &file_commands, resumed_id, &mut stderr)?;
        }
        if let Some(prompt) = non_interactive_prompt {
            let mut stdout = io::stdout().lock();
            let mut agent = agent?;
            // -p 不启动空闲轮询，也不等待迟到诊断；任务结束即显式回收服务器。
            let task = cli::run_cli_task_with_mode(
                &mut agent,
                Some(&prompt),
                output_mode,
                is_resumed,
                &mut stdout,
                &mut stderr,
                &mut error_ctx.result_written,
            );
            let hint = if output_mode.is_structured() {
                Ok(())
            } else {
                cli::write_resume_hint(&mut stderr, agent.session_id()).map_err(anyhow::Error::from)
            };
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
    })();
    stop_mcp(&mcp, result)
}

/// 所有模式返回后停止进程级 MCP 服务；`CleanupError` 保持类型并合并进返回错误，
/// 不能只依赖 `McpManager::Drop` 兜底。
fn stop_mcp(mcp: &pp_mcp::McpManager, result: Result<()>) -> Result<()> {
    let stop = mcp.shutdown();
    match (result, stop) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) | (Err(error), Ok(())) => Err(error),
        (Err(error), Err(stop)) => {
            if stop.is::<cancellation::CleanupError>() {
                Err(stop.context(format!("{error:#}")))
            } else {
                Err(error.context(format!("stopping MCP servers: {stop:#}")))
            }
        }
    }
}
