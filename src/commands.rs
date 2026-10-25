mod templates;
pub(crate) use templates::{FileCommand, FileCommands};

use crate::config::ThinkLevel;
use anyhow::{Context, Result, ensure};

pub(crate) struct CommandSpec {
    pub(crate) name: &'static str,
    pub(crate) usage: &'static str,
    pub(crate) description: &'static str,
}

pub(crate) const COMMANDS: &[CommandSpec] = &[
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
        name: "/model",
        usage: "/model [provider/model]",
        description: "List or select a configured model without clearing history",
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
        usage: "/plan [view|new|off]",
        description: "Plan a task with read-only research, then explicitly approve execution (TUI)",
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
        name: "/skills",
        usage: "/skills [enable NAME|disable NAME|sources [enable|disable claude|codex]]",
        description: "管理技能来源与逐项启停",
    },
    CommandSpec {
        name: "/quit",
        usage: "/quit",
        description: "Exit pp",
    },
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanCommand {
    Open,
    View,
    New,
    Off,
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
    Plan(PlanCommand),
    Quit,
    Model(Option<&'a str>),
    Think(Option<ThinkLevel>),
    Skills(SkillCommand<'a>),
    Prompt(String),
}

pub(crate) fn parse<'a>(input: &'a str, files: &FileCommands) -> Result<Option<Command<'a>>> {
    let input = input.trim();
    if !input.starts_with('/') || input.contains(['\n', '\r']) {
        return Ok(None);
    }
    let (name, argument) = input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(name, argument)| (name, argument.trim()));
    let command = match name {
        "/help" => Command::Help,
        "/tips" => Command::Tips,
        "/new" => Command::New,
        "/init" => Command::Prompt(crate::project_instructions::INIT_PROMPT.to_owned()),
        "/compact" => Command::Compact,
        "/resume" => Command::Resume,
        "/tree" => Command::Tree,
        "/changes" => Command::Changes,
        "/plan" => {
            return Ok(Some(Command::Plan(match argument {
                "" => PlanCommand::Open,
                "view" => PlanCommand::View,
                "new" => PlanCommand::New,
                "off" => PlanCommand::Off,
                _ => anyhow::bail!("usage: /plan [view|new|off]"),
            })));
        }
        "/quit" => Command::Quit,
        "/model" => {
            return Ok(Some(Command::Model(
                (!argument.is_empty()).then_some(argument),
            )));
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
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

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
            (" /resume ", Command::Resume),
            ("/quit", Command::Quit),
            ("/model", Command::Model(None)),
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
            "/help", "/tips", "/new", "/init", "/compact", "/resume", "/tree", "/quit",
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
