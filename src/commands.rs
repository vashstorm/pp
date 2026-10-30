mod templates;
pub(crate) use templates::{FileCommand, FileCommands};
pub(crate) mod task_test;

use crate::config::ThinkLevel;
use anyhow::{Context, Result, bail, ensure};

pub(crate) struct CommandSpec {
    pub(crate) name: &'static str,
    pub(crate) usage: &'static str,
    pub(crate) description: &'static str,
}

pub(crate) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/mcp",
        usage: "/mcp [list|enable <name>|disable <name>|reconnect <name>|reload|help]",
        description: "Manage MCP servers and connection state",
    },
    CommandSpec {
        name: "/Roles",
        usage: "/Roles",
        description: "Configure model and effort for each model role (TUI)",
    },
    CommandSpec {
        name: "/subagents",
        usage: "/subagents",
        description: "View, follow up, and control subagents",
    },
    CommandSpec {
        name: "/agents",
        usage: "/agents",
        description: "View available subagent roles and sources",
    },
    CommandSpec {
        name: "/help",
        usage: "/help",
        description: "Show available commands",
    },
    CommandSpec {
        name: "/tips",
        usage: "/tips",
        description: "Show startup tips without recording session history",
    },
    CommandSpec {
        name: "/login",
        usage: "/login",
        description: "Log in to a subscription provider (TUI)",
    },
    CommandSpec {
        name: "/logout",
        usage: "/logout",
        description: "Remove saved subscription authorization (TUI)",
    },
    CommandSpec {
        name: "/model",
        usage: "/model [provider/model]",
        description: "Select a model at the next safe boundary; /models is an alias",
    },
    CommandSpec {
        name: "/new",
        usage: "/new",
        description: "Reset the session, keeping the current model",
    },
    CommandSpec {
        name: "/init",
        usage: "/init",
        description: "Create or update project AGENTS.md using the current model",
    },
    CommandSpec {
        name: "/compact",
        usage: "/compact",
        description: "Summarize older context, preserving recent exchanges and visible history",
    },
    CommandSpec {
        name: "/resume",
        usage: "/resume",
        description: "List saved sessions and resume a conversation",
    },
    CommandSpec {
        name: "/effort",
        usage: "/effort [none|low|medium|high|xhigh|max]",
        description: "View or set the thinking effort level, keeping session history",
    },
    CommandSpec {
        name: "/plan",
        usage: "/plan [view|prompt]",
        description: "Toggle Plan mode, view the current plan, or send a planning task (TUI)",
    },
    CommandSpec {
        name: "/tree",
        usage: "/tree",
        description: "Browse current history and rewind to an earlier point (TUI)",
    },
    CommandSpec {
        name: "/changes",
        usage: "/changes",
        description: "Review latest-task edit/write changes and undo files (TUI)",
    },
    CommandSpec {
        name: "/plan-list",
        usage: "/plan-list",
        description: "Manage saved plans: preview, continue, execute, rename, delete (TUI)",
    },
    CommandSpec {
        name: "/task-test",
        usage: "/task-test",
        description: "Debug build: dispatch a fixed FAST/SLOW/REVIEW subagent batch",
    },
    CommandSpec {
        name: "/skills",
        usage: "/skills [enable NAME|disable NAME|sources [enable|disable claude|codex]]",
        description: "Manage skill sources and per-item toggles",
    },
    CommandSpec {
        name: "/quit",
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
    Source(crate::skills::SkillSource, bool),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command<'a> {
    Help,
    Tips,
    New,
    Compact,
    Resume,
    Tree,
    Changes,
    Agents,
    Roles,
    Subagents,
    PlanList,
    TaskTest,
    Login,
    Logout,
    Plan(PlanCommand<'a>),
    Quit,
    Model(Option<&'a str>),
    Think(Option<ThinkLevel>),
    Skills(SkillCommand<'a>),
    Mcp(crate::mcp::Action),
    Shell(&'a str),
    Prompt(String),
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
    let command = match name {
        "/help" => Command::Help,
        "/tips" => Command::Tips,
        "/new" => Command::New,
        "/init" => Command::Prompt(crate::project_instructions::INIT_PROMPT.to_owned()),
        "/compact" => Command::Compact,
        "/resume" => Command::Resume,
        "/tree" => Command::Tree,
        "/changes" => Command::Changes,
        "/agents" => Command::Agents,
        "/Roles" | "/roles" => {
            ensure!(argument.is_empty(), "usage: /Roles");
            return Ok(Some(Command::Roles));
        }
        "/subagents" => Command::Subagents,
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
        "/task-test" => {
            ensure!(
                cfg!(debug_assertions),
                "/task-test requires a debug build of pp"
            );
            Command::TaskTest
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
            use crate::mcp::Action;
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
                let source = crate::skills::SkillSource::ALL
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
        _ => {
            let file: &FileCommand = files
                .entries
                .iter()
                .find(|file| file.name == name)
                .context("unknown command; use /help")?;
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

/// release 构建隐藏 debug 专用命令；`/help` 与命令菜单共用同一判据。
pub(crate) fn visible(name: &str) -> bool {
    cfg!(debug_assertions) || name != "/task-test"
}

pub(crate) fn help(files: &FileCommands) -> String {
    COMMANDS
        .iter()
        .filter(|spec| visible(spec.name))
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
mod tests {
    use super::*;

    #[test]
    fn slash_plan_accepts_prompt_and_preserves_subcommands() {
        for (input, expected) in [
            (" /plan \t", PlanCommand::Toggle),
            ("/plan view", PlanCommand::View),
            ("/plan  调研登录流程  ", PlanCommand::Prompt("调研登录流程")),
            ("/plan\tview logs", PlanCommand::Prompt("view logs")),
            (
                "/plan first line\nsecond line",
                PlanCommand::Prompt("first line\nsecond line"),
            ),
        ] {
            assert_eq!(
                parse(input, &FileCommands::default()).unwrap(),
                Some(Command::Plan(expected))
            );
        }
    }

    #[test]
    fn slash_plan_old_subcommands_are_rejected_locally() {
        for input in ["/plan new", "/plan off", " /plan new ", "/plan\toff"] {
            assert_eq!(
                parse(input, &FileCommands::default())
                    .unwrap_err()
                    .to_string(),
                "Use /plan to toggle; manage plans with /plan-list."
            );
        }
        // 仅精确子命令被拒绝；其余正文仍作为规划任务发送。
        for (input, expected) in [
            ("/plan new task", PlanCommand::Prompt("new task")),
            ("/plan off switch", PlanCommand::Prompt("off switch")),
        ] {
            assert_eq!(
                parse(input, &FileCommands::default()).unwrap(),
                Some(Command::Plan(expected))
            );
        }
    }

    #[test]
    fn bang_shell_parse_preserves_command() {
        let files = FileCommands::default();
        for (input, command) in [
            ("!printf x", "printf x"),
            (" \t! printf 'x' | cat  \n", " printf 'x' | cat  \n"),
            ("!printf one\nprintf two\n", "printf one\nprintf two\n"),
            ("!!echo", "!echo"),
        ] {
            assert_eq!(parse(input, &files).unwrap(), Some(Command::Shell(command)));
        }
        for input in ["!", "  ! \n\t"] {
            assert_eq!(
                parse(input, &files).unwrap_err().to_string(),
                "usage: !command"
            );
        }
        assert_eq!(parse("text !echo", &files).unwrap(), None);
        assert_eq!(parse("/help\nextra", &files).unwrap(), None);
    }

    #[test]
    fn slash_skills_sources_and_individual_selection() {
        use crate::skills::SkillSource;
        for (input, expected) in [
            ("/skills", SkillCommand::List),
            ("/skills sources", SkillCommand::Sources),
            (
                "/skills sources enable claude",
                SkillCommand::Source(SkillSource::Claude, true),
            ),
            (
                "/skills sources\tdisable codex",
                SkillCommand::Source(SkillSource::Codex, false),
            ),
            ("/skills disable rust", SkillCommand::Toggle("rust", false)),
            (
                "/skills enable sources",
                SkillCommand::Toggle("sources", true),
            ),
        ] {
            assert_eq!(
                parse(input, &FileCommands::default()).unwrap(),
                Some(Command::Skills(expected))
            );
        }
        for input in [
            "/skills sources enable",
            "/skills sources toggle claude",
            "/skills sources enable unknown",
            "/skills sources enable claude extra",
        ] {
            assert!(parse(input, &FileCommands::default()).is_err());
        }
    }

    #[test]
    fn slash_valid_commands_and_model_reference_boundaries() {
        for (input, expected) in [
            (" /help \n", Command::Help),
            (" /tips ", Command::Tips),
            ("\t/new ", Command::New),
            (" /compact ", Command::Compact),
            (
                " /init ",
                Command::Prompt(crate::project_instructions::INIT_PROMPT.to_owned()),
            ),
            ("/resume", Command::Resume),
            ("/tree", Command::Tree),
            ("/plan-list", Command::PlanList),
            (" /plan-list ", Command::PlanList),
            ("/task-test", Command::TaskTest),
            (" /task-test ", Command::TaskTest),
            (" /resume ", Command::Resume),
            ("/quit", Command::Quit),
            ("/model", Command::Model(None)),
            ("/models", Command::Model(None)),
            ("/models fixture/one", Command::Model(Some("fixture/one"))),
            (
                " /model  gateway/team/model with spaces  ",
                Command::Model(Some("gateway/team/model with spaces")),
            ),
        ] {
            assert_eq!(
                parse(input, &FileCommands::default()).unwrap(),
                Some(expected)
            );
        }
    }

    #[test]
    fn slash_unknown_and_extra_arguments_are_local_errors() {
        for input in ["/", "/bogus", "/Help", "/modelx"] {
            assert_eq!(
                parse(input, &FileCommands::default())
                    .unwrap_err()
                    .to_string(),
                "unknown command; use /help"
            );
        }
        for command in [
            "/help",
            "/tips",
            "/new",
            "/init",
            "/compact",
            "/resume",
            "/tree",
            "/quit",
            "/plan-list",
            "/task-test",
        ] {
            assert_eq!(
                parse(&format!("{command} extra"), &FileCommands::default())
                    .unwrap_err()
                    .to_string(),
                format!("usage: {command}")
            );
        }
    }

    #[test]
    fn slash_help_lists_debug_only_commands_in_debug_builds() {
        let text = help(&FileCommands::default());
        let listed = text
            .contains("/task-test — Debug build: dispatch a fixed FAST/SLOW/REVIEW subagent batch");
        assert_eq!(listed, cfg!(debug_assertions), "{text}");
        assert_eq!(visible("/task-test"), cfg!(debug_assertions));
        assert!(visible("/help") && visible("/plan-list"));
    }

    #[test]
    fn slash_multiline_and_regular_tasks_are_not_commands() {
        for input in [
            "",
            "  ",
            "task",
            "/model\nother",
            "/resume\nsession-id",
            "/resume\r\nextra",
            "/help\r\ncode",
            "text /quit",
        ] {
            assert_eq!(parse(input, &FileCommands::default()).unwrap(), None);
        }
    }

    #[test]
    fn think_commands_validate_levels_locally() {
        assert_eq!(
            parse("/think", &FileCommands::default()).unwrap(),
            Some(Command::Think(None))
        );
        assert_eq!(
            parse("/effort ", &FileCommands::default()).unwrap(),
            Some(Command::Think(None))
        );
        for name in ["/effort", "/think"] {
            for level in ["none", "default", "low", "medium", "high", "xhigh", "max"] {
                assert_eq!(
                    parse(&format!(" {name} {level} "), &FileCommands::default()).unwrap(),
                    Some(Command::Think(Some(level.parse().unwrap())))
                );
            }
        }
        for input in [
            "/think HIGH",
            "/think low extra",
            "/think ultra",
            "/think null",
            "/effort HIGH",
            "/effort high extra",
        ] {
            assert!(parse(input, &FileCommands::default()).is_err());
        }
    }
}

#[cfg(test)]
mod mcp_tests {
    use super::*;
    #[test]
    fn slash_mcp_strict_arguments_and_core_commands() {
        let files = FileCommands::default();
        for input in [
            "/mcp",
            "/mcp help",
            "/mcp list",
            "/mcp reload",
            "/mcp enable context7",
            "/mcp disable context7",
            "/mcp reconnect context7",
        ] {
            assert!(matches!(
                parse(input, &files).unwrap(),
                Some(Command::Mcp(_))
            ));
        }
        for input in [
            "/mcp add",
            "/mcp test x",
            "/mcp list x",
            "/mcp enable",
            "/mcp enable a b",
        ] {
            assert!(parse(input, &files).is_err());
        }
    }
}
