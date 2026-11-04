use super::*;

fn report(path: &str, message: &str) -> pp_core::lsp_report::DiagnosticReport {
    pp_core::lsp_report::DiagnosticReport {
        path: path.into(),
        origin: "write".into(),
        owner: "root".into(),
        servers: vec![pp_core::lsp_report::ServerReport {
            server: "rust-analyzer".into(),
            status: pp_core::lsp_report::ReportStatus::ReportedIssues,
            note: None,
            diagnostics: vec![pp_core::lsp_report::ReportDiagnostic {
                line: 3,
                column: 2,
                end_line: 3,
                end_column: 4,
                severity: 1,
                message: message.into(),
                code: None,
                source: None,
                line_only: false,
            }],
        }],
        formatter: None,
        deduplicated: 0,
        omitted: 0,
        at_ms: 1,
    }
}

#[test]
fn lsp_late_diagnostics_append_a_card_without_starting_a_run() {
    let mut app = app();
    app.state = State::Ready;
    app.receive_event_for_test(WorkerEvent::Progress(AgentEvent::LspDiagnostics {
        id: None,
        report: report("src/a.rs", "expected i32"),
    }));
    assert!(matches!(app.state, State::Ready));
    assert!(app.active_cancellation.is_none());
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("LSP diagnostics (late)"), "{screen}");
    assert!(screen.contains("expected i32"), "{screen}");
}

#[test]
fn lsp_warmup_failure_is_visible_but_keeps_the_app_ready() {
    let mut app = app();
    app.state = State::Ready;
    app.receive_event_for_test(WorkerEvent::LspWarmupFailed {
        server: "rust-analyzer".into(),
        error: "initialization failed".into(),
    });
    assert!(matches!(app.state, State::Ready));
    let screen = draw(&mut app, 100, 30);
    assert!(screen.contains("LSP rust-analyzer unavailable"), "{screen}");
}
