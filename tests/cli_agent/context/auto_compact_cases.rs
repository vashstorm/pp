use super::*;

#[test]
fn context_auto_compact_summarizes_old_prefix_inside_task_both_protocols() {
    let protocol = Protocol::Responses;
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    super::super::system_prompt::write(
        dir.path(),
        ".pp/SYSTEM_TEMPLATE.md",
        super::super::system_prompt::LIVE_TEMPLATE,
    );
    context_auto_write_growth_files(dir.path());
    let mut cli = protocol.start(
        &server,
        dir.path(),
        &[("primary", Some(32768))],
        r#"{"default_model":"fixture/primary"}"#,
    );
    context_auto_grow_two_batches(protocol, &server, &mut cli, true);
    let (request, body) = protocol.receive(&server);
    context_assert_auto_summary_request(
        protocol,
        &body,
        &[AUTO_OLD_MARKER],
        &[AUTO_RECENT_MARKER, AUTO_REASONING, AUTO_ENCRYPTED],
    );
    super::super::system_prompt::assert_live(&body, protocol.name(), "execute");
    cli.wait_for(Pipe::Stderr, "Compacting context...");
    protocol.final_reply(request, AUTO_SUMMARY);
    cli.wait_for(Pipe::Stderr, "Context compacted: ~");

    let (request, body) = protocol.receive(&server);
    context_assert_auto_compacted_batch(
        protocol,
        &body,
        Some((AUTO_REASONING, AUTO_ENCRYPTED)),
        None,
    );
    super::super::system_prompt::assert_live(&body, protocol.name(), "execute");
    protocol.final_reply(request, "growth done");
    no_more(&server);
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{protocol:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"growth done\n", "{protocol:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Compacting context..."), "{stderr}");
    assert!(stderr.contains("Context compacted: ~"), "{stderr}");
    // The internal summary never reaches stdout or the visible transcript.
    assert!(!stderr.contains(AUTO_SUMMARY), "{stderr}");
    // Each read ran exactly once; compaction never replayed the old batch.
    assert_eq!(stderr.matches("tool read: running").count(), 2, "{stderr}");
    let statuses = context_statuses(&stderr);
    assert_eq!(statuses.len(), 1, "{stderr}");
    assert!(statuses[0].contains(" / 32768 tokens ("), "{}", statuses[0]);
    assert!(
        statuses[0].contains("Auto compact: on at 80%"),
        "{}",
        statuses[0]
    );
    let statistics = super::super::stats::database(dir.path());
    let summaries: i64 = statistics
        .query_row(
            "SELECT COUNT(*) FROM requests WHERE purpose='auto_compact'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(summaries, 1);
    // 摘要请求不是会话首次请求，也不再保存正文与会话条目。
    let snapshots: i64 = statistics
            .query_row(
                "SELECT COUNT(*) FROM requests r JOIN request_contexts c ON c.request_id=r.id WHERE r.purpose='auto_compact'",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(snapshots, 0);
}
