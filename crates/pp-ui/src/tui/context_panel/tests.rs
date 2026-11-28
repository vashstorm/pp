use super::*;
use crossterm::event::KeyModifiers;
use pp_config::config::{IconMode, Theme};
use pp_core::{context::TokenUsage, model::ThinkLevel};
use pp_runtime::agent::context_diagnosis::{
    CategoryKey, CategorySummary, ContextReport, DiagnosticItem,
};
use ratatui::{Terminal, backend::TestBackend};
use std::sync::Arc;

fn arc_item(
    kind_label: &str,
    title: &str,
    subtitle: &str,
    tokens: u64,
    bytes: u64,
    preview: &str,
) -> Arc<DiagnosticItem> {
    Arc::new(DiagnosticItem {
        kind_label: kind_label.into(),
        title: title.into(),
        subtitle: subtitle.into(),
        tokens,
        bytes,
        preview: preview.into(),
    })
}

fn sample_empty_report() -> ContextReport {
    ContextReport {
        model_reference: "test/demo".into(),
        model_name: "demo-model".into(),
        protocol: "response",
        work_mode: "execute",
        think_level: ThinkLevel::Medium,
        empty_session: true,
        active_tokens: 0,
        context_window: Some(500_000),
        auto_compact: true,
        threshold_percent: 85,
        last_request: None,
        session_output_tokens: 0,
        thinking_tokens: 0,
        thinking_in_context: true,
        thinking_turns: 0,
        categories: CategoryKey::ALL
            .into_iter()
            .map(|key| CategorySummary {
                key,
                name: key.label(),
                tokens: 0,
                bytes: 0,
                item_count: 0,
                in_context: true,
                items: Vec::new(),
            })
            .collect(),
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
        reasoning_tokens: Some(1_200),
    });
    report.session_output_tokens = 2_300;
    report.thinking_tokens = 1_200;
    report.thinking_turns = 1;

    report.categories[0] = CategorySummary {
        key: CategoryKey::System,
        name: "System Prompt",
        tokens: 2_500,
        bytes: 7_500,
        item_count: 2,
        in_context: true,
        items: vec![
            arc_item(
                "Base",
                "Base System Prompt (bundled)",
                "bundled",
                1_800,
                5_400,
                "Base body",
            ),
            arc_item(
                "Runtime",
                "Runtime Tool & Environment Rules",
                "prompts/runtime.md",
                700,
                2_100,
                "Runtime rules",
            ),
        ],
    };
    report.categories[1] = CategorySummary {
        key: CategoryKey::InstructionFiles,
        name: "Instruction Files",
        tokens: 53,
        bytes: 159,
        item_count: 1,
        in_context: true,
        items: vec![arc_item(
            "AGENTS.md",
            "AGENTS.md",
            "sent in session",
            53,
            159,
            "Keep tests fast.",
        )],
    };
    report.categories[2] = CategorySummary {
        key: CategoryKey::Skills,
        name: "Skills",
        tokens: 467,
        bytes: 1_401,
        item_count: 1,
        in_context: true,
        items: vec![arc_item(
            "Skill",
            "rust",
            "skill://rust",
            467,
            1_401,
            "Rust skill",
        )],
    };
    report.categories[3] = CategorySummary {
        key: CategoryKey::BuiltInTools,
        name: "Built-in Tools",
        tokens: 4_700,
        bytes: 14_100,
        item_count: 2,
        in_context: true,
        items: vec![
            arc_item(
                "Built-in",
                "read",
                "Read files",
                1_200,
                3_600,
                "{\"name\":\"read\"}",
            ),
            arc_item(
                "Built-in",
                "bash",
                "Run shell",
                3_500,
                10_500,
                "{\"name\":\"bash\"}",
            ),
        ],
    };
    report.categories[4] = CategorySummary {
        key: CategoryKey::CustomTools,
        name: "Custom Tools",
        tokens: 2_500,
        bytes: 7_500,
        item_count: 1,
        in_context: true,
        items: vec![arc_item(
            "MCP",
            "docs / search",
            "mcp__docs__search",
            2_500,
            7_500,
            "{\"name\":\"mcp__docs__search\"}",
        )],
    };
    report.categories[5] = CategorySummary {
        key: CategoryKey::UserMessages,
        name: "User / Developer Messages",
        tokens: 3_300,
        bytes: 9_900,
        item_count: 1,
        in_context: true,
        items: vec![arc_item(
            "User",
            "#1 User",
            "Check code",
            3_300,
            9_900,
            "Check code",
        )],
    };
    report.categories[6] = CategorySummary {
        key: CategoryKey::AssistantMessages,
        name: "Assistant Messages",
        tokens: 1_500,
        bytes: 4_500,
        item_count: 1,
        in_context: true,
        items: vec![arc_item(
            "Assistant",
            "#2 Assistant",
            "Done",
            1_500,
            4_500,
            "Done",
        )],
    };
    report.categories[7] = CategorySummary {
        key: CategoryKey::Thinking,
        name: "Assistant Thinking",
        tokens: 1_200,
        bytes: 3_600,
        item_count: 1,
        in_context: true,
        items: vec![arc_item(
            "Thinking",
            "#3 Reasoning (turn 1)",
            "Analyzed workspace",
            1_200,
            3_600,
            "Analyzed workspace",
        )],
    };
    report.categories[8] = CategorySummary {
        key: CategoryKey::ToolCalls,
        name: "Tool Calls",
        tokens: 800,
        bytes: 2_400,
        item_count: 1,
        in_context: true,
        items: vec![arc_item(
            "Tool Call",
            "#4 read(src/main.rs)",
            "call_1",
            800,
            2_400,
            "read",
        )],
    };
    report.categories[9] = CategorySummary {
        key: CategoryKey::ToolOutput,
        name: "Tool Output",
        tokens: 7_980,
        bytes: 23_940,
        item_count: 1,
        in_context: true,
        items: vec![arc_item(
            "Tool Output",
            "#5 read",
            "src/main.rs",
            7_980,
            23_940,
            "fn main() {}",
        )],
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
fn context_panel_grid_inline_metrics_ascii_and_expand_collapse() {
    {
        let mut panel = Panel::new();
        let loading = draw_panel(&mut panel, 110, 26);
        assert!(loading.contains("Inspecting current session context..."));

        panel.set_report(sample_empty_report());
        let screen = draw_panel(&mut panel, 120, 26);
        assert!(screen.contains("Context · test/demo (medium)"));
        assert!(screen.contains("Used 0 of 500K tokens · 0%"));
        assert!(screen.contains("425K left before auto-compaction at 85%"));
        assert!(screen.contains("⛶"));
        assert!(screen.contains("⛝"));
        let grid_only: String = screen
            .lines()
            .map(|line| line.chars().take(52).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(grid_only.matches('⛁').count(), 0);
        assert_eq!(grid_only.matches('⛝').count(), 60);
        assert_eq!(grid_only.matches('⛶').count(), 340);

        // Size and share follow the category text immediately (two spaces apart) and are never pushed to the right edge; an empty session shows 0 throughout.
        assert!(screen.contains("> ⛁ System Prompt  0  0.0%"));
        assert!(screen.contains("  ⛁ Instruction Files  0  0.0%"));
        assert!(screen.contains("  ⛁ Skills  0  0.0%"));
        assert!(screen.contains("  ⛁ Built-in Tools  0  0.0%"));
        assert!(screen.contains("  ⛁ Custom Tools  0  0.0%"));
        assert!(screen.contains("  ⛝ Auto-Compact Buffer  75K  15.0%"));
        assert!(screen.contains("  ⛶ Free Space  425K  85.0%"));
        assert!(matches!(panel.handle(key(KeyCode::Enter)), Action::None));
        assert!(!panel.expanded[0]);
        let right_normalized: String = screen
            .lines()
            .map(|line| line.chars().skip(53).collect::<String>())
            .collect::<Vec<_>>()
            .join(" ")
            .replace('│', " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(right_normalized.contains(
            "Category totals split Used by estimated share; the entries inside may not add up to them."
        ));
    }

    {
        let mut panel = Panel::new();
        panel.set_report(sample_report());

        let header_screen = draw_panel(&mut panel, 120, 26);
        assert!(
            header_screen.contains("400K left before auto-compaction at 85%"),
            "{header_screen}"
        );
        assert!(header_screen.contains("Cache 50%"), "{header_screen}");
        assert!(header_screen.contains("Reasoning 1.2K"), "{header_screen}");

        // Pressing Enter on System Prompt expands its 2 children in place.
        assert!(matches!(panel.handle(key(KeyCode::Enter)), Action::None));
        let expanded_screen = draw_panel(&mut panel, 130, 26);
        assert!(expanded_screen.contains("> ⛁ System Prompt ▾  2.5K  0.5%"));
        assert!(expanded_screen.contains("Base System Prompt (bundled) · bundled  1.8K  0.4%"));
        assert!(
            expanded_screen
                .contains("Runtime Tool & Environment Rules · prompts/runtime.md  700  0.1%")
        );

        // Move down into the first child, then Left collapses the parent category and pulls the cursor back to its row.
        assert!(matches!(panel.handle(key(KeyCode::Down)), Action::None));
        assert!(matches!(
            panel.visible_rows()[panel.cursor],
            RowKind::Child(0, 0)
        ));
        assert!(matches!(panel.handle(key(KeyCode::Left)), Action::None));
        assert_eq!(panel.visible_rows()[panel.cursor], RowKind::Category(0));
        let collapsed_screen = draw_panel(&mut panel, 110, 26);
        assert!(!collapsed_screen.contains("Base System Prompt (bundled)"));

        // Move to Built-in Tools (index 3) and expand with Right; r refreshes; Esc closes.
        for _ in 0..3 {
            assert!(matches!(panel.handle(key(KeyCode::Down)), Action::None));
        }
        assert!(matches!(panel.handle(key(KeyCode::Right)), Action::None));
        let tools_screen = draw_panel(&mut panel, 110, 26);
        assert!(tools_screen.contains("Read · Read files  1.2K  0.2%"));
        assert!(tools_screen.contains("Bash · Run shell  3.5K  0.7%"));

        // ASCII mode renders no Unicode grid glyphs.
        crate::tui::themes::set_active(Theme::Default, IconMode::Ascii);
        let ascii_screen = draw_panel(&mut panel, 110, 26);
        crate::tui::themes::set_active(Theme::Default, IconMode::Nerd);
        assert!(ascii_screen.contains("# ."));
        assert!(ascii_screen.contains("x x"));
        assert!(!ascii_screen.contains('⛁'));
        assert!(!ascii_screen.contains('⛶'));
        assert!(!ascii_screen.contains('⛝'));

        // A refresh keeps the expansion state that is still valid.
        assert!(matches!(
            panel.handle(key(KeyCode::Char('r'))),
            Action::Refresh
        ));
        panel.set_report(sample_report());
        assert!(panel.expanded[3]);
        assert!(matches!(panel.handle(key(KeyCode::Esc)), Action::Close));
    }
}

/// A category can hold many entries; expanding it in place lists every one of them, keeps the selected row visible
/// while scrolling, and renders the labels of the entries the report shares with the runtime classification.
#[test]
fn context_panel_expands_category_with_many_shared_items() {
    let mut report = sample_empty_report();
    report.empty_session = false;
    report.active_tokens = 60_000;
    let items: Vec<_> = (0..40)
        .map(|index| {
            arc_item(
                "Tool Output",
                &format!("#{index} read"),
                &format!("file_{index}.txt"),
                1_500,
                4_500,
                &format!("output body {index}"),
            )
        })
        .collect();
    report.categories[9] = CategorySummary {
        key: CategoryKey::ToolOutput,
        name: "Tool Output",
        tokens: 60_000,
        bytes: 180_000,
        item_count: items.len(),
        in_context: true,
        items,
    };

    let mut panel = Panel::new();
    panel.set_report(report);
    for _ in 0..9 {
        assert!(matches!(panel.handle(key(KeyCode::Down)), Action::None));
    }
    assert_eq!(panel.visible_rows()[panel.cursor], RowKind::Category(9));

    assert!(matches!(panel.handle(key(KeyCode::Enter)), Action::None));
    let rows = panel.visible_rows();
    assert_eq!(rows.len(), 10 + 40 + 2);
    assert_eq!(
        rows.iter()
            .filter(|row| matches!(row, RowKind::Child(9, _)))
            .count(),
        40
    );

    // The last entry is below the visible list, so the view scrolls with the cursor and keeps it on screen.
    assert!(matches!(panel.handle(key(KeyCode::End)), Action::None));
    let rows = panel.visible_rows();
    assert_eq!(rows[51], RowKind::FreeSpace);
    assert_eq!(rows[50], RowKind::AutoCompactBuffer);
    assert_eq!(rows[49], RowKind::Child(9, 39));
    assert!(matches!(panel.handle(key(KeyCode::Up)), Action::None));
    assert!(matches!(panel.handle(key(KeyCode::Up)), Action::None));
    assert_eq!(panel.visible_rows()[panel.cursor], RowKind::Child(9, 39));
    let scrolled = draw_panel(&mut panel, 120, 26);
    assert!(scrolled.contains("#39 read · file_39.txt"), "{scrolled}");
    assert!(!scrolled.contains("#0 read"), "{scrolled}");

    // Home returns to the first category row; the expanded entries are scrolled out again.
    assert!(matches!(panel.handle(key(KeyCode::Home)), Action::None));
    let top = draw_panel(&mut panel, 120, 26);
    assert!(top.contains("> ⛁ System Prompt"), "{top}");
    assert!(!top.contains("#39 read"), "{top}");
}

#[test]
fn app_context_command_states_and_compaction_policy_sync() {
    {
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

            // A late diagnostics reply must not overwrite the main task state.
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

    {
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

    {
        let dir = tempfile::tempdir().unwrap();
        let settings_path = dir.path().join("settings.yml");
        std::fs::write(&settings_path, "auto_compact_threshold_percent: 80\n").unwrap();

        let mut app = crate::tui::App::new(Err(anyhow::anyhow!("no cfg"))).unwrap();
        let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
        app.commands = Some(cmd_tx);
        app.settings_path = Some(settings_path.clone());
        app.usage = Some(pp_core::context::ContextUsage {
            estimated_tokens: 10_000,
            context_window: Some(100_000),
            last_request: None,
            session_output_tokens: 0,
            auto_compact: true,
            threshold_percent: 80,
        });
        let mut panel = Panel::new();
        panel.set_report(sample_empty_report());
        app.context_panel = Some(panel);

        app.settings_worker
            .save(
                &settings_path,
                pp_config::config::SettingPatch::AutoCompactThresholdPercent(90),
            )
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !app.receive_settings() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert!(matches!(
            cmd_rx.try_recv(),
            Ok(crate::tui::WorkerCommand::UpdateCompactionPolicy {
                auto_compact: true,
                threshold_percent: 90,
            })
        ));
        let usage = app.usage.unwrap();
        assert!(usage.auto_compact);
        assert_eq!(usage.threshold_percent, 90);

        let mut terminal = Terminal::new(TestBackend::new(120, 26)).unwrap();
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
        assert!(rendered.contains("Auto-Compact Buffer  50K  10.0%"));
        assert!(rendered.contains("Free Space  450K  90.0%"));
    }
}
