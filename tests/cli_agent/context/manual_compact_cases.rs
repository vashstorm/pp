use super::*;

#[test]
fn context_manual_compact_summarizes_old_prefix_and_keeps_native_suffix() {
    let protocol = Protocol::Responses;
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    super::super::system_prompt::write(
        dir.path(),
        ".pp/SYSTEM_TEMPLATE.md",
        super::super::system_prompt::LIVE_TEMPLATE,
    );
    context_write_large_file(dir.path());
    let mut cli = protocol.start(
        &server,
        dir.path(),
        &[("primary", Some(8192))],
        MANUAL_SETTINGS,
    );
    context_seed_history(protocol, &server, &mut cli);

    cli.input("/compact\n");
    let (request, body) = protocol.receive(&server);
    context_assert_summary_request(protocol, &body);
    super::super::system_prompt::assert_live(&body, protocol.name(), "execute");
    cli.wait_for(Pipe::Stderr, "Compacting context...");
    protocol.final_reply(request, SUMMARY_TEXT);
    cli.wait_for(Pipe::Stderr, "Context compacted: ~");
    // Compaction itself sends exactly one request.
    no_more(&server);

    cli.input("next task\n");
    let (request, body) = protocol.receive(&server);
    context_assert_compacted_history(protocol, &body, "next task");
    super::super::system_prompt::assert_live(&body, protocol.name(), "execute");
    protocol.final_reply(request, "next done");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // The summary output is internal: stdout contains only task answers.
    assert_eq!(output.stdout, b"alpha noted\nrecent done\nnext done\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let (before, after) = context_compacted_tokens(&stderr);
    assert!(after < before, "{stderr}");
    assert!(!stderr.contains(SUMMARY_TEXT), "{stderr}");
    // `/compact` prints its own final context status: the retained
    // context, the reset last-request usage, and the configured policy.
    let statuses = context_statuses(&stderr);
    assert_eq!(statuses.len(), 4, "{stderr}");
    let compaction_status = statuses[2];
    assert!(
        compaction_status.contains(" / 8192 tokens ("),
        "{compaction_status}"
    );
    assert!(
        compaction_status.contains("Last request: unavailable"),
        "{compaction_status}"
    );
    assert!(
        compaction_status.contains("Auto compact: off"),
        "{compaction_status}"
    );
    // The old tool batch ran once and was never replayed.
    assert_eq!(stderr.matches("tool read: running").count(), 1, "{stderr}");
    no_more(&server);
    let statistics = super::super::stats::database(dir.path());
    let summaries: i64 = statistics
        .query_row(
            "SELECT COUNT(*) FROM requests WHERE purpose='compact'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(summaries, 1);
    // A summary request is not the session's first request, and it also stores no body or session entry.
    let snapshots: i64 = statistics
            .query_row(
                "SELECT COUNT(*) FROM requests r JOIN request_contexts c ON c.request_id=r.id WHERE r.purpose='compact'",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(snapshots, 0);
}
