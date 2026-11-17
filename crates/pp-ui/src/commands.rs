#[cfg(debug_assertions)]
use pp_mock_ai::MockTest;
mod templates;
pub use templates::{FileCommand, FileCommands};

use anyhow::{Context, Result, bail, ensure};
use pp_core::model::ThinkLevel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandCategory {
    Session,
    Model,
    Workflow,
    Config,
    #[cfg(debug_assertions)]
    Debug,
}

impl CommandCategory {
    pub(crate) const ALL: &[Self] = &[
        Self::Session,
        Self::Model,
        Self::Workflow,
        Self::Config,
        #[cfg(debug_assertions)]
        Self::Debug,
    ];

    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Session => "Session & History",
            Self::Model => "Models & Thinking",
            Self::Workflow => "Workflow & Execution",
            Self::Config => "Settings & Extensions",
            #[cfg(debug_assertions)]
            Self::Debug => "Debug Scenarios",
        }
    }
}

pub(crate) struct CommandSpec {
    pub(crate) name: &'static str,
    pub(crate) display_label: Option<&'static str>,
    pub(crate) aliases: &'static [&'static str],
    pub(crate) category: CommandCategory,
    pub(crate) usage: &'static str,
    pub(crate) description: &'static str,
}

impl CommandSpec {
    pub(crate) const fn menu_label(&self) -> &'static str {
        match self.display_label {
            Some(label) => label,
            None => self.name,
        }
    }
}

pub(crate) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/mcp",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Config,
        usage: "/mcp [list|enable <name>|disable <name>|reconnect <name>|reload|help]",
        description: "Manage MCP servers and connection state",
    },
    CommandSpec {
        name: "/setting",
        display_label: Some("/setting (config)"),
        aliases: &["/settings", "/config"],
        category: CommandCategory::Config,
        usage: "/setting [general|roles|themes|skills]",
        description: "Open Settings TUI (/config, /settings)",
    },
    CommandSpec {
        name: "/sandbox",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Config,
        usage: "/sandbox [auto|workspace-write|read-only|off]",
        description: "Show sandbox status or override mode for this session",
    },
    CommandSpec {
        name: "/roles",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Model,
        usage: "/roles",
        description: "Configure model and effort for each model role (TUI)",
    },
    CommandSpec {
        name: "/agents",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Workflow,
        usage: "/agents",
        description: "View, follow up, and control subagents",
    },
    CommandSpec {
        name: "/help",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Config,
        usage: "/help",
        description: "Show available commands",
    },
    CommandSpec {
        name: "/tips",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Config,
        usage: "/tips",
        description: "Show startup tips without recording session history",
    },
    CommandSpec {
        name: "/stats",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Config,
        usage: "/stats [--host IP] [--port PORT] [--no-open]",
        description: "Open the local statistics page (same as pp stats)",
    },
    CommandSpec {
        name: "/login",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Config,
        usage: "/login",
        description: "Log in to a subscription provider (TUI)",
    },
    CommandSpec {
        name: "/logout",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Config,
        usage: "/logout",
        description: "Remove saved subscription authorization (TUI)",
    },
    CommandSpec {
        name: "/model",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Model,
        usage: "/model [provider/model]",
        description: "Select a model at the next safe boundary; /models is an alias",
    },
    CommandSpec {
        name: "/new",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Session,
        usage: "/new",
        description: "Reset the session, keeping the current model",
    },
    CommandSpec {
        name: "/init",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Session,
        usage: "/init",
        description: "Create or update project AGENTS.md using the current model",
    },
    CommandSpec {
        name: "/compact",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Session,
        usage: "/compact",
        description: "Summarize older context, preserving recent exchanges and visible history",
    },
    CommandSpec {
        name: "/context",
        display_label: None,
        aliases: &["/ctx"],
        category: CommandCategory::Session,
        usage: "/context",
        description: "Inspect session context usage breakdown and largest items (TUI)",
    },
    CommandSpec {
        name: "/resume",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Session,
        usage: "/resume",
        description: "List saved sessions and resume a conversation",
    },
    CommandSpec {
        name: "/effort",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Model,
        usage: "/effort [none|low|medium|high|xhigh|max]",
        description: "View or set the thinking effort level, keeping session history",
    },
    CommandSpec {
        name: "/plan",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Workflow,
        usage: "/plan [view|prompt]",
        description: "Toggle Plan mode, view the current plan, or send a planning task (TUI)",
    },
    CommandSpec {
        name: "/tree",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Session,
        usage: "/tree",
        description: "Browse current history and rewind to an earlier point (TUI)",
    },
    CommandSpec {
        name: "/pipes",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Workflow,
        usage: "/pipes [run-id]",
        description: "Browse temporary pipeline stage streams for a bash run (TUI)",
    },
    CommandSpec {
        name: "/plan-list",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Workflow,
        usage: "/plan-list",
        description: "Manage saved plans: preview, continue, execute, rename, delete (TUI)",
    },
    CommandSpec {
        name: "/themes",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Config,
        usage: "/themes [nerd|ascii] [THEME]",
        description: "Configure TUI icon style and color theme",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-task",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-task",
        description: "Debug build: dispatch a fixed FAST/SLOW/REVIEW subagent batch",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-tools",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-tools",
        description: "Debug build: run every local tool display scenario",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-read",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-read",
        description: "Debug build: local read tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-write",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-write",
        description: "Debug build: typewriter write on /tmp, then bash cleanup",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-edit",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-edit",
        description: "Debug build: local edit tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-rg",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-rg",
        description: "Debug build: local rg tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-glob",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-glob",
        description: "Debug build: local glob tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-bash",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-bash",
        description: "Debug build: local bash tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-bash-task",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-bash-task",
        description: "Debug build: local background bash scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-eval-py",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-eval-py",
        description: "Debug build: local Python eval streaming, state & bridge scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-wait",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-wait",
        description: "Debug build: local wait tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-todo",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-todo",
        description: "Debug build: local todo tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-ask",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-ask",
        description: "Debug build: local ask panel scenario",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-fail",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-fail",
        description: "Debug build: two failing attempts then a retry recovery",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-ui",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Debug,
        usage: "/test-ui",
        description: "Debug build: slow mock thinking and Markdown stream",
    },
    CommandSpec {
        name: "/skills",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Config,
        usage: "/skills [enable NAME|disable NAME|sources [enable|disable claude|codex]]",
        description: "Manage skill sources and per-item toggles",
    },
    CommandSpec {
        name: "/quit",
        display_label: None,
        aliases: &[],
        category: CommandCategory::Session,
        usage: "/quit",
        description: "Exit pp",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanCommand<'a> {
    Toggle,
    View,
    Prompt(&'a str),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SkillCommand<'a> {
    List,
    Toggle(&'a str, bool),
    Sources,
    Source(pp_config::skills::SkillSource, bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ThemeCommand {
    pub(crate) theme: Option<pp_config::config::Theme>,
    pub(crate) icons: Option<pp_config::config::IconMode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingTab {
    General,
    Roles,
    Themes,
    Skills,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command<'a> {
    Help,
    Tips,
    Stats(&'a str),
    New,
    Compact,
    Context,
    Resume,
    Tree,
    Pipes(Option<&'a str>),
    Setting(SettingTab),
    Roles,
    Agents,
    PlanList,
    Themes(ThemeCommand),
    #[cfg(debug_assertions)]
    MockTest(MockTest),
    Login,
    Logout,
    Plan(PlanCommand<'a>),
    Quit,
    Model(Option<&'a str>),
    Think(Option<ThinkLevel>),
    Sandbox(Option<pp_config::config::SandboxMode>),
    Skills(SkillCommand<'a>),
    Mcp(pp_mcp::Action),
    Shell(&'a str),
    Prompt(String),
    /// 未注册的斜杠命令；消费者必须静默丢弃，不发送模型请求。
    Unknown,
}

pub(crate) fn parse<'a>(input: &'a str, files: &FileCommands) -> Result<Option<Command<'a>>> {
    if let Some(command) = input.trim_start().strip_prefix('!') {
        ensure!(!command.trim().is_empty(), "usage: !command");
        return Ok(Some(Command::Shell(command)));
    }
    let input = input.trim();
    if !input.starts_with('/') {
        return Ok(None);
    }
    let (name, argument) = input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(name, argument)| (name, argument.trim()));
    if name != "/plan" && input.contains(['\n', '\r']) {
        return Ok(None);
    }
    #[cfg(debug_assertions)]
    if let Some(test) = MockTest::ALL.into_iter().find(|test| test.name() == name) {
        ensure!(argument.is_empty(), "usage: {name}");
        return Ok(Some(Command::MockTest(test)));
    }
    let command = match name {
        "/help" => Command::Help,
        "/tips" => Command::Tips,
        "/stats" => return Ok(Some(Command::Stats(argument))),
        "/new" => Command::New,
        "/init" => Command::Prompt(pp_tools::project_instructions::INIT_PROMPT.to_owned()),
        "/compact" => Command::Compact,
        "/context" | "/ctx" => Command::Context,
        "/resume" => Command::Resume,
        "/tree" => Command::Tree,
        "/pipes" => {
            ensure!(
                argument.is_empty()
                    || argument.len() == 32
                        && argument
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "usage: /pipes [run-id]"
            );
            return Ok(Some(Command::Pipes(
                (!argument.is_empty()).then_some(argument),
            )));
        }
        "/setting" | "/settings" | "/config" => {
            let tab = match argument {
                "" | "general" => SettingTab::General,
                "roles" => SettingTab::Roles,
                "themes" => SettingTab::Themes,
                "skills" => SettingTab::Skills,
                _ => bail!("usage: /setting [general|roles|themes|skills]"),
            };
            return Ok(Some(Command::Setting(tab)));
        }
        "/roles" => Command::Roles,
        "/agents" => Command::Agents,
        "/login" | "/logout" => {
            ensure!(argument.is_empty(), "usage: {name}");
            if name == "/login" {
                Command::Login
            } else {
                Command::Logout
            }
        }
        "/plan-list" => {
            ensure!(argument.is_empty(), "usage: /plan-list");
            Command::PlanList
        }
        "/plan" => {
            return Ok(Some(Command::Plan(match argument {
                "" => PlanCommand::Toggle,
                "view" => PlanCommand::View,
                // 旧子命令已删除：只报本地错误，不进入 Plan、不发送模型请求。
                "new" | "off" => {
                    bail!("Use /plan to toggle; manage plans with /plan-list.")
                }
                prompt => PlanCommand::Prompt(prompt),
            })));
        }
        "/quit" => Command::Quit,
        "/model" | "/models" => {
            return Ok(Some(Command::Model(
                (!argument.is_empty()).then_some(argument),
            )));
        }
        "/mcp" => {
            use pp_mcp::Action;
            let words: Vec<_> = argument.split_whitespace().collect();
            let action = match words.as_slice() {
                [] | ["help"] => Action::Help,
                ["list"] => Action::List,
                ["reload"] => Action::Reload,
                ["enable", name] => Action::Enable((*name).into()),
                ["disable", name] => Action::Disable((*name).into()),
                ["reconnect", name] => Action::Reconnect((*name).into()),
                _ => bail!(
                    "usage: /mcp [list|enable <name>|disable <name>|reconnect <name>|reload|help]"
                ),
            };
            return Ok(Some(Command::Mcp(action)));
        }
        "/skills" => {
            if argument.is_empty() {
                return Ok(Some(Command::Skills(SkillCommand::List)));
            }
            if argument == "sources" {
                return Ok(Some(Command::Skills(SkillCommand::Sources)));
            }
            if let Some(arguments) = argument
                .strip_prefix("sources")
                .filter(|rest| rest.starts_with(char::is_whitespace))
            {
                let words = arguments.split_whitespace().collect::<Vec<_>>();
                ensure!(
                    words.len() == 2 && matches!(words[0], "enable" | "disable"),
                    "usage: /skills sources [enable|disable claude|codex]"
                );
                let source = pp_config::skills::SkillSource::ALL
                    .into_iter()
                    .find(|source| source.name() == words[1])
                    .context("unknown skill source; use claude or codex")?;
                return Ok(Some(Command::Skills(SkillCommand::Source(
                    source,
                    words[0] == "enable",
                ))));
            }
            let (action, name) = argument
                .split_once(char::is_whitespace)
                .context("usage: /skills [enable NAME|disable NAME]")?;
            ensure!(!name.trim().is_empty(), "skill name cannot be empty");
            ensure!(
                matches!(action, "enable" | "disable"),
                "usage: /skills [enable NAME|disable NAME]"
            );
            return Ok(Some(Command::Skills(SkillCommand::Toggle(
                name.trim(),
                action == "enable",
            ))));
        }
        "/effort" | "/think" => {
            return Ok(Some(Command::Think(
                (!argument.is_empty())
                    .then(|| argument.parse())
                    .transpose()?,
            )));
        }
        "/sandbox" => {
            return Ok(Some(Command::Sandbox(
                (!argument.is_empty())
                    .then(|| argument.parse())
                    .transpose()?,
            )));
        }
        "/themes" | "/theme" => {
            let mut icons = None;
            let mut theme = None;
            for word in argument.split_whitespace() {
                if let Some(parsed) = pp_config::config::IconMode::parse(word) {
                    ensure!(
                        icons.replace(parsed).is_none(),
                        "usage: /themes [nerd|ascii] [THEME]"
                    );
                } else if let Some(parsed) = pp_config::config::Theme::parse(word) {
                    ensure!(
                        theme.replace(parsed).is_none(),
                        "usage: /themes [nerd|ascii] [THEME]"
                    );
                } else {
                    bail!(
                        "usage: /themes [nerd|ascii] [THEME] (icons: nerd, ascii; themes: default, tokyonight, catppuccin, dracula, nord, gruvbox, solarized-light)"
                    );
                }
            }
            return Ok(Some(Command::Themes(ThemeCommand { theme, icons })));
        }
        _ => {
            let file: Option<&FileCommand> = files.entries.iter().find(|file| file.name == name);
            let Some(file) = file else {
                // 未知命令静默忽略：不产生本地错误，也不作为普通提示词发送。
                return Ok(Some(Command::Unknown));
            };
            let prompt = file.expand(argument);
            ensure!(
                !prompt.trim().is_empty(),
                "command expands to an empty prompt"
            );
            return Ok(Some(Command::Prompt(prompt)));
        }
    };
    let spec = COMMANDS
        .iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name))
        .expect("registered command");
    ensure!(argument.is_empty(), "usage: {}", spec.usage);
    Ok(Some(command))
}

pub(crate) const SHELL_COMMAND_USAGE: &str = "!command";
pub(crate) const SHELL_COMMAND_DESCRIPTION: &str =
    "Run a local shell command without sending it to the model";
pub(crate) const HELP_USAGE_COLUMN: usize = 28;

fn spec_order(name: &str) -> usize {
    match name {
        "/new" => 0,
        "/resume" => 1,
        "/tree" => 2,
        "/compact" => 3,
        "/context" => 4,
        "/init" => 5,
        "/quit" => 6,
        "/model" => 0,
        "/effort" => 1,
        "/roles" => 2,
        "/plan" => 0,
        "/plan-list" => 1,
        "/agents" => 2,
        "/pipes" => 3,
        "/setting" => 0,
        "/themes" => 1,
        "/skills" => 2,
        "/mcp" => 3,
        "/stats" => 4,
        "/login" => 5,
        "/logout" => 6,
        "/tips" => 7,
        "/help" => 8,
        _ => usize::MAX,
    }
}

pub(crate) fn category_specs(category: CommandCategory) -> Vec<&'static CommandSpec> {
    let mut specs: Vec<&'static CommandSpec> = COMMANDS
        .iter()
        .filter(|spec| spec.category == category)
        .collect();
    specs.sort_by(|left, right| {
        spec_order(left.name)
            .cmp(&spec_order(right.name))
            .then_with(|| left.name.cmp(right.name))
    });
    specs
}

fn format_help_row(usage: &str, description: &str) -> String {
    if usage.chars().count() < HELP_USAGE_COLUMN {
        format!("  {usage:<HELP_USAGE_COLUMN$} {description}")
    } else {
        format!("  {usage} — {description}")
    }
}

/// 命令清单：`COMMANDS` 已按构建裁剪，非 dev/test 不含 `/test-*` 条目。
pub(crate) fn help(files: &FileCommands) -> String {
    let mut lines = vec!["Help:".to_owned()];
    for &category in CommandCategory::ALL {
        let specs = category_specs(category);
        if specs.is_empty() {
            continue;
        }
        lines.push(String::new());
        lines.push(category.title().to_owned());
        for spec in specs {
            lines.push(format_help_row(spec.usage, spec.description));
        }
        if category == CommandCategory::Workflow {
            lines.push(format_help_row(
                SHELL_COMMAND_USAGE,
                SHELL_COMMAND_DESCRIPTION,
            ));
        }
    }
    if !files.entries.is_empty() {
        lines.push(String::new());
        lines.push("Custom Commands".to_owned());
        for file in &files.entries {
            let usage = format!("{} [arguments]", file.name);
            lines.push(format_help_row(&usage, &file.description));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests;
