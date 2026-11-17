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
    use pp_config::skills::SkillSource;
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
fn slash_setting_roles_and_agents_casing_and_aliases() {
    let files = FileCommands::default();
    for (input, expected) in [
        ("/setting", Command::Setting(SettingTab::General)),
        (" /settings ", Command::Setting(SettingTab::General)),
        ("/config", Command::Setting(SettingTab::General)),
        ("/setting general", Command::Setting(SettingTab::General)),
        ("/config roles", Command::Setting(SettingTab::Roles)),
        ("/settings themes", Command::Setting(SettingTab::Themes)),
        ("/setting skills", Command::Setting(SettingTab::Skills)),
        ("/roles", Command::Roles),
        ("/agents", Command::Agents),
    ] {
        assert_eq!(parse(input, &files).unwrap(), Some(expected), "{input}");
    }
    for unknown in ["/Roles", "/subagents", "/Config"] {
        assert_eq!(
            parse(unknown, &files).unwrap(),
            Some(Command::Unknown),
            "{unknown}"
        );
    }
    for invalid in [
        "/settings unknown",
        "/setting unknown",
        "/config general extra",
        "/roles extra",
        "/agents extra",
    ] {
        assert!(parse(invalid, &files).is_err(), "{invalid}");
    }
    assert_eq!(
        parse("/setting unknown", &files).unwrap_err().to_string(),
        "usage: /setting [general|roles|themes|skills]"
    );
}

#[test]
fn slash_valid_commands_and_model_reference_boundaries() {
    for (input, expected) in [
        (" /help \n", Command::Help),
        (" /tips ", Command::Tips),
        (" /stats ", Command::Stats("")),
        (
            "/stats --port 0 --no-open",
            Command::Stats("--port 0 --no-open"),
        ),
        ("\t/new ", Command::New),
        (" /compact ", Command::Compact),
        ("/context", Command::Context),
        (" /ctx ", Command::Context),
        (
            " /init ",
            Command::Prompt(pp_tools::project_instructions::INIT_PROMPT.to_owned()),
        ),
        ("/resume", Command::Resume),
        ("/tree", Command::Tree),
        ("/plan-list", Command::PlanList),
        (" /plan-list ", Command::PlanList),
        #[cfg(debug_assertions)]
        ("/test-task", Command::MockTest(MockTest::Task)),
        #[cfg(debug_assertions)]
        (" /test-task ", Command::MockTest(MockTest::Task)),
        #[cfg(debug_assertions)]
        ("/test-ui", Command::MockTest(MockTest::Ui)),
        #[cfg(debug_assertions)]
        (" /test-ui ", Command::MockTest(MockTest::Ui)),
        #[cfg(debug_assertions)]
        ("/test-tools", Command::MockTest(MockTest::All)),
        #[cfg(debug_assertions)]
        ("/test-read", Command::MockTest(MockTest::Read)),
        #[cfg(debug_assertions)]
        ("/test-write", Command::MockTest(MockTest::Write)),
        #[cfg(debug_assertions)]
        ("/test-edit", Command::MockTest(MockTest::Edit)),
        #[cfg(debug_assertions)]
        ("/test-rg", Command::MockTest(MockTest::Rg)),
        #[cfg(debug_assertions)]
        ("/test-glob", Command::MockTest(MockTest::Glob)),
        #[cfg(debug_assertions)]
        ("/test-bash", Command::MockTest(MockTest::Bash)),
        #[cfg(debug_assertions)]
        ("/test-bash-task", Command::MockTest(MockTest::BashTask)),
        #[cfg(debug_assertions)]
        ("/test-eval-py", Command::MockTest(MockTest::EvalPy)),
        #[cfg(debug_assertions)]
        ("/test-wait", Command::MockTest(MockTest::Wait)),
        #[cfg(debug_assertions)]
        ("/test-todo", Command::MockTest(MockTest::Todo)),
        #[cfg(debug_assertions)]
        ("/test-ask", Command::MockTest(MockTest::Ask)),
        #[cfg(debug_assertions)]
        ("/test-fail", Command::MockTest(MockTest::Fail)),
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
fn slash_unknown_commands_are_silently_ignored() {
    for input in ["/", "/bogus", "/Help", "/modelx", "/task-test"] {
        assert_eq!(
            parse(input, &FileCommands::default()).unwrap(),
            Some(Command::Unknown)
        );
    }
}

#[test]
fn slash_extra_arguments_are_local_errors() {
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
        #[cfg(debug_assertions)]
        "/test-task",
        #[cfg(debug_assertions)]
        "/test-ui",
        #[cfg(debug_assertions)]
        "/test-tools",
        #[cfg(debug_assertions)]
        "/test-read",
        #[cfg(debug_assertions)]
        "/test-write",
        #[cfg(debug_assertions)]
        "/test-edit",
        #[cfg(debug_assertions)]
        "/test-rg",
        #[cfg(debug_assertions)]
        "/test-glob",
        #[cfg(debug_assertions)]
        "/test-bash",
        #[cfg(debug_assertions)]
        "/test-bash-task",
        #[cfg(debug_assertions)]
        "/test-eval-py",
        #[cfg(debug_assertions)]
        "/test-wait",
        #[cfg(debug_assertions)]
        "/test-todo",
        #[cfg(debug_assertions)]
        "/test-ask",
    ] {
        assert_eq!(
            parse(&format!("{command} extra"), &FileCommands::default())
                .unwrap_err()
                .to_string(),
            format!("usage: {command}")
        );
    }
}

/// 15 个 `/test-*` 命令名：仅含 debug assertions 的构建注册并解析。
const TEST_COMMAND_NAMES: [&str; 15] = [
    "/test-task",
    "/test-ui",
    "/test-tools",
    "/test-read",
    "/test-write",
    "/test-edit",
    "/test-rg",
    "/test-glob",
    "/test-bash",
    "/test-bash-task",
    "/test-eval-py",
    "/test-wait",
    "/test-todo",
    "/test-ask",
    "/test-fail",
];

#[cfg(debug_assertions)]
#[test]
fn slash_mock_tests_match_all_and_reject_arguments() {
    assert_eq!(MockTest::ALL.len(), TEST_COMMAND_NAMES.len());
    let files = FileCommands::default();
    for test in MockTest::ALL {
        assert!(
            COMMANDS.iter().any(|spec| spec.name == test.name()),
            "{} is registered",
            test.name()
        );
        assert!(TEST_COMMAND_NAMES.contains(&test.name()));
        assert_eq!(
            parse(test.name(), &files).unwrap(),
            Some(Command::MockTest(test)),
            "{}",
            test.name()
        );
        assert_eq!(
            parse(&format!("{} extra", test.name()), &files)
                .unwrap_err()
                .to_string(),
            format!("usage: {}", test.name()),
            "{} extra",
            test.name()
        );
    }
}

#[test]
fn slash_help_lists_debug_only_commands_in_debug_builds() {
    let text = help(&FileCommands::default());
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.first().copied(), Some("Help:"));
    for title in [
        "Session & History",
        "Models & Thinking",
        "Workflow & Execution",
        "Settings & Extensions",
    ] {
        assert!(lines.contains(&title), "missing section {title}\n{text}");
    }
    for name in TEST_COMMAND_NAMES {
        assert_eq!(
            lines
                .iter()
                .any(|line| line.starts_with("  ") && line[2..].starts_with(&format!("{name} "))),
            cfg!(debug_assertions),
            "{name}\n{text}"
        );
    }
    // 普通命令在所有构建都注册、解析并出现在帮助里。
    for name in [
        "/help",
        "/tips",
        "/setting",
        "/roles",
        "/agents",
        "/plan-list",
        "/quit",
        "!command",
    ] {
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("  ") && line[2..].starts_with(&format!("{name} "))),
            "{name}\n{text}"
        );
    }
    for removed in ["/Roles", "/subagents", "/config"] {
        assert!(
            !lines
                .iter()
                .any(|line| line.trim_start().starts_with(&format!("{removed} "))),
            "{removed}\n{text}"
        );
    }
}

#[test]
fn slash_themes_parses_icons_and_theme_arguments() {
    use pp_config::config::{IconMode, Theme};

    let files = FileCommands::default();
    assert_eq!(
        parse("/themes", &files).unwrap(),
        Some(Command::Themes(ThemeCommand {
            theme: None,
            icons: None,
        }))
    );
    assert_eq!(
        parse("/theme ascii tokyonight", &files).unwrap(),
        Some(Command::Themes(ThemeCommand {
            theme: Some(Theme::TokyoNight),
            icons: Some(IconMode::Ascii),
        }))
    );
    assert_eq!(
        parse("/themes dracula nerd", &files).unwrap(),
        Some(Command::Themes(ThemeCommand {
            theme: Some(Theme::Dracula),
            icons: Some(IconMode::Nerd),
        }))
    );
    for invalid in [
        "/themes unknown",
        "/themes ascii nerd",
        "/themes nord dracula",
        "/themes ascii tokyonight extra",
    ] {
        assert!(parse(invalid, &files).is_err(), "{invalid}");
    }
}

#[cfg(not(debug_assertions))]
#[test]
fn slash_test_commands_are_unknown_without_debug_assertions() {
    let files = FileCommands::default();
    for name in TEST_COMMAND_NAMES {
        assert_eq!(
            parse(name, &files).unwrap(),
            Some(Command::Unknown),
            "{name}"
        );
        assert_eq!(
            parse(&format!(" {name} extra "), &files).unwrap(),
            Some(Command::Unknown),
            "{name} extra"
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

#[test]
fn slash_pipes_accepts_zero_or_one_lowercase_hex_run_id() {
    let files = FileCommands::default();
    for input in ["/pipes", " /pipes  ", "/pipes\t"] {
        assert_eq!(
            parse(input, &files).unwrap(),
            Some(Command::Pipes(None)),
            "{input}"
        );
    }
    let run = "0123456789abcdef0123456789abcdef";
    assert_eq!(
        parse(&format!(" /pipes {run} "), &files).unwrap(),
        Some(Command::Pipes(Some(run)))
    );
    for input in [
        "/pipes 0123456789ABCDEF0123456789abcdef",
        "/pipes 0123456789abcdef0123456789abcde",
        "/pipes 0123456789abcdef0123456789abcdef0",
        "/pipes 0123456789abcdef0123456789abcdeg",
        "/pipes two ids",
    ] {
        assert_eq!(
            parse(input, &files).unwrap_err().to_string(),
            "usage: /pipes [run-id]",
            "{input}"
        );
    }
}

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

#[test]
fn slash_sandbox_parses_modes_and_rejects_invalid_args() {
    let files = FileCommands::default();
    assert_eq!(
        parse("/sandbox", &files).unwrap(),
        Some(Command::Sandbox(None))
    );
    for mode in ["auto", "workspace-write", "read-only", "off"] {
        assert_eq!(
            parse(&format!("/sandbox {mode}"), &files).unwrap(),
            Some(Command::Sandbox(Some(mode.parse().unwrap())))
        );
    }
    assert!(parse("/sandbox invalid", &files).is_err());
    assert!(parse("/sandbox off extra", &files).is_err());
}
