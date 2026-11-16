use super::*;
use crossterm::event::KeyModifiers;
use pp_config::config::{IconMode, Theme};
use pp_core::{context::TokenUsage, model::ThinkLevel};
use pp_runtime::agent::context_diagnosis::{
    CategoryKey, CategorySummary, ContextReport, DiagnosticItem,
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
        active_tokens: 7_720,
        context_window: Some(500_000),
        auto_compact: true,
        threshold_percent: 85,
        last_request: None,
        session_output_tokens: 0,
        thinking_tokens: 0,
        thinking_in_context: true,
        thinking_turns: 0,
        categories: vec![
            CategorySummary {
                key: CategoryKey::System,
                name: "System Prompt",
                tokens: 2_500,
                bytes: 7_500,
                item_count: 2,
                in_context: true,
                items: vec![
                    DiagnosticItem {
                        kind_label: "Base".into(),
                        title: "Base System Prompt (bundled)".into(),
                        subtitle: "bundled".into(),
                        tokens: 1_800,
                        bytes: 5_400,
                        preview: "Base body".into(),
                    },
                    DiagnosticItem {
                        kind_label: "Runtime".into(),
                        title: "Runtime Tool & Environment Rules".into(),
                        subtitle: "prompts/runtime.md".into(),
                        tokens: 700,
                        bytes: 2_100,
                        preview: "Runtime rules".into(),
                    },
                ],
            },
            CategorySummary {
                key: CategoryKey::InstructionFiles,
                name: "Instruction Files",
                tokens: 53,
                bytes: 159,
                item_count: 1,
                in_context: true,
                items: vec![DiagnosticItem {
                    kind_label: "AGENTS.md".into(),
                    title: "AGENTS.md".into(),
                    subtitle: "startup scope".into(),
                    tokens: 53,
                    bytes: 159,
                    preview: "Keep tests fast.".into(),
                }],
            },
            CategorySummary {
                key: CategoryKey::Skills,
                name: "Skills",
                tokens: 467,
                bytes: 1_401,
                item_count: 1,
                in_context: true,
                items: vec![DiagnosticItem {
                    kind_label: "Skill".into(),
                    title: "rust".into(),
                    subtitle: "skill://rust".into(),
                    tokens: 467,
                    bytes: 1_401,
                    preview: "Rust skill".into(),
                }],
            },
            CategorySummary {
                key: CategoryKey::BuiltInTools,
                name: "Built-in Tools",
                tokens: 4_700,
                bytes: 14_100,
                item_count: 2,
                in_context: true,
                items: vec![
                    DiagnosticItem {
                        kind_label: "Built-in".into(),
                        title: "read".into(),
                        subtitle: "Read files".into(),
                        tokens: 1_200,
                        bytes: 3_600,
                        preview: "{\"name\":\"read\"}".into(),
                    },
                    DiagnosticItem {
                        kind_label: "Built-in".into(),
                        title: "bash".into(),
                        subtitle: "Run shell".into(),
                        tokens: 3_500,
                        bytes: 10_500,
                        preview: "{\"name\":\"bash\"}".into(),
                    },
                ],
            },
            CategorySummary {
                key: CategoryKey::CustomTools,
                name: "Custom Tools",
                tokens: 0,
                bytes: 0,
                item_count: 0,
                in_context: true,
                items: Vec::new(),
            },
            CategorySummary {
                key: CategoryKey::UserMessages,
                name: "User / Developer Messages",
                tokens: 0,
                bytes: 0,
                item_count: 0,
                in_context: true,
                items: Vec::new(),
            },
            CategorySummary {
                key: CategoryKey::AssistantMessages,
                name: "Assistant Messages",
                tokens: 0,
                bytes: 0,
                item_count: 0,
                in_context: true,
                items: Vec::new(),
            },
            CategorySummary {
                key: CategoryKey::Thinking,
                name: "Assistant Thinking",
                tokens: 0,
                bytes: 0,
                item_count: 0,
                in_context: true,
                items: Vec::new(),
            },
            CategorySummary {
                key: CategoryKey::ToolCalls,
                name: "Tool Calls",
                tokens: 0,
                bytes: 0,
                item_count: 0,
                in_context: true,
                items: Vec::new(),
            },
            CategorySummary {
                key: CategoryKey::ToolOutput,
                name: "Tool Output",
                tokens: 0,
                bytes: 0,
                item_count: 0,
                in_context: true,
                items: Vec::new(),
            },
        ],
    }
}

fn sample_report() -> ContextReport {
    let mut report = sample_empty_report();
    report.empty_session = false;
    report.active_tokens = 25_000;
    report.last_request = Some(TokenUsage {
        input_tokens: 24_100,
        output_tokens: 450,
        cached_tokens: Some(12_000),
        cache_write_tokens: Some(1_200),
    });
    report.session_output_tokens = 2_300;
    report.thinking_tokens = 1_200;
    report.thinking_turns = 1;

    report.categories[4] = CategorySummary {
        key: CategoryKey::CustomTools,
        name: "Custom Tools",
        tokens: 2_500,
        bytes: 7_500,
        item_count: 1,
        in_context: true,
        items: vec![DiagnosticItem {
            kind_label: "MCP".into(),
            title: "docs / search".into(),
            subtitle: "mcp__docs__search".into(),
            tokens: 2_500,
            bytes: 7_500,
            preview: "{\"name\":\"mcp__docs__search\"}".into(),
        }],
    };
    report.categories[5] = CategorySummary {
        key: CategoryKey::UserMessages,
        name: "User / Developer Messages",
        tokens: 3_300,
        bytes: 9_900,
        item_count: 1,
        in_context: true,
        items: vec![DiagnosticItem {
            kind_label: "User".into(),
            title: "#1 User".into(),
            subtitle: "Check code".into(),
            tokens: 3_300,
            bytes: 9_900,
            preview: "Check code".into(),
        }],
    };
    report.categories[6] = CategorySummary {
        key: CategoryKey::AssistantMessages,
        name: "Assistant Messages",
        tokens: 1_500,
        bytes: 4_500,
        item_count: 1,
        in_context: true,
        items: vec![DiagnosticItem {
            kind_label: "Assistant".into(),
            title: "#2 Assistant".into(),
            subtitle: "Done".into(),
            tokens: 1_500,
            bytes: 4_500,
            preview: "Done".into(),
        }],
    };
    report.categories[7] = CategorySummary {
        key: CategoryKey::Thinking,
        name: "Assistant Thinking",
        tokens: 1_200,
        bytes: 3_600,
        item_count: 1,
        in_context: true,
        items: vec![DiagnosticItem {
            kind_label: "Thinking".into(),
            title: "#3 Reasoning (turn 1)".into(),
            subtitle: "Analyzed workspace".into(),
            tokens: 1_200,
            bytes: 3_600,
            preview: "Analyzed workspace".into(),
        }],
    };
    report.categories[8] = CategorySummary {
        key: CategoryKey::ToolCalls,
        name: "Tool Calls",
        tokens: 800,
        bytes: 2_400,
        item_count: 1,
        in_context: true,
        items: vec![DiagnosticItem {
            kind_label: "Tool Call".into(),
            title: "#4 read(src/main.rs)".into(),
            subtitle: "call_1".into(),
            tokens: 800,
            bytes: 2_400,
            preview: "read".into(),
        }],
    };
    report.categories[9] = CategorySummary {
        key: CategoryKey::ToolOutput,
        name: "Tool Output",
        tokens: 7_980,
        bytes: 23_940,
        item_count: 1,
        in_context: true,
        items: vec![DiagnosticItem {
            kind_label: "Tool Output".into(),
            title: "#5 read".into(),
            subtitle: "src/main.rs".into(),
            tokens: 7_980,
            bytes: 23_940,
            preview: "fn main() {}".into(),
        }],
    };
    report
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
fn empty_session_renders_grid_and_compact_inline_metrics() {
    let mut panel = Panel::new();
    let loading = draw_panel(&mut panel, 110, 26);
    assert!(loading.contains("Inspecting current session context..."));

    panel.set_report(sample_empty_report());
    let screen = draw_panel(&mut panel, 120, 26);
    assert!(screen.contains("Context · test/demo (medium)"));
    assert!(screen.contains("Used 7.7K of 500K tokens · 2%"));
    assert!(screen.contains("417.3K left before auto-compaction at 85%"));
    assert!(screen.contains("⛁"));
    assert!(screen.contains("⛶"));
    assert!(screen.contains("⛝"));
    let grid_only: String = screen
        .lines()
        .map(|line| line.chars().take(42).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    let total_cells = grid_only.matches('⛁').count()
        + grid_only.matches('⛶').count()
        + grid_only.matches('⛝').count();
    assert_eq!(total_cells, 200);
    assert_eq!(grid_only.matches('⛝').count(), 30);

    // 具体大小与占比紧跟在分类文字后方（2 空格间隔），绝不推到窗口右边缘对齐。
    assert!(screen.contains("> ⛁ System Prompt  2.5K  0.5%"));
    assert!(screen.contains("  ⛁ Instruction Files  53  <0.1%"));
    assert!(screen.contains("  ⛁ Skills  467  0.1%"));
    assert!(screen.contains("  ⛁ Built-in Tools  4.7K  0.9%"));
    assert!(screen.contains("  ⛁ Custom Tools  0  0.0%"));
    assert!(screen.contains("  ⛝ Auto-Compact Buffer  75K  15.0%"));
    assert!(screen.contains("  ⛶ Free Space  417.3K  83.5%"));
    let normalized: String = screen
        .replace('│', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(normalized.contains(
        "Category totals split Used by estimated share; the entries inside may not add up to them."
    ));
}

#[test]
fn categories_expand_and_collapse_in_place_and_ascii_mode_uses_ascii_glyphs() {
    let mut panel = Panel::new();
    panel.set_report(sample_report());

    // 在 System Prompt 按 Enter 原地展开 2 个子项。
    assert!(matches!(panel.handle(key(KeyCode::Enter)), Action::None));
    let expanded_screen = draw_panel(&mut panel, 120, 26);
    assert!(expanded_screen.contains("> ⛁ System Prompt ▾  2.5K  0.5%"));
    assert!(expanded_screen.contains("Base System Prompt (bundled) · bundled  1.8K  0.4%"));
    assert!(
        expanded_screen
            .contains("Runtime Tool & Environment Rules · prompts/runtime.md  700  0.1%")
    );

    // 向下进入第一个子项，按 Left 收起所属分类并将光标拉回分类行。
    assert!(matches!(panel.handle(key(KeyCode::Down)), Action::None));
    assert!(matches!(
        panel.visible_rows()[panel.cursor],
        RowKind::Child(0, 0)
    ));
    assert!(matches!(panel.handle(key(KeyCode::Left)), Action::None));
    assert_eq!(panel.visible_rows()[panel.cursor], RowKind::Category(0));
    let collapsed_screen = draw_panel(&mut panel, 110, 26);
    assert!(!collapsed_screen.contains("Base System Prompt (bundled)"));

    // 移动到 Built-in Tools (index 3) 并用 Right 展开；r 刷新；Esc 关闭。
    for _ in 0..3 {
        assert!(matches!(panel.handle(key(KeyCode::Down)), Action::None));
    }
    assert!(matches!(panel.handle(key(KeyCode::Right)), Action::None));
    let tools_screen = draw_panel(&mut panel, 110, 26);
    assert!(tools_screen.contains("read · Read files  1.2K  0.2%"));
    assert!(tools_screen.contains("bash · Run shell  3.5K  0.7%"));

    // ASCII 模式下不出现 Unicode 网格字形。
    crate::tui::themes::set_active(Theme::Default, IconMode::Ascii);
    let ascii_screen = draw_panel(&mut panel, 110, 26);
    crate::tui::themes::set_active(Theme::Default, IconMode::Nerd);
    assert!(ascii_screen.contains("# ."));
    assert!(ascii_screen.contains("x x"));
    assert!(!ascii_screen.contains('⛁'));
    assert!(!ascii_screen.contains('⛶'));
    assert!(!ascii_screen.contains('⛝'));

    // 刷新后保留未失效的展开态。
    assert!(matches!(
        panel.handle(key(KeyCode::Char('r'))),
        Action::Refresh
    ));
    panel.set_report(sample_report());
    assert!(panel.expanded[3]);
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

    let mut terminal = Terminal::new(TestBackend::new(110, 28)).unwrap();
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
    assert!(rendered.contains("Context · test/demo"));
    assert!(rendered.contains("Custom Tools"));
    assert!(rendered.contains("Assistant Thinking"));

    assert!(!app.handle(key(KeyCode::Esc), &mut dirty));
    assert!(!app.needs_alternate());
}
