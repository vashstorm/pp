use super::*;
use pp_core::lsp_report::DiagnosticReport;
use pp_core::lsp_report::ReportDiagnostic;
use pp_core::lsp_report::ReportStatus;
use pp_core::lsp_report::ServerReport;

fn diagnostic(severity: u8, line: u32, message: &str) -> ReportDiagnostic {
    ReportDiagnostic {
        line,
        column: 1,
        end_line: line,
        end_column: 2,
        severity,
        message: message.into(),
        code: None,
        source: Some("test".into()),
        line_only: false,
    }
}

fn report(path: &str, diagnostics: Vec<ReportDiagnostic>) -> DiagnosticReport {
    DiagnosticReport {
        path: path.into(),
        origin: "write".into(),
        owner: "root".into(),
        servers: vec![ServerReport {
            server: "rust-analyzer".into(),
            status: if diagnostics.is_empty() {
                ReportStatus::ReportedClean
            } else {
                ReportStatus::ReportedIssues
            },
            note: None,
            diagnostics,
        }],
        formatter: None,
        deduplicated: 0,
        omitted: 0,
        at_ms: 1,
    }
}

fn late(report: DiagnosticReport) -> AgentEvent {
    AgentEvent::LspDiagnostics { id: None, report }
}

#[test]
fn lsp_terminal_late_report_is_an_independent_card_without_touching_history() {
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "write-1",
        "write",
        json!({"path":"src/a.rs"}),
    );
    transcript.progress(AgentEvent::FileChanged {
        id: "write-1".into(),
        path: "src/a.rs".into(),
        diff: FileDiff::Text {
            created: true,
            deleted: false,
            added: 1,
            removed: 0,
            hunks: Vec::new(),
        },
    });
    transcript.progress(AgentEvent::ToolFinished {
        id: "write-1".into(),
        result: json!({"ok":true,"bytes_written":4}),
    });
    let before = screen(&render(&mut transcript, 100, 24));
    assert_eq!(transcript.entries.len(), 1);
    transcript.progress(late(report(
        "src/a.rs",
        vec![diagnostic(1, 2, "mismatched types")],
    )));
    assert_eq!(transcript.entries.len(), 2);
    let late = &transcript.entries[1];
    assert_eq!(late.kind, EntryKind::Lsp);
    assert!(late.tool.is_none());
    // 旧写入卡片保持冻结，迟到报告只追加新卡片。
    assert!(matches!(transcript.entries[0].body, EntryBody::Diff(_)));
    assert!(transcript.entries[0].diagnostics.is_empty());
    let text = screen(&render(&mut transcript, 100, 24));
    assert!(text.contains("LSP diagnostics (late)"), "{text}");
    assert!(text.contains("mismatched types"), "{text}");
    assert!(!before.contains("mismatched types"), "{before}");
}

#[test]
fn lsp_terminal_inline_report_attaches_to_the_tool_call_not_a_late_card() {
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "lsp-1",
        "lsp",
        json!({"action":"diagnostics","file":"src/a.rs"}),
    );
    transcript.progress(AgentEvent::LspDiagnostics {
        id: Some("lsp-1".into()),
        report: report("src/a.rs", vec![diagnostic(2, 7, "unused variable")]),
    });
    assert_eq!(transcript.entries.len(), 1);
    let entry = &mut transcript.entries[0];
    assert_eq!(entry.kind, EntryKind::Lsp);
    assert!(matches!(entry.body, EntryBody::Lsp(_)));
    assert_eq!(entry.diagnostics.len(), 1);
    let text = entry_text(entry, 120);
    assert!(text.contains("[warning] 7:1 unused variable"), "{text}");
}

#[test]
fn lsp_terminal_multi_file_change_accumulates_by_path_and_shows_deletion() {
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "lsp-2",
        "lsp",
        json!({"action":"rename_file","file":"src/old.rs"}),
    );
    for (path, deleted) in [("src/old.rs", true), ("src/new.rs", false)] {
        transcript.progress(AgentEvent::FileChanged {
            id: "lsp-2".into(),
            path: path.into(),
            diff: FileDiff::Text {
                created: !deleted,
                deleted,
                added: usize::from(!deleted),
                removed: usize::from(deleted),
                hunks: Vec::new(),
            },
        });
    }
    // 同一路径的重复通知只替换，不追加第二份。
    transcript.progress(AgentEvent::FileChanged {
        id: "lsp-2".into(),
        path: "src/new.rs".into(),
        diff: FileDiff::Text {
            created: true,
            deleted: false,
            added: 3,
            removed: 0,
            hunks: Vec::new(),
        },
    });
    let EntryBody::Lsp(block) = &transcript.entries[0].body else {
        panic!("lsp card body");
    };
    assert_eq!(block.changes.len(), 2);
    let text = entry_text(&mut transcript.entries[0], 120);
    assert!(text.contains("Deleted · src/old.rs"), "{text}");
    assert!(text.contains("Created · src/new.rs"), "{text}");
}

#[test]
fn lsp_terminal_late_report_collapses_and_expands_with_ctrl_o() {
    let mut transcript = Transcript::new();
    transcript.progress(late(report(
        "src/a.rs",
        (1..=18)
            .map(|line| diagnostic(3, line, &format!("hint-{line}")))
            .collect(),
    )));
    let collapsed = entry_text(&mut transcript.entries[0], 120);
    // 标题另有 1 行；诊断正文本身折叠为 10 行（含省略提示）。
    assert!(collapsed.lines().count() <= 12, "{collapsed}");
    assert!(collapsed.contains("more diagnostic lines"), "{collapsed}");
    assert!(!collapsed.contains("hint-18"), "{collapsed}");
    transcript.toggle_tool_output();
    let expanded = entry_text(&mut transcript.entries[0], 120);
    assert!(expanded.contains("hint-18"), "{expanded}");
    assert!(
        expanded.lines().count() > collapsed.lines().count(),
        "{expanded}"
    );
}

#[test]
fn lsp_terminal_tool_content_is_readable_not_json() {
    let mut transcript = Transcript::new();
    tool(
        &mut transcript,
        "lsp-4",
        "lsp",
        json!({"action":"hover","file":"src/a.rs","line":2,"symbol":"answer"}),
    );
    transcript.progress(AgentEvent::ToolFinished {
        id: "lsp-4".into(),
        result: json!({
            "ok": true,
            "content": "fn answer() -> i32",
            "details": {"action": "hover", "serverName": "rust-analyzer"}
        }),
    });
    let text = entry_text(&mut transcript.entries[0], 120);
    assert!(text.contains("LSP · hover · src/a.rs"), "{text}");
    assert!(text.contains("fn answer() -> i32"), "{text}");
    assert!(!text.contains("\"serverName\""), "{text}");
}

#[test]
fn lsp_terminal_replay_rebuilds_frozen_reports_without_new_queries() {
    let report = report("src/a.rs", vec![diagnostic(1, 4, "expected i32")]);
    let mut transcript = Transcript::new();
    transcript.replay(vec![
        ReplayRecord::Event(AgentEvent::ToolStarted {
            id: "lsp-3".into(),
            name: "lsp".into(),
            arguments: r#"{"action":"diagnostics","file":"src/a.rs"}"#.into(),
        }),
        ReplayRecord::Event(AgentEvent::LspDiagnostics {
            id: Some("lsp-3".into()),
            report: report.clone(),
        }),
        ReplayRecord::Event(AgentEvent::ToolFinished {
            id: "lsp-3".into(),
            result: json!({"ok":true,"content":"1 diagnostic","diagnostics":report}),
        }),
        ReplayRecord::Event(late(report)),
    ]);
    assert_eq!(transcript.entries.len(), 2);
    assert_eq!(transcript.entries[0].diagnostics.len(), 1);
    assert_eq!(transcript.entries[1].kind, EntryKind::Lsp);
    let text = screen(&render(&mut transcript, 100, 24));
    assert!(text.contains("expected i32"), "{text}");
}
