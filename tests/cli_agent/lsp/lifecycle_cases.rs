/// 只读子 Agent 与 Plan 同源：execution 层强制 ReadOnly，共享同一个真实
/// 服务器进程，拒绝全部 mutating 动作且不启动任何 checker。
/// 行模式：慢诊断跨过内联 500ms 窗口后，空闲时显示迟到报告但不触发模型
/// 请求；下一次用户任务的首个请求恰好包含一次该报告，native 配对不拆。
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

        // final reply 之后没有任何新输入：先确认没有 provider 请求，再释放
        // FIFO，让后台诊断在空闲轮询中被展示。
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

