use super::*;
use pp_core::lsp_report::ReportDiagnostic;
use pp_core::lsp_report::ServerReport;

fn report(message: Option<&str>) -> DiagnosticReport {
    DiagnosticReport {
        path: "src/lib.rs".into(),
        servers: vec![ServerReport {
            server: "fixture".into(),
            status: if message.is_some() {
                ReportStatus::ReportedIssues
            } else {
                ReportStatus::ReportedClean
            },
            diagnostics: message
                .into_iter()
                .map(|message| ReportDiagnostic {
                    line: 1,
                    column: 1,
                    end_line: 1,
                    end_column: 2,
                    severity: 1,
                    message: message.into(),
                    code: None,
                    source: None,
                    line_only: false,
                })
                .collect(),
            note: None,
        }],
        ..Default::default()
    }
}

#[test]
fn lsp_writethrough_dedup_preserves_issues_and_recurs_after_resolution() {
    let owner = Owner::new();
    let mut first = report(Some("type mismatch"));
    owner.deduplicate(&mut first, true);
    assert_eq!(first.total(), 1);
    let mut repeated = report(Some("type mismatch"));
    owner.deduplicate(&mut repeated, true);
    assert!(repeated.has_issues());
    assert!(!repeated.all_clean());
    assert_eq!(repeated.total(), 1);
    assert_eq!(repeated.deduplicated, 1);
    assert!(repeated.servers[0].diagnostics.is_empty());
    owner.deduplicate(&mut report(None), true);
    let mut recurring = report(Some("type mismatch"));
    owner.deduplicate(&mut recurring, true);
    assert_eq!(recurring.servers[0].diagnostics[0].message, "type mismatch");
    assert_eq!(recurring.deduplicated, 0);
}

#[test]
fn lsp_writethrough_owners_and_session_ledgers_are_independent() {
    let parent = Owner::new();
    let child = Owner::new();
    parent.deduplicate(&mut report(Some("bad type")), true);
    let mut child_report = report(Some("bad type"));
    child.deduplicate(&mut child_report, true);
    assert_eq!(child_report.deduplicated, 0);
    parent.reset();
    let mut next_session = report(Some("bad type"));
    parent.deduplicate(&mut next_session, true);
    assert_eq!(next_session.deduplicated, 0);
}

#[test]
fn lsp_writethrough_new_mutation_cancels_old_before_operation_wait() {
    let coordinator = Coordinator::default();
    let first = coordinator.mutate(Path::new("/workspace/file.rs"));
    let other = coordinator.mutate(Path::new("/workspace/other.rs"));
    let next = coordinator.mutate(Path::new("/workspace/file.rs"));
    assert!(first.cancellation.is_cancelled());
    assert!(!coordinator.current(&first));
    assert!(coordinator.current(&other));
    assert!(coordinator.current(&next));
    coordinator.stop().unwrap();
    assert!(next.cancellation.is_cancelled());
    assert!(other.cancellation.is_cancelled());
    assert!(!coordinator.current(&next));
}

#[test]
fn lsp_writethrough_unverified_does_not_resolve_previous_problem() {
    let owner = Owner::new();
    owner.deduplicate(&mut report(Some("type mismatch")), true);
    let mut unverified = report(None);
    unverified.servers[0].status = ReportStatus::Unreported;
    owner.deduplicate(&mut unverified, true);
    let mut repeated = report(Some("type mismatch"));
    owner.deduplicate(&mut repeated, true);
    assert_eq!(repeated.deduplicated, 1);
    assert!(repeated.has_issues());
}

#[test]
fn lsp_writethrough_position_changes_do_not_repeat_message() {
    let owner = Owner::new();
    owner.deduplicate(&mut report(Some("src/lib.rs:1:2: type mismatch")), true);
    let mut moved = report(Some("src/lib.rs:10:3: type mismatch"));
    owner.deduplicate(&mut moved, true);
    assert_eq!(moved.deduplicated, 1);
    assert_eq!(moved.total(), 1);
    assert!(moved.has_issues());
}

#[test]
fn lsp_writethrough_display_and_model_ack_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(
        dir.path().to_path_buf(),
        None,
        super::super::LspSettings::default(),
    );
    let owner = Owner::new();
    let ticket = registry.diagnostics().mutate(&dir.path().join("file.rs"));
    owner.inner.state.lock().unwrap().inbox.push_back(Delivery {
        id: 1,
        report: report(Some("bad type")),
        run: Some("run-1".into()),
        presented: false,
        model_consumed: false,
        ticket,
        generation: 0,
        versions: Vec::new(),
    });
    owner.presented(1);
    let shown = owner.pending(&registry).unwrap();
    assert_eq!(
        shown[0].report.servers[0].diagnostics[0].message,
        "bad type"
    );
    assert!(shown[0].presented);
    assert!(!shown[0].model_consumed);
    assert_eq!(shown[0].run.as_deref(), Some("run-1"));
    owner.consumed(&[1]);
    assert!(owner.pending(&registry).unwrap().is_empty());
    registry.stop_all().unwrap();
}

#[test]
fn lsp_writethrough_stale_inbox_does_not_leak_to_next_mutation_or_session() {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(
        dir.path().to_path_buf(),
        None,
        super::super::LspSettings::default(),
    );
    let owner = Owner::new();
    let path = dir.path().join("file.rs");
    for id in 1..=2 {
        let ticket = registry.diagnostics().mutate(&path);
        owner.inner.state.lock().unwrap().inbox.push_back(Delivery {
            id,
            report: report(Some("old problem")),
            run: None,
            presented: false,
            model_consumed: false,
            ticket,
            generation: 0,
            versions: Vec::new(),
        });
        if id == 1 {
            registry.diagnostics().mutate(&path);
        } else {
            owner.reset();
        }
        assert!(owner.pending(&registry).unwrap().is_empty());
    }
    registry.stop_all().unwrap();
}
