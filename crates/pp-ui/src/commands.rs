#[cfg(debug_assertions)]
use pp_runtime::mock_ai::MockTest;
mod templates;
pub use templates::{FileCommand, FileCommands};

use anyhow::{Context, Result, bail, ensure};
use pp_core::model::ThinkLevel;

pub(crate) struct CommandSpec {
    pub(crate) name: &'static str,
    pub(crate) display_label: Option<&'static str>,
    pub(crate) aliases: &'static [&'static str],
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
        usage: "/mcp [list|enable <name>|disable <name>|reconnect <name>|reload|help]",
        description: "Manage MCP servers and connection state",
    },
    CommandSpec {
        name: "/setting",
        display_label: Some("/setting (config)"),
        aliases: &["/settings", "/config"],
        usage: "/setting [general|roles|themes|skills]",
        description: "Open Settings TUI (/config, /settings)",
    },
    CommandSpec {
        name: "/roles",
        display_label: None,
        aliases: &[],
        usage: "/roles",
        description: "Configure model and effort for each model role (TUI)",
    },
    CommandSpec {
        name: "/agents",
        display_label: None,
        aliases: &[],
        usage: "/agents",
        description: "View, follow up, and control subagents",
    },
    CommandSpec {
        name: "/help",
        display_label: None,
        aliases: &[],
        usage: "/help",
        description: "Show available commands",
    },
    CommandSpec {
        name: "/tips",
        display_label: None,
        aliases: &[],
        usage: "/tips",
        description: "Show startup tips without recording session history",
    },
    CommandSpec {
        name: "/stats",
        display_label: None,
        aliases: &[],
        usage: "/stats [--host IP] [--port PORT] [--no-open]",
        description: "Open the local statistics page (same as pp stats)",
    },
    CommandSpec {
        name: "/login",
        display_label: None,
        aliases: &[],
        usage: "/login",
        description: "Log in to a subscription provider (TUI)",
    },
    CommandSpec {
        name: "/logout",
        display_label: None,
        aliases: &[],
        usage: "/logout",
        description: "Remove saved subscription authorization (TUI)",
    },
    CommandSpec {
        name: "/model",
        display_label: None,
        aliases: &[],
        usage: "/model [provider/model]",
        description: "Select a model at the next safe boundary; /models is an alias",
    },
    CommandSpec {
        name: "/new",
        display_label: None,
        aliases: &[],
        usage: "/new",
        description: "Reset the session, keeping the current model",
    },
    CommandSpec {
        name: "/init",
        display_label: None,
        aliases: &[],
        usage: "/init",
        description: "Create or update project AGENTS.md using the current model",
    },
    CommandSpec {
        name: "/compact",
        display_label: None,
        aliases: &[],
        usage: "/compact",
        description: "Summarize older context, preserving recent exchanges and visible history",
    },
    CommandSpec {
        name: "/resume",
        display_label: None,
        aliases: &[],
        usage: "/resume",
        description: "List saved sessions and resume a conversation",
    },
    CommandSpec {
        name: "/effort",
        display_label: None,
        aliases: &[],
        usage: "/effort [none|low|medium|high|xhigh|max]",
        description: "View or set the thinking effort level, keeping session history",
    },
    CommandSpec {
        name: "/plan",
        display_label: None,
        aliases: &[],
        usage: "/plan [view|prompt]",
        description: "Toggle Plan mode, view the current plan, or send a planning task (TUI)",
    },
    CommandSpec {
        name: "/tree",
        display_label: None,
        aliases: &[],
        usage: "/tree",
        description: "Browse current history and rewind to an earlier point (TUI)",
    },
    CommandSpec {
        name: "/pipes",
        display_label: None,
        aliases: &[],
        usage: "/pipes [run-id]",
        description: "Browse temporary pipeline stage streams for a bash run (TUI)",
    },
    CommandSpec {
        name: "/plan-list",
        display_label: None,
        aliases: &[],
        usage: "/plan-list",
        description: "Manage saved plans: preview, continue, execute, rename, delete (TUI)",
    },
    CommandSpec {
        name: "/themes",
        display_label: None,
        aliases: &[],
        usage: "/themes [nerd|ascii] [THEME]",
        description: "Configure TUI icon style and color theme",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-task",
        display_label: None,
        aliases: &[],
        usage: "/test-task",
        description: "Debug build: dispatch a fixed FAST/SLOW/REVIEW subagent batch",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-tools",
        display_label: None,
        aliases: &[],
        usage: "/test-tools",
        description: "Debug build: run every local tool display scenario",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-read",
        display_label: None,
        aliases: &[],
        usage: "/test-read",
        description: "Debug build: local read tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-write",
        display_label: None,
        aliases: &[],
        usage: "/test-write",
        description: "Debug build: typewriter write on /tmp, then bash cleanup",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-edit",
        display_label: None,
        aliases: &[],
        usage: "/test-edit",
        description: "Debug build: local edit tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-rg",
        display_label: None,
        aliases: &[],
        usage: "/test-rg",
        description: "Debug build: local rg tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-glob",
        display_label: None,
        aliases: &[],
        usage: "/test-glob",
        description: "Debug build: local glob tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-bash",
        display_label: None,
        aliases: &[],
        usage: "/test-bash",
        description: "Debug build: local bash tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-bash-task",
        display_label: None,
        aliases: &[],
        usage: "/test-bash-task",
        description: "Debug build: local background bash scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-wait",
        display_label: None,
        aliases: &[],
        usage: "/test-wait",
        description: "Debug build: local wait tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-todo",
        display_label: None,
        aliases: &[],
        usage: "/test-todo",
        description: "Debug build: local todo tool scenarios",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-ask",
        display_label: None,
        aliases: &[],
        usage: "/test-ask",
        description: "Debug build: local ask panel scenario",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-fail",
        display_label: None,
        aliases: &[],
        usage: "/test-fail",
        description: "Debug build: two failing attempts then a retry recovery",
    },
    #[cfg(debug_assertions)]
    CommandSpec {
        name: "/test-ui",
        display_label: None,
        aliases: &[],
        usage: "/test-ui",
        description: "Debug build: slow mock thinking and Markdown stream",
    },
    CommandSpec {
        name: "/skills",
        display_label: None,
        aliases: &[],
        usage: "/skills [enable NAME|disable NAME|sources [enable|disable claude|codex]]",
        description: "Manage skill sources and per-item toggles",
    },
    CommandSpec {
        name: "/quit",
        display_label: None,
        aliases: &[],
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
    Skills(SkillCommand<'a>),
    Mcp(pp_runtime::mcp::Action),
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
        "/init" => Command::Prompt(pp_runtime::project_instructions::INIT_PROMPT.to_owned()),
        "/compact" => Command::Compact,
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
            use pp_runtime::mcp::Action;
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
        .find(|spec| spec.name == name)
        .expect("registered command");
    ensure!(argument.is_empty(), "usage: {}", spec.usage);
    Ok(Some(command))
}

/// 命令清单：`COMMANDS` 已按构建裁剪，非 dev/test 不含 `/test-*` 条目。
pub(crate) fn help(files: &FileCommands) -> String {
    COMMANDS
        .iter()
        .map(|spec| format!("{} — {}", spec.usage, spec.description))
        .chain(
            files
                .entries
                .iter()
                .map(|file| format!("{} [arguments] — {}", file.name, file.description)),
        )
        .chain(std::iter::once(
            "!command — Run a local shell command without sending it to the model".to_owned(),
        ))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests;
