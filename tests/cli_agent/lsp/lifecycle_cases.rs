/// The read-only subagent shares its origin with Plan: the execution layer forces ReadOnly, shares the same real
/// server process, rejects every mutating action and starts no checker.
/// Line mode: once a slow diagnostic crosses the inline 500ms window, idle shows the late report without triggering a model
/// request; the next user task's first request contains that report exactly once and native pairing does not split.
#[test]
fn lsp_roundtrip_writethrough_late_line_idle_and_next_request() {
    let protocol = "response";
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        lsp_project(dir.path());
        let log = dir.path().join("lsp-log.jsonl");
        let barrier = dir.path().join("barriers");
        lsp_write_config(dir.path(), &log, &barrier, &[]);
        lsp_fifo(&barrier.join("diagnostics"));
        let mut cli = super::streaming::LiveCli::new(Cli::start_with(
            dir.path(),
            None,
            &lsp_settings(false, false),
            &lsp_models(&provider, protocol),
        ));
        cli.input("late diagnostics task\n");
        let (request, _) = lsp_receive(&provider, protocol);
        lsp_calls_reply(
            request,
            protocol,
            json!([match protocol {
                "completion" => call(
                    "late-write",
                    "write",
                    json!({"path":"src/helper.lspfixture","content":"pub fn value() -> i32 { \"wrong\" }\n"}),
                ),
                _ => response_call(
                    "late-write",
                    "write",
                    json!({"path":"src/helper.lspfixture","content":"pub fn value() -> i32 { \"wrong\" }\n"}),
                ),
            }]),
        );
        lsp_wait_log(&log, "diagnostics barrier", |events| {
            events.iter().any(|event| event["barrier"] == "diagnostics")
        });
        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_pairs(protocol, &body);
        let write = lsp_tool_result(protocol, &body, "late-write");
        assert_eq!(write["ok"], true, "{write}");
        assert!(
            write["warning"]
                .as_str()
                .is_some_and(|warning| warning.contains("still running in the background")),
            "inline path must hand off instead of blocking: {write}"
        );
        assert!(write.get("diagnostics").is_none(), "{write}");
        lsp_final(request, protocol, "late task first pass");

        // No new input since the final reply: confirm there is no provider request before releasing the
        // FIFO so the background diagnostics surface during an idle poll.
        no_more(&provider);
        lsp_release_fifo(&barrier.join("diagnostics"));
        cli.wait_for(super::streaming::Pipe::Stderr, "LSP diagnostics (late)");
        cli.wait_for(super::streaming::Pipe::Stderr, "fixture: wrong value");
        no_more(&provider);

        cli.input("late follow-up task\n");
        let (request, body) = lsp_receive(&provider, protocol);
        let text = body.to_string();
        assert_eq!(
            text.matches("LSP diagnostics (late):").count(),
            1,
            "late report must be injected exactly once: {body}"
        );
        assert_eq!(
            text.matches("fixture: wrong value").count(),
            1,
            "diagnostic message repeated or missing: {body}"
        );
        let developer = lsp_dev_message(protocol, &body);
        assert!(developer.contains("src/helper.lspfixture"), "{developer}");
        lsp_assert_pairs(protocol, &body);
        lsp_final(request, protocol, "late follow-up done");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        let events = lsp_wait_started(&log);
        let pids = lsp_started_pids(&events);
        assert_eq!(pids.len(), 1, "{events:?}");
        lsp_assert_pid_dead(pids[0]);
}

