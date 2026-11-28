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
fn lsp_terminal_reports_changes_and_readable_cards() {
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
    let late_entry = &transcript.entries[1];
    assert_eq!(late_entry.kind, EntryKind::Lsp);
    assert!(late_entry.tool.is_none());
    assert!(matches!(transcript.entries[0].body, EntryBody::Diff(_)));
    assert!(transcript.entries[0].diagnostics.is_empty());
    let text = screen(&render(&mut transcript, 100, 24));
    assert!(text.contains("LSP diagnostics (late)"), "{text}");
    assert!(text.contains("mismatched types"), "{text}");
    assert!(!before.contains("mismatched types"), "{before}");

    let mut inline = Transcript::new();
    tool(
        &mut inline,
        "lsp-1",
        "lsp",
        json!({"action":"diagnostics","file":"src/a.rs"}),
    );
    inline.progress(AgentEvent::LspDiagnostics {
        id: Some("lsp-1".into()),
        report: report("src/a.rs", vec![diagnostic(2, 7, "unused variable")]),
    });
    assert_eq!(inline.entries.len(), 1);
    let entry = &mut inline.entries[0];
    assert_eq!(entry.kind, EntryKind::Lsp);
    assert!(matches!(entry.body, EntryBody::Lsp(_)));
    assert_eq!(entry.diagnostics.len(), 1);
    let text = entry_text(entry, 120);
    assert!(text.contains("[warning] 7:1 unused variable"), "{text}");

    let mut multi = Transcript::new();
    tool(
        &mut multi,
        "lsp-2",
        "lsp",
        json!({"action":"rename_file","file":"src/old.rs"}),
    );
    for (path, deleted) in [("src/old.rs", true), ("src/new.rs", false)] {
        multi.progress(AgentEvent::FileChanged {
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
    multi.progress(AgentEvent::FileChanged {
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
    let EntryBody::Lsp(block) = &multi.entries[0].body else {
        panic!("lsp card body");
    };
    assert_eq!(block.changes.len(), 2);
    let text = entry_text(&mut multi.entries[0], 120);
    assert!(text.contains("Deleted · src/old.rs"), "{text}");
    assert!(text.contains("Created · src/new.rs"), "{text}");

    let mut folded = Transcript::new();
    folded.progress(late(report(
        "src/a.rs",
        (1..=18)
            .map(|line| diagnostic(3, line, &format!("hint-{line}")))
            .collect(),
    )));
    let collapsed = entry_text(&mut folded.entries[0], 120);
    assert!(collapsed.lines().count() <= 12, "{collapsed}");
    assert!(collapsed.contains("more diagnostic lines"), "{collapsed}");
    assert!(!collapsed.contains("hint-18"), "{collapsed}");
    folded.toggle_tool_output();
    let expanded = entry_text(&mut folded.entries[0], 120);
    assert!(expanded.contains("hint-18"), "{expanded}");
    assert!(
        expanded.lines().count() > collapsed.lines().count(),
        "{expanded}"
    );

    let mut hover = Transcript::new();
    tool(
        &mut hover,
        "lsp-4",
        "lsp",
        json!({"action":"hover","file":"src/a.rs","line":2,"symbol":"answer"}),
    );
    hover.progress(AgentEvent::ToolFinished {
        id: "lsp-4".into(),
        result: json!({
            "ok": true,
            "content": "fn answer() -> i32",
            "details": {"action": "hover", "serverName": "rust-analyzer"}
        }),
    });
    let text = entry_text(&mut hover.entries[0], 120);
    assert!(text.contains("LSP · hover · src/a.rs"), "{text}");
    assert!(text.contains("fn answer() -> i32"), "{text}");
    assert!(!text.contains("\"serverName\""), "{text}");
}

#[test]
fn lsp_terminal_symbols_and_replay() {
    let content = format!(
        "Symbols in src/a.rs:\n  Module activity @ line 2\n    Field nested_item Vec<{}> signature_end @ line 3\n  Struct RequestContext @ line 4\n  Function capture @ line 5\n  Function hidden_item @ line 6\n  Function last_item @ line 7",
        "类型_".repeat(80),
    );
    for width in [40, 120] {
        let mut transcript = Transcript::new();
        tool(
            &mut transcript,
            "symbols",
            "lsp",
            json!({"action":"symbols","file":"src/a.rs"}),
        );
        transcript.progress(AgentEvent::ToolFinished {
            id: "symbols".into(),
            result: json!({"ok":true,"content":content}),
        });
        let collapsed = entry_text(&mut transcript.entries[0], width);
        assert_eq!(collapsed.lines().count(), 6, "{collapsed}");
        assert!(collapsed.contains("2 more · Ctrl + O"), "{collapsed}");
        assert!(!collapsed.contains("Symbols in"), "{collapsed}");
        assert!(!collapsed.contains("hidden_item"), "{collapsed}");
        assert!(!collapsed.contains("signature_end"), "{collapsed}");
        assert!(
            collapsed
                .lines()
                .any(|line| line == "  Module activity @ line 2"),
            "{collapsed}"
        );
        assert!(
            collapsed.contains("    Field nested_item Vec<"),
            "{collapsed}"
        );

        transcript.toggle_tool_output();
        let expanded = entry_text(&mut transcript.entries[0], width);
        assert!(
            expanded
                .replace('\n', "")
                .contains(&format!("Vec<{}> signature_end", "类型_".repeat(80))),
            "{expanded}",
        );
        assert!(expanded.contains("last_item"), "{expanded}");
        assert!(!expanded.contains("Ctrl + O"), "{expanded}");
    }

    let rep = report("src/a.rs", vec![diagnostic(1, 4, "expected i32")]);
    let mut replayed = Transcript::new();
    replayed.replay(vec![
        ReplayRecord::Event(AgentEvent::ToolStarted {
            id: "lsp-3".into(),
            name: "lsp".into(),
            arguments: r#"{"action":"diagnostics","file":"src/a.rs"}"#.into(),
        }),
        ReplayRecord::Event(AgentEvent::LspDiagnostics {
            id: Some("lsp-3".into()),
            report: rep.clone(),
        }),
        ReplayRecord::Event(AgentEvent::ToolFinished {
            id: "lsp-3".into(),
            result: json!({"ok":true,"content":"1 diagnostic","diagnostics":rep}),
        }),
        ReplayRecord::Event(late(rep)),
    ]);
    assert_eq!(replayed.entries.len(), 2);
    assert_eq!(replayed.entries[0].diagnostics.len(), 1);
    assert_eq!(replayed.entries[1].kind, EntryKind::Lsp);
    let text = screen(&render(&mut replayed, 100, 24));
    assert!(text.contains("expected i32"), "{text}");
}

#[test]
fn slimming_message_snapshot_matches_live_card_order_diff_lsp_and_folded_body() {
    use pp_core::events::{DiffHunk, DiffLine, DiffLineKind};
    let diagnostic_report = report("src/a.rs", vec![diagnostic(1, 1, "expected i32")]);
    let changed = AgentEvent::FileChanged {
        id: "write".into(),
        path: "src/a.rs".into(),
        diff: FileDiff::Text {
            created: false,
            deleted: false,
            added: 1,
            removed: 1,
            hunks: vec![DiffHunk {
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Removed,
                        old_line: Some(1),
                        new_line: None,
                        text: "old".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Added,
                        old_line: None,
                        new_line: Some(1),
                        text: "new".into(),
                    },
                ],
            }],
        },
    };
    let inline = AgentEvent::LspDiagnostics {
        id: Some("write".into()),
        report: diagnostic_report.clone(),
    };
    let late = late(diagnostic_report);
    let read_result = json!({"ok":true,"path":"src/a.rs","content":(0..100).map(|i|format!("line-{i}\n")).collect::<String>()});
    let write_result = json!({"ok":true,"path":"src/a.rs"});
    let events = vec![
        AgentEvent::AssistantStarted,
        AgentEvent::ThinkingDelta {
            text: "thought".into(),
        },
        AgentEvent::AssistantDelta {
            text: "answer".into(),
            refusal: false,
        },
        AgentEvent::AssistantFinished {
            text: "answer".into(),
        },
        AgentEvent::ToolStarted {
            id: "read".into(),
            name: "read".into(),
            arguments: r#"{"path":"src/a.rs"}"#.into(),
        },
        AgentEvent::ToolFinished {
            id: "read".into(),
            result: read_result.clone(),
        },
        AgentEvent::ToolStarted {
            id: "write".into(),
            name: "write".into(),
            arguments: r#"{"path":"src/a.rs"}"#.into(),
        },
        changed.clone(),
        inline.clone(),
        AgentEvent::ToolFinished {
            id: "write".into(),
            result: write_result.clone(),
        },
        late.clone(),
    ];
    // Freeze the new wire fixture; auxiliary events are written before the assistant snapshot and merged by order on read.
    let payloads = vec![
        json!({"type":"custom","customType":"pp.event","data":{"event":"assistant_started"}}),
        json!({"type":"custom","customType":"pp.event","data":{"event":changed}}),
        json!({"type":"custom","customType":"pp.event","data":{"event":inline}}),
        json!({"type":"message","message":{"role":"assistant","stopReason":"toolUse","content":[
            {"type":"thinking","thinking":"thought","summary":false}, {"type":"text","text":"answer"},
            {"type":"toolCall","id":"read","name":"read","rawArguments":"{\"path\":\"src/a.rs\"}"},
            {"type":"toolCall","id":"write","name":"write","rawArguments":"{\"path\":\"src/a.rs\"}"}],
            "ppReplay":{"attempt":"frozen-attempt","order":[
                {"kind":"content","index":0},{"kind":"content","index":1},{"kind":"finish"},
                {"kind":"content","index":2},{"kind":"result","index":0},
                {"kind":"content","index":3},{"kind":"event","index":0},{"kind":"event","index":1},{"kind":"result","index":1}]}}}),
        json!({"type":"message","message":{"role":"toolResult","toolCallId":"read","isError":false,"content":[{"type":"text","text":read_result.to_string()}]}}),
        json!({"type":"message","message":{"role":"toolResult","toolCallId":"write","isError":false,"content":[{"type":"text","text":write_result.to_string()}]}}),
        json!({"type":"custom","customType":"pp.native_append","data":{"protocol":"response","items":[]}}),
        json!({"type":"custom","customType":"pp.event","data":{"event":late}}),
    ];
    let lines = payloads
        .into_iter()
        .enumerate()
        .map(|(i, mut p)| {
            p["id"] = format!("entry-{i}").into();
            p["parentId"] = if i == 0 {
                serde_json::Value::Null
            } else {
                format!("entry-{}", i - 1).into()
            };
            p["timestamp"] = "2026-11-12T00:00:00Z".into();
            format!("{p}\n")
        })
        .collect::<String>();
    let mut live = Transcript::new();
    for event in events {
        live.progress(event);
    }
    let mut replayed = Transcript::new();
    assert!(
        replayed
            .replay(pp_runtime::agent::persistence::replay_fixture(&lines).unwrap())
            .is_none()
    );
    assert_eq!(live.entries.len(), replayed.entries.len());
    for expanded in [false, true] {
        for (a, b) in live.entries.iter_mut().zip(&mut replayed.entries) {
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.status, b.status);
            assert_eq!(a.text, b.text);
            assert_eq!(a.arguments, b.arguments);
            assert_eq!(a.result, b.result);
            assert_eq!(a.diagnostics.len(), b.diagnostics.len());
            a.expanded = expanded;
            b.expanded = expanded;
            a.layout = None;
            b.layout = None;
            assert_eq!(entry_text(a, 100), entry_text(b, 100));
        }
    }
}
