use super::*;
use pp_core::{context::TokenUsage, model::ThinkLevel};
use pp_runtime::agent::context_diagnosis::{
    CategorySummary, CompactionPreview, CompactionStatus, ContextReport, DiagnosticItem,
};
use ratatui::{Terminal, backend::TestBackend};

fn sample_report() -> ContextReport {
    ContextReport {
        model_reference: "test/demo".into(),
        model_name: "demo-model".into(),
        protocol: "response",
        work_mode: "execute",
        think_level: ThinkLevel::Medium,
        empty_session: false,
        active_tokens: 18_500,
        prepared_tokens: 18_500,
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
                name: "System Prompt",
                tokens: 1_500,
                bytes: 4_500,
                item_count: 2,
                description: "Base and runtime".into(),
            },
            CategorySummary {
                name: "AGENTS.md",
                tokens: 800,
                bytes: 2_400,
                item_count: 2,
                description: "Workspace rules".into(),
            },
            CategorySummary {
                name: "Skills Index",
                tokens: 200,
                bytes: 600,
                item_count: 1,
                description: "1 enabled".into(),
            },
            CategorySummary {
                name: "Tools & MCP",
                tokens: 3_500,
                bytes: 10_500,
                item_count: 3,
                description: "Built-in and MCP".into(),
            },
            CategorySummary {
                name: "History Messages",
                tokens: 4_500,
                bytes: 13_500,
                item_count: 4,
                description: "Messages".into(),
            },
            CategorySummary {
                name: "Tool Results",
                tokens: 8_000,
                bytes: 24_000,
                item_count: 2,
                description: "Outputs".into(),
            },
        ],
        largest_items: vec![DiagnosticItem {
            kind_label: "Tool Result".into(),
            title: "#4 Tool Result · read".into(),
            subtitle: "src/main.rs".into(),
            tokens: 7_500,
            bytes: 22_500,
            preview: "Line 1\nLine 2\n".repeat(40),
            is_header: false,
            rankable: true,
        }],
        system_items: vec![
            DiagnosticItem {
                kind_label: "Summary Row".into(),
                title: "Full Composed System Prompt".into(),
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
        tool_items: vec![
            DiagnosticItem {
                kind_label: "Built-in".into(),
                title: "read".into(),
                subtitle: "Read files".into(),
                tokens: 500,
                bytes: 1_500,
                preview: "{\"name\":\"read\"}".into(),
                is_header: false,
                rankable: true,
            },
            DiagnosticItem {
                kind_label: "MCP".into(),
                title: "mcp__docs__search".into(),
                subtitle: "server: docs · tool: search".into(),
                tokens: 2_500,
                bytes: 7_500,
                preview: "{\"name\":\"mcp__docs__search\"}".into(),
                is_header: false,
                rankable: true,
            },
        ],
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
        history_items: vec![
            DiagnosticItem {
                kind_label: "Summary Row".into(),
                title: "History Breakdown by Role / Type".into(),
                subtitle: "6 items".into(),
                tokens: 12_500,
                bytes: 37_500,
                preview: "Breakdown".into(),
                is_header: true,
                rankable: false,
            },
            DiagnosticItem {
                kind_label: "User".into(),
                title: "#1 User".into(),
                subtitle: "Check code".into(),
                tokens: 100,
                bytes: 300,
                preview: "Check code".into(),
                is_header: false,
                rankable: true,
            },
            DiagnosticItem {
                kind_label: "Tool Result".into(),
                title: "#4 Tool Result · read".into(),
                subtitle: "src/main.rs".into(),
                tokens: 7_500,
                bytes: 22_500,
                preview: "Line 1\nLine 2\n".repeat(40),
                is_header: false,
                rankable: true,
            },
        ],
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
fn context_panel_navigates_categories_sorts_scrolls_and_closes() {
    let mut panel = Panel::new();
    let loading = draw_panel(&mut panel, 110, 32);
    assert!(loading.contains("Inspecting current session context..."));

    panel.set_report(sample_report());
    let overview = draw_panel(&mut panel, 110, 32);
    assert!(overview.contains("Context Diagnostics"));
    assert!(overview.contains("Active: ~18.5K/128K"));
    assert!(overview.contains("cache read 12K"));
    assert!(overview.contains("Compact: 2 exch (~10K)"));
    assert!(overview.contains("Tools & MCP"));

    // Overview 上对第 4 行 (Tools & MCP) 按 Enter 直接跳转到对应分类。
    for _ in 0..3 {
        assert!(matches!(panel.handle(key(KeyCode::Down)), Action::None));
    }
    assert!(matches!(panel.handle(key(KeyCode::Enter)), Action::None));
    assert_eq!(panel.category, Category::Tools);

    // 默认顺序：read 在前；按 s 切换 Tokens 降序：mcp__docs__search (2500) 在前。
    assert_eq!(panel.current_items()[0].title, "read");
    assert!(matches!(
        panel.handle(key(KeyCode::Char('s'))),
        Action::None
    ));
    assert_eq!(panel.current_items()[0].title, "mcp__docs__search");

    // 按 Tab / Enter 切入 Preview 滚动，Esc 退回 List，再按 Esc 关闭面板。
    assert!(matches!(panel.handle(key(KeyCode::Enter)), Action::None));
    assert_eq!(panel.focus, Focus::Preview);
    assert!(matches!(panel.handle(key(KeyCode::Esc)), Action::None));
    assert_eq!(panel.focus, Focus::List);
    assert!(matches!(
        panel.handle(key(KeyCode::Char('r'))),
        Action::Refresh
    ));
    assert!(matches!(panel.handle(key(KeyCode::Esc)), Action::Close));
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
    assert!(rendered.contains("Tools & MCP"));

    assert!(!app.handle(key(KeyCode::Esc), &mut dirty));
    assert!(!app.needs_alternate());
}
