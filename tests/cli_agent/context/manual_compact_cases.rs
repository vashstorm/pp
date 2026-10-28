use super::*;

#[test]
fn context_manual_compact_summarizes_old_prefix_and_keeps_native_suffix() {
    for protocol in PROTOCOLS {
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
        let entries: i64 = statistics.query_row("SELECT COUNT(*) FROM requests r JOIN request_payloads p ON r.id=p.request_id WHERE r.purpose='compact' AND p.session_entry_json IS NOT NULL", [], |r|r.get(0)).unwrap();
        assert_eq!(entries, 0);
    }
}

#[test]
fn context_manual_compact_noops_without_requests_for_empty_and_single_exchange() {
    // Empty session: nothing to compact, no model request, no journal.
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Protocol::Chat.start(
        &server,
        dir.path(),
        &[("primary", Some(32768))],
        MANUAL_SETTINGS,
    );
    cli.input("/compact\n/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Nothing to compact."), "{stderr}");
    assert!(!stderr.contains("Compacting context..."), "{stderr}");
    assert!(
        !dir.path().join(".pp/agent/sessions").exists(),
        "empty /compact must not create a journal"
    );
    no_more(&server);

    // One complete exchange: there is no removable old exchange.
    for protocol in PROTOCOLS {
        let server = super::super::server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start(
            &server,
            dir.path(),
            &[("primary", Some(32768))],
            MANUAL_SETTINGS,
        );
        cli.input("only task\n");
        let (request, _) = protocol.receive(&server);
        protocol.final_reply(request, "only done");
        cli.wait_for(Pipe::Stdout, "only done");
        cli.input("/compact\n/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"only done\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Nothing to compact."), "{stderr}");
        assert!(!stderr.contains("Compacting context..."), "{stderr}");
        no_more(&server);
    }
}

#[test]
fn context_manual_compact_rejects_arguments_and_p_prompt_is_literal() {
    // Interactive `/compact` rejects arguments before any model request.
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Protocol::Chat.start(
        &server,
        dir.path(),
        &[("primary", Some(32768))],
        MANUAL_SETTINGS,
    );
    cli.input("/compact now\n/help\n/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("usage: /compact"), "{stderr}");
    assert!(
        stderr.contains("/compact — Summarize older context"),
        "{stderr}"
    );
    assert!(!stderr.contains("Compacting context..."), "{stderr}");
    no_more(&server);

    // A `-p` prompt is never parsed as a slash command.
    let server = super::super::server();
    let dir = tempfile::tempdir().unwrap();
    let models = Protocol::Chat.models(&server, &[("primary", None)]);
    let cli = Cli::start_with(
        dir.path(),
        Some("/compact"),
        r#"{"default_model":"fixture/primary"}"#,
        &models,
    );
    let cli = LiveCli::new(cli);
    let (request, body) = Protocol::Chat.receive(&server);
    slash_assert_message(&body["messages"][1], "user", "/compact");
    Protocol::Chat.final_reply(request, "literal handled");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"literal handled\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("Compacting context..."), "{stderr}");
    assert!(!stderr.contains("usage: /compact"), "{stderr}");
    no_more(&server);
}

#[test]
fn context_manual_compact_failure_matrix_preserves_history_and_never_runs_tools() {
    for protocol in PROTOCOLS {
        for failure in MANUAL_FAILURES {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
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
            failure.reply(protocol, request);
            cli.wait_for(Pipe::Stderr, failure.expected());
            // No hidden follow-up request (for example a summary tool call).
            no_more(&server);

            cli.input("after failures\n");
            let (request, body) = protocol.receive(&server);
            context_assert_uncompacted_history(protocol, &body, "after failures");
            protocol.final_reply(request, "still intact");
            cli.input("/quit\n");
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{failure:?} {protocol:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                output.stdout, b"alpha noted\nrecent done\nstill intact\n",
                "{failure:?} {protocol:?}"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(
                stderr.matches("tool read: running").count(),
                1,
                "{failure:?} {protocol:?}: {stderr}"
            );
            assert!(
                !stderr.contains("SUMMARY-LONG"),
                "{failure:?} {protocol:?}: {stderr}"
            );
            no_more(&server);
        }
    }
}

#[test]
fn context_manual_compact_rejects_summary_request_beyond_window_without_network() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        context_write_large_file(dir.path());
        let mut cli = protocol.start(
            &server,
            dir.path(),
            &[("primary", Some(512))],
            MANUAL_SETTINGS,
        );
        context_seed_history(protocol, &server, &mut cli);

        cli.input("/compact\n");
        cli.wait_for(
            Pipe::Stderr,
            "cannot compact: summary request exceeds context window",
        );
        // The oversized summary request is rejected locally.
        no_more(&server);

        cli.input("after window\n");
        let (request, body) = protocol.receive(&server);
        context_assert_uncompacted_history(protocol, &body, "after window");
        protocol.final_reply(request, "still intact");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"alpha noted\nrecent done\nstill intact\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(stderr.matches("tool read: running").count(), 1, "{stderr}");
        no_more(&server);
    }
}

#[test]
fn context_manual_compact_persists_before_exit_and_resumes_native_cursor() {
    for protocol in PROTOCOLS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        context_write_large_file(home.path());
        let mut cli = protocol.start(
            &server,
            home.path(),
            &[("primary", Some(8192))],
            MANUAL_SETTINGS,
        );
        context_seed_history(protocol, &server, &mut cli);

        cli.input("/compact\n");
        let (request, body) = protocol.receive(&server);
        context_assert_summary_request(protocol, &body);
        protocol.final_reply(request, SUMMARY_TEXT);
        cli.wait_for(Pipe::Stderr, "Context compacted: ~");
        no_more(&server);
        // Exit immediately after the success event, without another task.
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        let id = context_session_id(&stderr);
        let path = context_session_path(home.path(), &id);
        let compacted = std::fs::read(&path).unwrap();
        let compacted_log = String::from_utf8_lossy(&compacted);
        // The compacted state was committed before exit, and old records stay.
        assert!(
            compacted_log.contains("[Context summary]"),
            "{compacted_log}"
        );
        assert!(compacted_log.contains(OLD_USER), "{compacted_log}");
        assert!(compacted_log.contains(OLD_MARKER), "{compacted_log}");
        assert_eq!(context_session_count(home.path()), 1);

        // First new task after resume already uses the summary and suffix.
        let home_str = home.path().to_str().unwrap();
        let args = ["--resume", id.as_str(), "-p", "resumed task"];
        let resumed = LiveCli::new(Cli::spawn(home.path(), &args, &[("HOME", home_str)]));
        let (request, body) = protocol.receive(&server);
        context_assert_compacted_history(protocol, &body, "resumed task");
        protocol.final_reply(request, "resumed done");
        let output = resumed.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"resumed done\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("Resumed session: {id}")),
            "{stderr}"
        );
        assert!(!stderr.contains("Session: "), "{stderr}");
        let after_first = std::fs::read(&path).unwrap();
        assert!(
            after_first.starts_with(&compacted),
            "resume must append, not rewrite"
        );
        assert!(String::from_utf8_lossy(&after_first).contains(OLD_USER));

        // A second resume appends from the correct native cursor: the summary,
        // the retained suffix, and both resumed exchanges appear exactly once.
        let args = ["--resume", id.as_str(), "-p", "second resumed"];
        let resumed = LiveCli::new(Cli::spawn(home.path(), &args, &[("HOME", home_str)]));
        let (request, body) = protocol.receive(&server);
        context_assert_second_resume(protocol, &body);
        protocol.final_reply(request, "second done");
        let output = resumed.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"second done\n");
        let after_second = std::fs::read(&path).unwrap();
        assert!(
            after_second.starts_with(&after_first),
            "second resume must keep appending"
        );
        assert!(String::from_utf8_lossy(&after_second).contains(OLD_USER));
        assert_eq!(context_session_count(home.path()), 1);
        no_more(&server);
    }
}
