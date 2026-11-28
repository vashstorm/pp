use super::*;

#[test]
fn slash_builtin_commands_parse_and_validate_usage() {
    use pp_config::config::{IconMode, Theme, ToolStyle};
    use pp_config::skills::SkillSource;
    let files = FileCommands::default();
    for (input, expected) in [
        (" /plan \t", PlanCommand::Toggle),
        ("/plan view", PlanCommand::View),
        (
            "/plan  investigate login flow  ",
            PlanCommand::Prompt("investigate login flow"),
        ),
        ("/plan\tview logs", PlanCommand::Prompt("view logs")),
        (
            "/plan first line\nsecond line",
            PlanCommand::Prompt("first line\nsecond line"),
        ),
        ("/plan new task", PlanCommand::Prompt("new task")),
        ("/plan off switch", PlanCommand::Prompt("off switch")),
    ] {
        assert_eq!(parse(input, &files).unwrap(), Some(Command::Plan(expected)));
    }
    for input in ["/plan new", "/plan off", " /plan new ", "/plan\toff"] {
        assert_eq!(
            parse(input, &files).unwrap_err().to_string(),
            "Use /plan to toggle; manage plans with /plan-list."
        );
    }
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

    for (input, expected) in [
        ("/skills", Command::Skills(SkillCommand::List)),
        ("/skills sources", Command::Skills(SkillCommand::Sources)),
        (
            "/skills sources enable claude",
            Command::Skills(SkillCommand::Source(SkillSource::Claude, true)),
        ),
        (
            "/skills sources\tdisable codex",
            Command::Skills(SkillCommand::Source(SkillSource::Codex, false)),
        ),
        (
            "/skills disable rust",
            Command::Skills(SkillCommand::Toggle("rust", false)),
        ),
        (
            "/skills enable sources",
            Command::Skills(SkillCommand::Toggle("sources", true)),
        ),
        ("/setting", Command::Setting(SettingTab::General)),
        (" /settings ", Command::Setting(SettingTab::General)),
        ("/config", Command::Setting(SettingTab::General)),
        ("/setting general", Command::Setting(SettingTab::General)),
        ("/config roles", Command::Setting(SettingTab::Roles)),
        ("/settings themes", Command::Setting(SettingTab::Themes)),
        ("/setting skills", Command::Setting(SettingTab::Skills)),
        ("/roles", Command::Roles),
        ("/agents", Command::Agents),
        (
            "/themes",
            Command::Themes(ThemeCommand {
                theme: None,
                icons: None,
                tools: None,
            }),
        ),
        (
            "/theme ascii tokyonight",
            Command::Themes(ThemeCommand {
                theme: Some(Theme::TokyoNight),
                icons: Some(IconMode::Ascii),
                tools: None,
            }),
        ),
        (
            "/themes dracula nerd",
            Command::Themes(ThemeCommand {
                theme: Some(Theme::Dracula),
                icons: Some(IconMode::Nerd),
                tools: None,
            }),
        ),
        (
            "/themes rail",
            Command::Themes(ThemeCommand {
                theme: None,
                icons: None,
                tools: Some(ToolStyle::Rail),
            }),
        ),
        (
            "/themes ascii dracula ledger",
            Command::Themes(ThemeCommand {
                theme: Some(Theme::Dracula),
                icons: Some(IconMode::Ascii),
                tools: Some(ToolStyle::Ledger),
            }),
        ),
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
        (" /resume ", Command::Resume),
        ("/quit", Command::Quit),
        ("/model", Command::Model(None)),
        ("/models", Command::Model(None)),
        ("/models fixture/one", Command::Model(Some("fixture/one"))),
        (
            " /model  gateway/team/model with spaces  ",
            Command::Model(Some("gateway/team/model with spaces")),
        ),
        ("/", Command::Unknown),
        ("/bogus", Command::Unknown),
        ("/Help", Command::Unknown),
        ("/modelx", Command::Unknown),
        ("/task-test", Command::Unknown),
        ("/Roles", Command::Unknown),
        ("/subagents", Command::Unknown),
        ("/Config", Command::Unknown),
    ] {
        assert_eq!(parse(input, &files).unwrap(), Some(expected), "{input}");
    }
    for mcp in [
        "/mcp",
        "/mcp help",
        "/mcp list",
        "/mcp reload",
        "/mcp enable context7",
        "/mcp disable context7",
        "/mcp reconnect context7",
    ] {
        assert!(
            matches!(parse(mcp, &files).unwrap(), Some(Command::Mcp(_))),
            "{mcp}"
        );
    }
    for invalid in [
        "/skills sources enable",
        "/skills sources toggle claude",
        "/skills sources enable unknown",
        "/skills sources enable claude extra",
        "/settings unknown",
        "/config general extra",
        "/roles extra",
        "/agents extra",
        "/themes unknown",
        "/themes ascii nerd",
        "/themes nord dracula",
        "/themes rail slab",
        "/themes ascii tokyonight rail extra",
        "/mcp add",
        "/mcp test x",
        "/mcp list x",
        "/mcp enable",
        "/mcp enable a b",
    ] {
        assert!(parse(invalid, &files).is_err(), "{invalid}");
    }
    assert_eq!(
        parse("/setting unknown", &files).unwrap_err().to_string(),
        "usage: /setting [general|roles|themes|skills]"
    );
    for non_cmd in [
        "",
        "  ",
        "task",
        "/model\nother",
        "/resume\nsession-id",
        "/resume\r\nextra",
        "/help\r\ncode",
        "text /quit",
    ] {
        assert_eq!(parse(non_cmd, &files).unwrap(), None, "{non_cmd}");
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
        #[cfg(debug_assertions)]
        "/test-permission",
    ] {
        assert_eq!(
            parse(&format!("{command} extra"), &files)
                .unwrap_err()
                .to_string(),
            format!("usage: {command}")
        );
    }
}

/// The 16 embedded mock scenario command names: registered and parsed only in builds
/// with debug assertions.
const TEST_COMMAND_NAMES: [&str; 16] = [
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
    "/test-eval-js",
    "/test-wait",
    "/test-todo",
    "/test-ask",
    "/test-fail",
];

#[cfg(debug_assertions)]
#[test]
fn slash_debug_mock_and_web_search_commands_parse() {
    use pp_config::config::WebSearchProvider;

    let files = FileCommands::default();
    assert_eq!(MockTest::ALL.len(), TEST_COMMAND_NAMES.len());
    for test in MockTest::ALL {
        assert!(COMMANDS.iter().any(|spec| spec.name == test.name()));
        assert!(TEST_COMMAND_NAMES.contains(&test.name()));
        assert_eq!(
            parse(test.name(), &files).unwrap(),
            Some(Command::MockTest(test))
        );
        assert_eq!(
            parse(&format!("{} extra", test.name()), &files)
                .unwrap_err()
                .to_string(),
            format!("usage: {}", test.name())
        );
    }
    for (input, expected_provider, expected_queries) in [
        (
            "/test-web-search",
            None,
            vec![
                "site:news.ycombinator.com rust programming language",
                "site:github.com rust programming language",
            ],
        ),
        ("/test-web-search rust async", None, vec!["rust async"]),
        (
            "/test-web-search --provider duckduckgo",
            Some(WebSearchProvider::Duckduckgo),
            vec!["rust programming language"],
        ),
        (
            " /test-web-search --provider=SearXNG site:rust-lang.org ownership ",
            Some(WebSearchProvider::Searxng),
            vec!["site:rust-lang.org ownership"],
        ),
        (
            "/test-web-search -- --literal-query",
            None,
            vec!["--literal-query"],
        ),
    ] {
        assert_eq!(
            parse(input, &files).unwrap(),
            Some(Command::TestWebSearch(WebSearchTestCommand {
                provider: expected_provider,
                queries: expected_queries.into_iter().map(str::to_owned).collect(),
            })),
            "{input}"
        );
    }
    for invalid in [
        "/test-web-search --provider",
        "/test-web-search --provider=",
        "/test-web-search --provider brave rust",
        "/test-web-search --provider public --provider auto rust",
        "/test-web-search --unknown rust",
    ] {
        assert!(parse(invalid, &files).is_err(), "{invalid}");
    }
}

#[test]
fn slash_help_and_think_sandbox_dump_args() {
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
    for name in TEST_COMMAND_NAMES
        .iter()
        .copied()
        .chain(["/test-web-search", "/test-permission"])
    {
        assert_eq!(
            lines
                .iter()
                .any(|line| line.starts_with("  ") && line[2..].starts_with(&format!("{name} "))),
            cfg!(debug_assertions),
            "{name}\n{text}"
        );
    }
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

    use pp_runtime::agent::DumpMode;
    let files = FileCommands::default();
    assert_eq!(parse("/think", &files).unwrap(), Some(Command::Think(None)));
    assert_eq!(
        parse("/effort ", &files).unwrap(),
        Some(Command::Think(None))
    );
    for name in ["/effort", "/think"] {
        for level in ["none", "default", "low", "medium", "high", "xhigh", "max"] {
            assert_eq!(
                parse(&format!(" {name} {level} "), &files).unwrap(),
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
        assert!(parse(input, &files).is_err());
    }
    assert_eq!(parse("/pipes", &files).unwrap(), Some(Command::Unknown));
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
    assert_eq!(
        parse("/dump all", &files).unwrap(),
        Some(Command::Dump(DumpMode::All))
    );
    assert_eq!(
        parse(" /dump  result ", &files).unwrap(),
        Some(Command::Dump(DumpMode::Result))
    );
    for invalid in ["/dump", "/dump invalid", "/dump all extra"] {
        assert_eq!(
            parse(invalid, &files).unwrap_err().to_string(),
            "usage: /dump <all|result>",
            "{invalid}"
        );
    }
}
