use crate::config::ThinkLevel;
use anyhow::{Context, Result, bail, ensure};

pub(crate) struct CommandSpec {
    pub(crate) name: &'static str,
    pub(crate) icon: &'static str,
    pub(crate) usage: &'static str,
    pub(crate) description: &'static str,
}

pub(crate) const COMMANDS: &[CommandSpec] = &[
    CommandSpec {
        name: "/help",
        icon: "\u{f059}",
        usage: "/help",
        description: "Show available commands",
    },
    CommandSpec {
        name: "/model",
        icon: "\u{ec19}",
        usage: "/model [provider/model]",
        description: "List or select a configured model without clearing history",
    },
    CommandSpec {
        name: "/new",
        icon: "\u{f021}",
        usage: "/new",
        description: "Reset the session, keeping the current model",
    },
    CommandSpec {
        name: "/effort",
        icon: "\u{f0eb}",
        usage: "/effort [none|low|medium|high|xhigh|max]",
        description: "View or set the thinking effort level, keeping session history",
    },
    CommandSpec {
        name: "/skills",
        icon: "◆",
        usage: "/skills [enable 名称|disable 名称]",
        description: "管理全局 skill 启用状态",
    },
    CommandSpec {
        name: "/quit",
        icon: "\u{f08b}",
        usage: "/quit",
        description: "Exit pp",
    },
];

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Command<'a> {
    Help,
    New,
    Quit,
    Model(Option<&'a str>),
    Think(Option<ThinkLevel>),
    Skills(Option<(&'a str, bool)>),
}

pub(crate) fn parse(input: &str) -> Result<Option<Command<'_>>> {
    let input = input.trim();
    if !input.starts_with('/') || input.contains(['\n', '\r']) {
        return Ok(None);
    }
    let (name, argument) = input
        .split_once(char::is_whitespace)
        .map_or((input, ""), |(name, argument)| (name, argument.trim()));
    let command = match name {
        "/help" => Command::Help,
        "/new" => Command::New,
        "/quit" => Command::Quit,
        "/model" => {
            return Ok(Some(Command::Model(
                (!argument.is_empty()).then_some(argument),
            )));
        }
        "/skills" => {
            if argument.is_empty() {
                return Ok(Some(Command::Skills(None)));
            }
            let (action, name) = argument
                .split_once(char::is_whitespace)
                .context("用法：/skills [enable 名称|disable 名称]")?;
            ensure!(!name.trim().is_empty(), "skill 名称不能为空");
            ensure!(
                matches!(action, "enable" | "disable"),
                "用法：/skills [enable 名称|disable 名称]"
            );
            return Ok(Some(Command::Skills(Some((
                name.trim(),
                action == "enable",
            )))));
        }
        "/effort" | "/think" => {
            return Ok(Some(Command::Think(
                (!argument.is_empty())
                    .then(|| argument.parse())
                    .transpose()?,
            )));
        }
        _ => bail!("unknown command; use /help"),
    };
    let spec = COMMANDS
        .iter()
        .find(|spec| spec.name == name)
        .expect("registered command");
    ensure!(argument.is_empty(), "usage: {}", spec.usage);
    Ok(Some(command))
}

pub(crate) fn help() -> String {
    COMMANDS
        .iter()
        .map(|spec| format!("{} — {}", spec.usage, spec.description))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_valid_commands_and_model_reference_boundaries() {
        for (input, expected) in [
            (" /help \n", Command::Help),
            ("\t/new ", Command::New),
            ("/quit", Command::Quit),
            ("/model", Command::Model(None)),
            (
                " /model  gateway/team/model with spaces  ",
                Command::Model(Some("gateway/team/model with spaces")),
            ),
        ] {
            assert_eq!(parse(input).unwrap(), Some(expected));
        }
    }

    #[test]
    fn slash_unknown_and_extra_arguments_are_local_errors() {
        for input in ["/", "/bogus", "/Help", "/modelx"] {
            assert_eq!(
                parse(input).unwrap_err().to_string(),
                "unknown command; use /help"
            );
        }
        for command in ["/help", "/new", "/quit"] {
            assert_eq!(
                parse(&format!("{command} extra")).unwrap_err().to_string(),
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
            "/help\r\ncode",
            "text /quit",
        ] {
            assert_eq!(parse(input).unwrap(), None);
        }
    }

    #[test]
    fn think_commands_validate_levels_locally() {
        assert_eq!(parse("/think").unwrap(), Some(Command::Think(None)));
        assert_eq!(parse("/effort ").unwrap(), Some(Command::Think(None)));
        for name in ["/effort", "/think"] {
            for level in ["none", "default", "low", "medium", "high", "xhigh", "max"] {
                assert_eq!(
                    parse(&format!(" {name} {level} ")).unwrap(),
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
            assert!(parse(input).is_err());
        }
    }
}
