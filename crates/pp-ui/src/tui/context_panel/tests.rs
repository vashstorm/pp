use super::*;
use pp_core::{context::TokenUsage, model::ThinkLevel};
use pp_runtime::agent::context_diagnosis::{
    CategoryKey, CategorySummary, CompactionPreview, CompactionStatus, ContextReport,
    DiagnosticItem,
};
use ratatui::{Terminal, backend::TestBackend};

fn sample_empty_report() -> ContextReport {
    ContextReport {
        model_reference: "test/demo".into(),
        model_name: "demo-model".into(),
        protocol: "response",
        work_mode: "execute",
        think_level: ThinkLevel::Medium,
        empty_session: true,
        active_tokens: 0,
        context_window: Some(128_000),
        auto_compact: true,
        threshold_percent: 80,
        last_request: None,
        session_output_tokens: 0,
        thinking_tokens: 0,
        thinking_in_context: true,
        thinking_turns: 0,
        compaction: CompactionPreview {
            retained_budget_tokens: 16_384,
            summary_budget_tokens: 2_048,
            total_exchanges: 0,
            has_existing_summary: false,
            status: CompactionStatus::NotEnoughHistory,
        },
        categories: Vec::new(),
        system_items: Vec::new(),
        agent_md_items: Vec::new(),
        skill_items: Vec::new(),
        tool_items: Vec::new(),
        mcp_items: Vec::new(),
        message_items: Vec::new(),
        thinking_items: Vec::new(),
        tool_result_items: Vec::new(),
    }
}

fn sample_report() -> ContextReport {
    ContextReport {
        model_reference: "test/demo".into(),
        model_name: "demo-model".into(),
        protocol: "response",
        work_mode: "execute",
        think_level: ThinkLevel::Medium,
        empty_session: false,
        active_tokens: 18_500,
        context_window: Some(128_000),
        auto_compact: true,
        threshold_percent: 80,
        last_request: Some(TokenUsage {
            input_tokens: 17_900,
            output_tokens: 450,
            cached_tokens: Some(12_000),
            cache_write_tokens: Some(1_200),
        }),
        session_output_tokens: 2_300,
        thinking_tokens: 1_200,
        thinking_in_context: true,
        thinking_turns: 1,
        compaction: CompactionPreview {
            retained_budget_tokens: 16_384,
            summary_budget_tokens: 2_048,
            total_exchanges: 4,
            has_existing_summary: false,
            status: CompactionStatus::Ready {
                compressible_exchanges: 2,
                compressible_tokens: 10_000,
                retained_exchanges: 2,
                retained_tokens: 4_500,
            },
        },
        categories: vec![
            CategorySummary {
                key: CategoryKey::System,
                name: "System Prompt",
                tokens: 1_500,
                bytes: 4_500,
                item_count: 2,
                in_context: true,
                description: "Base and runtime".into(),
            },
            CategorySummary {
                key: CategoryKey::AgentsMd,
                name: "AGENTS.md",
                tokens: 800,
                bytes: 2_400,
                item_count: 2,
                in_context: true,
                description: "Workspace rules".into(),
            },
            CategorySummary {
                key: CategoryKey::Skills,
                name: "Skills",
                tokens: 200,
                bytes: 600,
                item_count: 1,
                in_context: true,
                description: "1 in prompt".into(),
            },
            CategorySummary {
                key: CategoryKey::Tools,
                name: "Tools",
                tokens: 1_000,
                bytes: 3_000,
                item_count: 2,
                in_context: true,
                description: "Built-in tool schemas".into(),
            },
            CategorySummary {
                key: CategoryKey::Mcp,
                name: "MCP",
                tokens: 2_500,
                bytes: 7_500,
                item_count: 1,
                in_context: true,
                description: "External MCP schemas".into(),
            },
            CategorySummary {
                key: CategoryKey::Messages,
                name: "Messages",
                tokens: 3_300,
                bytes: 9_900,
                item_count: 4,
                in_context: true,
                description: "Messages".into(),
            },
            CategorySummary {
                key: CategoryKey::Thinking,
                name: "Thinking",
                tokens: 1_200,
                bytes: 3_600,
                item_count: 1,
                in_context: true,
                description: "LLM reasoning".into(),
            },
            CategorySummary {
                key: CategoryKey::ToolResults,
                name: "Tool Results",
                tokens: 8_000,
                bytes: 24_000,
                item_count: 2,
                in_context: true,
                description: "Outputs".into(),
            },
        ],
        system_items: vec![
            DiagnosticItem {
                kind_label: "Summary Row".into(),
                title: "Active System Prompt".into(),
                subtitle: "Matches".into(),
                tokens: 1_500,
                bytes: 4_500,
                preview: "Full system".into(),
                is_header: true,
                rankable: false,
            },
            DiagnosticItem {
                kind_label: "Base".into(),
                title: "Base System Prompt".into(),
                subtitle: "bundled".into(),
                tokens: 1_000,
                bytes: 3_000,
                preview: "Base body".into(),
                is_header: false,
                rankable: true,
            },
        ],
        agent_md_items: vec![DiagnosticItem {
            kind_label: "AGENTS.md".into(),
            title: "AGENTS.md".into(),
            subtitle: "sent".into(),
            tokens: 600,
            bytes: 1_800,
            preview: "Keep tests fast.".into(),
            is_header: false,
            rankable: true,
        }],
        skill_items: vec![DiagnosticItem {
            kind_label: "Skill Index".into(),
            title: "rust".into(),
            subtitle: "skill://rust".into(),
            tokens: 200,
            bytes: 600,
            preview: "Rust guidelines".into(),
            is_header: false,
            rankable: true,
        }],
        tool_items: vec![
            DiagnosticItem {
                kind_label: "Built-in".into(),
                title: "read".into(),
                subtitle: "Read files".into(),
                tokens: 300,
                bytes: 900,
                preview: "{\"name\":\"read\"}".into(),
                is_header: false,
                rankable: true,
            },
            DiagnosticItem {
                kind_label: "Built-in".into(),
                title: "bash".into(),
                subtitle: "Run shell".into(),
                tokens: 700,
                bytes: 2_100,
                preview: "{\"name\":\"bash\"}".into(),
                is_header: false,
                rankable: true,
            },
        ],
        mcp_items: vec![
            DiagnosticItem {
                kind_label: "Summary Row".into(),
                title: "MCP Servers Overview".into(),
                subtitle: "1 server · 1 tool".into(),
                tokens: 2_500,
                bytes: 7_500,
                preview: "- docs [connected] 1 tool".into(),
                is_header: true,
                rankable: false,
            },
            DiagnosticItem {
                kind_label: "MCP".into(),
                title: "docs / search".into(),
                subtitle: "mcp__docs__search".into(),
                tokens: 2_500,
                bytes: 7_500,
                preview: "{\"name\":\"mcp__docs__search\"}".into(),
                is_header: false,
                rankable: true,
            },
        ],
        message_items: vec![DiagnosticItem {
            kind_label: "User".into(),
            title: "#1 User".into(),
            subtitle: "Check code".into(),
            tokens: 100,
            bytes: 300,
            preview: "Check code".into(),
            is_header: false,
            rankable: true,
        }],
        thinking_items: vec![
            DiagnosticItem {
                kind_label: "Summary Row".into(),
                title: "LLM Reasoning in Context".into(),
                subtitle: "1 turn".into(),
                tokens: 1_200,
                bytes: 3_600,
                preview: "Summary".into(),
                is_header: true,
                rankable: false,
            },
            DiagnosticItem {
                kind_label: "Thinking".into(),
                title: "#2 Reasoning (turn 1)".into(),
                subtitle: "Analyzed workspace".into(),
                tokens: 1_200,
                bytes: 3_600,
                preview: "Analyzed workspace files carefully.".into(),
                is_header: false,
                rankable: true,
            },
        ],
        tool_result_items: vec![DiagnosticItem {
            kind_label: "Tool Result".into(),
            title: "#4 Tool Result · read".into(),
            subtitle: "src/main.rs".into(),
            tokens: 7_500,
            bytes: 22_500,
            preview: "Line 1\nLine 2\n".repeat(40),
            is_header: false,
            rankable: true,
        }],
    }
}

fn draw_panel(panel: &mut Panel, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| panel.draw(frame, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn empty_session_renders_clean_empty_state_without_fake_metrics() {
    let mut panel = Panel::new();
    panel.set_report(sample_empty_report());
    let screen = draw_panel(&mut panel, 110, 32);
    assert!(screen.contains("No Context Data Yet"));
    assert!(screen.contains("No messages have been sent"));
    assert!(!screen.contains("Prepared"));
}

#[test]
fn context_panel_drills_down_to_split_tools_mcp_thinking_and_detail_view() {
    let mut panel = Panel::new();
    let loading = draw_panel(&mut panel, 110, 32);
    assert!(loading.contains("Inspecting current session context..."));

    panel.set_report(sample_report());
    let overview = draw_panel(&mut panel, 110, 32);
    assert!(overview.contains("Context Diagnostics"));
    assert!(overview.contains("Context: ~18.5K/128K"));
    assert!(overview.contains("cache 12K"));
    assert!(overview.contains("Thinking: ~1.2K (1t)"));
    assert!(overview.contains("Tools"));
    assert!(overview.contains("MCP"));

    // Overview 上对第 4 行 (Tools) 按 Enter 跳转到 Tools 列表。
    for _ in 0..3 {
        assert!(matches!(panel.handle(key(KeyCode::Down)), Action::None));
    }
    assert!(matches!(panel.handle(key(KeyCode::Enter)), Action::None));
    assert_eq!(panel.category, Category::Tools);
    assert_eq!(panel.view, ViewMode::List);

    // 默认顺序：read (300) 在前；按 s 切换 Tokens 降序：bash (700) 在前。
    assert_eq!(panel.current_items()[0].title, "read");
    assert!(matches!(
        panel.handle(key(KeyCode::Char('s'))),
        Action::None
    ));
    assert_eq!(panel.current_items()[0].title, "bash");

    // 按 Enter 点开具体条目详情，Esc 退回 List，Backspace 退回 Overview，再 Esc 关闭面板。
    assert!(matches!(panel.handle(key(KeyCode::Enter)), Action::None));
    assert_eq!(panel.view, ViewMode::Detail);
    let detail_screen = draw_panel(&mut panel, 110, 32);
    assert!(detail_screen.contains("{\"name\":\"bash\"}"));
    assert!(detail_screen.contains("Esc/Backspace: Back"));

    assert!(matches!(panel.handle(key(KeyCode::Esc)), Action::None));
    assert_eq!(panel.view, ViewMode::List);
    assert_eq!(panel.category, Category::Tools);

    assert!(matches!(
        panel.handle(key(KeyCode::Backspace)),
        Action::None
    ));
    assert_eq!(panel.category, Category::Overview);

    assert!(matches!(
        panel.handle(key(KeyCode::Char('r'))),
        Action::Refresh
    ));
    assert!(matches!(panel.handle(key(KeyCode::Esc)), Action::Close));
}

#[test]
fn app_context_does_not_replace_running_or_cancelling_state() {
    use crate::tui::{App, CancellationToken, State, WorkerEvent};

    for (state, command) in [(State::Running, "/context"), (State::Cancelling, "/ctx")] {
        let original_state = std::mem::discriminant(&state);
        let mut app = App::new(Err(anyhow::anyhow!("no cfg"))).unwrap();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        let cancellation = CancellationToken::new();
        app.commands = Some(cmd_tx);
        app.state = state;
        app.active_cancellation = Some(cancellation.clone());
        app.insert_input(command.into());
        let mut dirty = false;
        app.handle(key(KeyCode::Enter), &mut dirty);
        assert!(app.context_panel.is_some());
        assert!(!app.has_pending());
        assert!(cmd_rx.try_recv().is_err());

        // 迟到的诊断回复不能覆盖主任务的状态。
        app.receive_event_for_test(WorkerEvent::ContextInspected(Ok(sample_report())));
        app.handle(key(KeyCode::Char('r')), &mut dirty);
        assert!(cmd_rx.try_recv().is_err());
        app.handle(key(KeyCode::Esc), &mut dirty);
        assert!(app.context_panel.is_none());
        assert_eq!(std::mem::discriminant(&app.state), original_state);
        assert!(!cancellation.is_cancelled());
        assert!(app.active_cancellation.is_some());
    }
}

#[test]
fn app_submits_context_command_opens_alternate_panel_and_closes_on_escape() {
    let mut app = crate::tui::App::new(Err(anyhow::anyhow!("no cfg"))).unwrap();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    app.commands = Some(cmd_tx);
    app.state = crate::tui::State::Ready;
    app.insert_input("/context".into());
    let mut dirty = false;
    assert!(!app.handle(key(KeyCode::Enter), &mut dirty));
    assert!(matches!(
        cmd_rx.try_recv(),
        Ok(crate::tui::WorkerCommand::InspectContext)
    ));
    assert!(app.needs_alternate());
    assert!(matches!(app.state, crate::tui::State::InspectingContext));

    app.receive_event_for_test(crate::tui::WorkerEvent::ContextInspected(Ok(
        sample_report(),
    )));
    assert!(matches!(app.state, crate::tui::State::Ready));

    let mut terminal = Terminal::new(TestBackend::new(110, 32)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, &crate::tui::StatusContext::default()))
        .unwrap();
    let rendered: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(rendered.contains("Context Diagnostics"));
    assert!(rendered.contains("MCP"));
    assert!(rendered.contains("Thinking"));

    assert!(!app.handle(key(KeyCode::Esc), &mut dirty));
    assert!(!app.needs_alternate());
}

#[test]
fn overview_supports_sorting_eight_categories_by_tokens_while_anchoring_diagnostics_rows() {
    let mut panel = Panel::new();
    let mut report = sample_report();
    report.thinking_in_context = false;
    report.categories[6].in_context = false; // Thinking
    panel.set_report(report);

    let default_screen = draw_panel(&mut panel, 110, 32);
    assert!(default_screen.contains("[Sort: Default]"));
    assert!(default_screen.contains("· s sort"));
    let default_titles: Vec<&str> = panel
        .current_items()
        .iter()
        .map(|item| item.title.as_str())
        .collect();
    assert_eq!(
        default_titles,
        vec![
            "System Prompt",
            "AGENTS.md",
            "Skills",
            "Tools",
            "MCP",
            "Messages",
            "Thinking",
            "Tool Results",
            "Server Usage & Capacity",
            "Compaction Boundary",
        ]
    );

    assert!(matches!(
        panel.handle(key(KeyCode::Char('s'))),
        Action::None
    ));
    let sorted_screen = draw_panel(&mut panel, 110, 32);
    assert!(sorted_screen.contains("[Sort: Tokens ↓]"));
    let sorted_titles: Vec<&str> = panel
        .current_items()
        .iter()
        .map(|item| item.title.as_str())
        .collect();
    assert_eq!(
        sorted_titles,
        vec![
            "Tool Results",
            "Messages",
            "MCP",
            "System Prompt",
            "Thinking",
            "Tools",
            "AGENTS.md",
            "Skills",
            "Server Usage & Capacity",
            "Compaction Boundary",
        ]
    );
    // Thinking moved from index 6 to index 4 and must retain `in_context == false`.
    assert!(!panel.current_row_in_context(4));
    assert!(panel.current_row_in_context(0));

    // Drill down on sorted row 2 ("MCP") -> must open Category::Mcp rather than unsorted index 2 (Skills).
    for _ in 0..2 {
        assert!(matches!(panel.handle(key(KeyCode::Down)), Action::None));
    }
    assert!(matches!(panel.handle(key(KeyCode::Enter)), Action::None));
    assert_eq!(panel.category, Category::Mcp);
    assert_eq!(panel.view, ViewMode::List);

    // Return to Overview and toggle sort back to Default.
    assert!(matches!(
        panel.handle(key(KeyCode::Backspace)),
        Action::None
    ));
    assert_eq!(panel.category, Category::Overview);
    assert!(matches!(
        panel.handle(key(KeyCode::Char('s'))),
        Action::None
    ));
    assert_eq!(panel.current_items()[0].title, "System Prompt");
}

#[test]
fn overview_renders_separator_between_categories_and_diagnostics_and_skips_it_on_click() {
    let mut panel = Panel::new();
    panel.set_report(sample_report());
    let screen = draw_panel(&mut panel, 110, 32);

    let lines: Vec<&str> = screen.lines().collect();
    let diag_line = lines
        .iter()
        .position(|line| line.contains("[Diagnostics] Server Usage & Capacity"))
        .expect("Diagnostics row rendered");
    let last_cat_line = lines
        .iter()
        .position(|line| line.contains("[Category] Tool Results"))
        .expect("last Category row rendered");
    assert_eq!(diag_line, last_cat_line + 2);
    assert!(lines[last_cat_line + 1].contains("──────────"));

    // Clicking the separator line should not select Diagnostics or move selection.
    let list_x = panel.list_area.x + 2;
    let sep_y = panel.list_area.y + 8;
    let click_sep = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: list_x,
        row: sep_y,
        modifiers: KeyModifiers::NONE,
    });
    assert!(matches!(panel.handle(click_sep), Action::None));
    assert_eq!(panel.list_index, 0);

    // Clicking Diagnostics right below the separator selects index 8 (`Server Usage & Capacity`).
    let click_diag = Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: list_x,
        row: sep_y + 1,
        modifiers: KeyModifiers::NONE,
    });
    assert!(matches!(panel.handle(click_diag), Action::None));
    assert_eq!(panel.list_index, 8);
    assert_eq!(
        panel.current_items()[panel.list_index].title,
        "Server Usage & Capacity"
    );
}
