/// 只读子 Agent 与 Plan 同源：execution 层强制 ReadOnly，共享同一个真实
/// 服务器进程，拒绝全部 mutating 动作且不启动任何 checker。
#[test]
fn lsp_roundtrip_readonly_subagents_share_server_and_deny_mutations() {
    let provider = server();
    let dir = tempfile::tempdir().unwrap();
    lsp_project(dir.path());
    let log = dir.path().join("lsp-log.jsonl");
    let barrier = dir.path().join("barriers");
    lsp_write_config(dir.path(), &log, &barrier, &[]);
    let mut cli = super::streaming::LiveCli::new(Cli::start_with(
        dir.path(),
        Some("readonly subagents"),
        &lsp_settings(false, false),
        &lsp_models(&provider, "completion"),
    ));
    let (request, body) = lsp_receive(&provider, "completion");
    assert!(body.to_string().contains("\"task\""));
    lsp_calls_reply(
        request,
        "completion",
        json!([call(
            "spawn-readonly",
            "task",
            json!({"context":"readonly","tasks":[{"role":"fast","readonly":true,"task":"inspect only"},{"role":"fast","readonly":true,"task":"review only"}]})
        )]),
    );
    let mut children_done = 0;
    let mut parent_done = false;
    for _ in 0..30 {
        let (request, body) = lsp_receive(&provider, "completion");
        if lsp_is_child(&body) {
            let outputs = lsp_output_ids("completion", &body);
            if !outputs.contains("child-bad") {
                lsp_assert_schema(&body, "completion", true);
                lsp_calls_reply(
                    request,
                    "completion",
                    json!([
                        call(
                            "child-bad",
                            "lsp",
                            json!({"action":"rename","file":"src/helper.lspfixture","line":1,"symbol":"value","new_name":"number","apply":true})
                        ),
                        call(
                            "child-request",
                            "lsp",
                            json!({"action":"request","query":"textDocument/hover"})
                        ),
                        call("child-reload", "lsp", json!({"action":"reload"})),
                        call(
                            "child-code",
                            "lsp",
                            json!({"action":"code_actions","file":"src/helper.lspfixture","apply":true})
                        ),
                        call(
                            "child-workspace",
                            "lsp",
                            json!({"action":"diagnostics","file":"*"})
                        ),
                    ]),
                );
            } else if !outputs.contains("child-def") {
                for id in [
                    "child-bad",
                    "child-request",
                    "child-reload",
                    "child-code",
                    "child-workspace",
                ] {
                    let result = lsp_tool_result("completion", &body, id);
                    assert_eq!(result["ok"], false, "{id}: {result}");
                    assert!(
                        result["error"].as_str().unwrap().contains("read-only")
                            || result["error"].as_str().unwrap().contains("not available")
                            || result["error"].as_str().unwrap().contains("unsupported")
                            || result["error"]
                                .as_str()
                                .unwrap()
                                .contains("unknown lsp action")
                            || result["error"].as_str().unwrap().contains("not run by LSP"),
                        "{id}: {result}"
                    );
                }
                lsp_calls_reply(
                    request,
                    "completion",
                    json!([
                        call(
                            "child-def",
                            "lsp",
                            json!({"action":"definition","file":"src/lib.lspfixture","line":3,"symbol":"value","timeout":10})
                        ),
                        call(
                            "child-advice",
                            "lsp",
                            json!({"action":"rename","file":"src/helper.lspfixture","line":1,"symbol":"value","new_name":"number","timeout":10})
                        )
                    ]),
                );
            } else {
                let definition = lsp_tool_result("completion", &body, "child-def");
                assert_eq!(definition["ok"], true, "{definition}");
                let advice = lsp_tool_result("completion", &body, "child-advice");
                assert_eq!(advice["ok"], true, "{advice}");
                assert_eq!(advice["details"]["applied"], false);
                assert_eq!(
                    fs::read_to_string(dir.path().join("src/helper.lspfixture")).unwrap(),
                    LSP_HELPER
                );
                children_done += 1;
                lsp_final(request, "completion", "readonly-child-done");
            }
        } else if children_done == 2
            && lsp_dev_message("completion", &body)
                .matches("pp-subagent-result:")
                .count()
                == 2
        {
            lsp_final(request, "completion", "readonly subagents done");
            parent_done = true;
            break;
        } else {
            lsp_final(request, "completion", "waiting for readonly children");
        }
    }
    assert!(parent_done && children_done == 2);
    cli.wait_for(super::streaming::Pipe::Stdout, "readonly subagents done");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert!(!dir.path().join("target").exists());
    let events = lsp_wait_started(&log);
    let pids = lsp_started_pids(&events);
    assert_eq!(
        pids.len(),
        1,
        "readonly subagents must share one server: {events:?}"
    );
    lsp_assert_no_method(
        &events,
        &["textDocument/codeAction", "workspace/executeCommand"],
    );
    lsp_assert_pid_dead(pids[0]);
    no_more(&provider);
}

/// 行模式：慢诊断跨过内联 500ms 窗口后，空闲时显示迟到报告但不触发模型
/// 请求；下一次用户任务的首个请求恰好包含一次该报告，native 配对不拆。
#[test]
fn lsp_roundtrip_writethrough_late_line_idle_and_next_request() {
    for protocol in ["completion", "response"] {
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
}

/// 非隔离子 Agent 与父 Agent 在同一 cwd 共享同一个服务器 PID，但各自持有
/// 独立的诊断 owner/inbox。
#[test]
fn lsp_roundtrip_parent_child_owner_reports_share_fixture() {
    let provider = server();
    let dir = tempfile::tempdir().unwrap();
    lsp_project(dir.path());
    let log = dir.path().join("lsp-log.jsonl");
    let barrier = dir.path().join("barriers");
    lsp_write_config(dir.path(), &log, &barrier, &[]);
    let cli = Cli::start_with(
        dir.path(),
        Some("owner reports"),
        &lsp_settings(false, false),
        &lsp_models(&provider, "completion"),
    );
    let (request, _) = lsp_receive(&provider, "completion");
    lsp_calls_reply(
        request,
        "completion",
        json!([lsp_call(
            "completion",
            "parent-diag-1",
            json!({"action":"diagnostics","file":"src/helper.lspfixture","timeout":10})
        )]),
    );
    let (request, body) = lsp_receive(&provider, "completion");
    let first = lsp_tool_result("completion", &body, "parent-diag-1");
    let parent_owner = first["diagnostics"]["owner"].as_str().unwrap().to_owned();
    lsp_calls_reply(
        request,
        "completion",
        json!([call(
            "spawn-owner",
            "task",
            json!({"context":"owner check","tasks":[{"role":"fast","readonly":true,"task":"diagnostics only"}]})
        )]),
    );
    let mut child_owner = None;
    for _ in 0..20 {
        let (request, body) = lsp_receive(&provider, "completion");
        if lsp_is_child(&body) {
            let outputs = lsp_output_ids("completion", &body);
            if !outputs.contains("child-diag") {
                lsp_calls_reply(
                    request,
                    "completion",
                    json!([call(
                        "child-diag",
                        "lsp",
                        json!({"action":"diagnostics","file":"src/helper.lspfixture","timeout":10})
                    )]),
                );
            } else {
                let result = lsp_tool_result("completion", &body, "child-diag");
                let owner = result["diagnostics"]["owner"].as_str().unwrap().to_owned();
                assert_ne!(owner, parent_owner, "child reused the parent owner");
                child_owner = Some(owner.clone());
                lsp_final(request, "completion", &format!("child owner {owner}"));
            }
        } else {
            let outputs = lsp_output_ids("completion", &body);
            if outputs.contains("parent-diag-2") {
                let result = lsp_tool_result("completion", &body, "parent-diag-2");
                let owner = result["diagnostics"]["owner"].as_str().unwrap();
                assert_eq!(owner, parent_owner);
                assert_ne!(owner, child_owner.as_deref().unwrap());
                lsp_final(request, "completion", "owner reports done");
                break;
            }
            if child_owner.is_some() {
                lsp_calls_reply(
                    request,
                    "completion",
                    json!([lsp_call(
                        "completion",
                        "parent-diag-2",
                        json!({"action":"diagnostics","file":"src/helper.lspfixture","timeout":10})
                    )]),
                );
            } else {
                lsp_final(request, "completion", "waiting for child owner");
            }
        }
    }
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    let events = lsp_wait_started(&log);
    let pids = lsp_started_pids(&events);
    assert_eq!(
        pids.len(),
        1,
        "same-cwd child must share the parent PID: {events:?}"
    );
    lsp_assert_pid_dead(pids[0]);
    no_more(&provider);
}

/// isolated 子 Agent 使用 prepare 后的独立 cwd：不同根、不同服务器 PID。
#[test]
fn lsp_roundtrip_isolated_subagent_uses_separate_client() {
    let provider = server();
    let project = tempfile::tempdir().unwrap();
    lsp_project(project.path());
    let status = Command::new("git")
        .current_dir(project.path())
        .args(["init", "-q"])
        .status()
        .unwrap();
    assert!(status.success());
    for (key, value) in [
        ("user.email", "fixture@example.com"),
        ("user.name", "fixture"),
    ] {
        let status = Command::new("git")
            .current_dir(project.path())
            .args(["config", key, value])
            .status()
            .unwrap();
        assert!(status.success());
    }
    let status = Command::new("git")
        .current_dir(project.path())
        .args(["add", "."])
        .status()
        .unwrap();
    assert!(status.success());
    let status = Command::new("git")
        .current_dir(project.path())
        .args(["commit", "-qm", "fixture"])
        .status()
        .unwrap();
    assert!(status.success());

    let log = project.path().join("lsp-log.jsonl");
    let barrier = project.path().join("barriers");
    lsp_write_config(project.path(), &log, &barrier, &[]);
    let cli = Cli::start_with(
        project.path(),
        Some("isolated client"),
        &lsp_settings(false, false),
        &lsp_models(&provider, "completion"),
    );
    let (request, _) = lsp_receive(&provider, "completion");
    lsp_calls_reply(
        request,
        "completion",
        json!([lsp_call(
            "completion",
            "parent-def",
            json!({"action":"definition","file":"src/lib.lspfixture","line":3,"symbol":"value","timeout":10})
        )]),
    );
    let (request, body) = lsp_receive(&provider, "completion");
    let parent = lsp_tool_result("completion", &body, "parent-def");
    assert_eq!(parent["ok"], true, "{parent}");
    lsp_calls_reply(
        request,
        "completion",
        json!([call(
            "spawn-isolated",
            "task",
            json!({"context":"isolated","tasks":[{"role":"fast","readonly":true,"task":"isolated lookup","isolated":true}]})
        )]),
    );
    let mut child_done = false;
    for _ in 0..20 {
        let (request, body) = lsp_receive(&provider, "completion");
        if lsp_is_child(&body) {
            let outputs = lsp_output_ids("completion", &body);
            if !outputs.contains("child-def") {
                lsp_calls_reply(
                    request,
                    "completion",
                    json!([call(
                        "child-def",
                        "lsp",
                        json!({"action":"definition","file":"src/lib.lspfixture","line":3,"symbol":"value","timeout":10})
                    )]),
                );
            } else {
                let result = lsp_tool_result("completion", &body, "child-def");
                assert_eq!(result["ok"], true, "{result}");
                assert!(
                    result["content"]
                        .as_str()
                        .unwrap()
                        .contains("src/helper.lspfixture:1:8"),
                    "{result}"
                );
                child_done = true;
                lsp_final(request, "completion", "isolated child done");
            }
        } else if child_done && body.to_string().contains("isolated child done") {
            lsp_final(request, "completion", "isolated parent done");
            break;
        } else {
            lsp_final(request, "completion", "waiting for isolated child");
        }
    }
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    let events = lsp_wait_log(&log, "two isolated starts", |events| {
        lsp_started_pids(events).len() >= 2
    });
    let pids = lsp_started_pids(&events);
    assert_eq!(pids.len(), 2, "{events:?}");
    assert_ne!(pids[0], pids[1]);
    let roots: Vec<PathBuf> = events
        .iter()
        .filter(|event| lsp_started(event))
        .map(|event| fs::canonicalize(event["cwd"].as_str().unwrap()).unwrap())
        .collect();
    let source = fs::canonicalize(project.path()).unwrap();
    assert_eq!(roots.iter().filter(|root| **root == source).count(), 1);
    assert_eq!(roots.iter().filter(|root| **root != source).count(), 1);
    for pid in pids {
        lsp_assert_pid_dead(pid);
    }
    no_more(&provider);
}

#[test]
fn lsp_settings_invalid_timeout_and_unknown_fields_reject_before_request() {
    let provider = server();
    for setting in [
        "lsp: {requestTimeout: 4}",
        "lsp: {formatOnWrite: true}",
        "lsp: {requestTimeout: 301}",
        "lsp: {requestTimeout: 5.5}",
        "lsp: {shared: secret-marker}",
        "lsp: {enabled: secret-marker}",
    ] {
        let home = tempfile::tempdir().unwrap();
        let output = Cli::start_with(
            home.path(),
            Some("hello"),
            setting,
            &fixture_models(&provider),
        )
        .finish();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            !output.status.success(),
            "invalid LSP setting accepted: {setting}"
        );
        assert!(output.stdout.is_empty());
        assert!(!stderr.contains("secret-marker"), "{stderr}");
    }
    no_more(&provider);
}
