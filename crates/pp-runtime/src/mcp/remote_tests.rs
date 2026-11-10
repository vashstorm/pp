use super::*;
#[path = "../../../../tests/fixtures/mcp_remote.rs"]
mod fixture;
use fixture::RemoteServer;

fn load(
    server: &RemoteServer,
    transport: &str,
    timeout: u64,
) -> (tempfile::TempDir, Arc<McpManager>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    std::fs::write(&path, server.config(transport, timeout).to_string()).unwrap();
    let manager = Arc::new(McpManager::new(path, dir.path().to_path_buf()));
    manager
        .connect_enabled(&CancellationToken::new(), Access::Full)
        .unwrap();
    assert_eq!(
        manager.definitions(Access::Full).len(),
        2,
        "{}",
        manager.view(Access::Full).text()
    );
    (dir, manager)
}

#[test]
fn mcp_remote_roundtrip_json_http_sse_and_legacy_sse() {
    for (transport, mode) in [
        ("http", "json"),
        ("http", "sse"),
        ("sse", "json"),
        ("http", "no_session"),
        ("sse", "absolute_endpoint"),
    ] {
        let server = RemoteServer::new(mode);
        let (_dir, manager) = load(&server, transport, 3000);
        let token = CancellationToken::new();
        let result = manager
            .call(
                "mcp__remote__echo",
                r#"{"text":"中文\n第二行"}"#,
                Access::Full,
                &token,
            )
            .unwrap();
        assert_eq!(result["result"]["content"][0]["text"], "中文\n第二行");
        assert_eq!(result["result"]["structuredContent"]["count"], 1);
        assert_eq!(result["ok"], true);
        assert_eq!(
            manager
                .call("mcp__remote__fail", "{}", Access::Full, &token)
                .unwrap()["ok"],
            false
        );
        manager.shutdown().unwrap();
        assert_eq!(manager.test_connection_count(), 0);
        let log = server.log();
        let initialize = log
            .iter()
            .find(|r| r["body"]["method"] == "initialize")
            .unwrap();
        assert_eq!(initialize["headers"]["x-test-key"], "literal-secret");
        assert_eq!(
            initialize["headers"]["accept"],
            "application/json, text/event-stream"
        );
        let notified = log
            .iter()
            .find(|r| r["body"]["method"] == "notifications/initialized")
            .unwrap();
        assert_eq!(notified["headers"]["mcp-protocol-version"], "2025-06-18");
        if transport == "http" {
            if mode == "no_session" {
                assert!(notified["headers"].get("mcp-session-id").is_none());
            } else {
                assert_eq!(notified["headers"]["mcp-session-id"], "secret-session");
            }
            assert_eq!(
                log.iter().filter(|r| r["method"] == "DELETE").count(),
                usize::from(mode != "no_session")
            );
        } else {
            assert!(log.iter().any(|r| r["method"] == "GET"));
        }
        assert!(
            log.iter()
                .any(|r| r["body"]["params"]["cursor"] == "second")
        );
        if mode == "sse" || transport == "sse" {
            assert!(
                log.iter()
                    .any(|r| r["body"]["id"] == "server-ping" && r["body"]["result"] == json!({}))
            );
            assert!(
                log.iter()
                    .any(|r| r["body"]["id"] == "unsupported"
                        && r["body"]["error"]["code"] == -32601)
            );
        }
    }
}

#[test]
fn mcp_remote_failures_are_redacted_bounded_and_never_replay_calls() {
    for mode in [
        "status401",
        "status404",
        "status500",
        "redirect",
        "disconnect",
        "bad_json",
        "wrong_id",
        "oversized",
        "stall_call",
        "rpc_error",
    ] {
        let server = RemoteServer::new(mode);
        let (_dir, manager) = load(&server, "http", 1000);
        let start = Instant::now();
        let error = manager
            .call(
                "mcp__remote__echo",
                "{}",
                Access::Full,
                &CancellationToken::new(),
            )
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(
            !text.contains("secret") && !text.contains(&server.url),
            "{text}"
        );
        assert!(start.elapsed() < Duration::from_secs(3), "{mode}");
        if mode == "stall_call" {
            assert!(text.contains("timed out"));
        }
        if mode != "rpc_error" {
            assert!(manager.definitions(Access::Full).is_empty(), "{mode}");
            assert!(
                manager
                    .call(
                        "mcp__remote__echo",
                        "{}",
                        Access::Full,
                        &CancellationToken::new()
                    )
                    .is_err()
            );
        }
        manager.shutdown().unwrap();
        assert_eq!(
            server
                .log()
                .iter()
                .filter(|r| r["body"]["method"] == "tools/call")
                .count(),
            1,
            "{mode}"
        );
    }
}

#[test]
fn mcp_remote_cancel_during_headers_closes_worker() {
    let server = RemoteServer::new("stall_headers");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    std::fs::write(&path, server.config("http", 30000).to_string()).unwrap();
    let token = CancellationToken::new();
    let cancel = token.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        cancel.cancel();
    });
    let start = Instant::now();
    let manager = McpManager::new(path, dir.path().to_path_buf());
    let error = manager.connect_enabled(&token, Access::Full).err().unwrap();
    canceller.join().unwrap();
    assert!(error.is::<Cancelled>());
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn mcp_remote_cancel_lazy_restart_checks_complete_tool_definitions() {
    for transport in ["http", "sse"] {
        for changed in [false, true] {
            let server = RemoteServer::new("stall_once");
            let (_dir, manager) = load(&server, transport, 3000);
            let token = CancellationToken::new();
            let call = manager.clone();
            let cancel = token.clone();
            let worker =
                thread::spawn(move || call.call("mcp__remote__echo", "{}", Access::Full, &cancel));
            server.wait_calls(1);
            token.cancel();
            assert!(worker.join().unwrap().unwrap_err().is::<Cancelled>());
            if changed {
                server.changed();
            }
            let result = manager.call(
                "mcp__remote__echo",
                r#"{"text":"restarted"}"#,
                Access::Full,
                &CancellationToken::new(),
            );
            if changed {
                assert!(
                    result
                        .unwrap_err()
                        .to_string()
                        .contains("definitions changed")
                );
            } else {
                assert_eq!(result.unwrap()["ok"], true);
            }
            let log = server.log();
            assert_eq!(
                log.iter()
                    .filter(|r| r["body"]["method"] == "initialize")
                    .count(),
                2
            );
            assert_eq!(
                log.iter()
                    .filter(|r| r["body"]["method"] == "tools/call")
                    .count(),
                if changed { 1 } else { 2 }
            );
            manager.shutdown().unwrap();
        }
    }
}

#[test]
fn mcp_remote_management_plan_reload_and_cleanup_track_later_connections() {
    let server = RemoteServer::new("json");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    std::fs::write(&path, server.config("http", 1000).to_string()).unwrap();
    let token = CancellationToken::new();
    let manager = McpManager::new(path.clone(), dir.path().to_path_buf());
    manager.connect_enabled(&token, Access::ReadOnly).unwrap();
    for action in [
        Action::List,
        Action::Disable("remote".into()),
        Action::Enable("remote".into()),
        Action::Reload,
    ] {
        manager.manage(&action, &token, Access::ReadOnly).unwrap();
    }
    assert!(server.log().is_empty());
    manager
        .manage(&Action::Reconnect("remote".into()), &token, Access::Full)
        .unwrap();
    assert_eq!(manager.definitions(Access::Full).len(), 2);
    std::fs::write(&path, "invalid secret").unwrap();
    assert!(
        manager
            .manage(&Action::Reload, &token, Access::Full)
            .is_err()
    );
    assert_eq!(
        manager
            .call("mcp__remote__echo", "{}", Access::Full, &token)
            .unwrap()["ok"],
        true
    );
    std::fs::write(&path, server.config("http", 1000).to_string()).unwrap();
    server.changed();
    manager
        .manage(&Action::Reload, &token, Access::Full)
        .unwrap();
    assert!(
        manager
            .definitions(Access::Full)
            .iter()
            .any(|t| t.description.as_deref() == Some("changed"))
    );
    // 退出路径覆盖 reload 之后新建的连接，并禁止旧槽复活。
    manager.shutdown().unwrap();
    assert_eq!(manager.test_connection_count(), 0);
    let before = server.log().len();
    assert!(
        manager
            .call("mcp__remote__echo", "{}", Access::Full, &token)
            .is_err()
    );
    assert_eq!(server.log().len(), before);
}

#[test]
fn mcp_remote_cleanup_interrupts_active_requests_without_waiting_for_state_lock() {
    for transport in ["http", "sse"] {
        let server = RemoteServer::new("stall_call");
        let (_dir, manager) = load(&server, transport, 30000);
        let call = manager.clone();
        let worker = thread::spawn(move || {
            call.call(
                "mcp__remote__echo",
                "{}",
                Access::Full,
                &CancellationToken::new(),
            )
        });
        server.wait_calls(1);
        let start = Instant::now();
        manager.shutdown().unwrap();
        assert!(worker.join().unwrap().is_err());
        assert!(start.elapsed() < Duration::from_secs(2));
    }
}

#[test]
fn mcp_remote_legacy_endpoint_cannot_forward_credentials_cross_origin() {
    let server = RemoteServer::new("cross_origin");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    std::fs::write(&path, server.config("sse", 1000).to_string()).unwrap();
    let manager = McpManager::new(path, dir.path().to_path_buf());
    manager
        .connect_enabled(&CancellationToken::new(), Access::Full)
        .unwrap();
    assert!(manager.definitions(Access::Full).is_empty());
    assert_eq!(server.log().len(), 1);
    assert!(!manager.view(Access::Full).text().contains("secret"));
}

#[test]
fn mcp_read_only_scope_matches_original_names_and_management_filters_connections() {
    let remote = RemoteServer::new("readonly_tools");
    let suspended = RemoteServer::new("json");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    let mut config = remote.config("http", 3000);
    let entry = config["mcpServers"]["remote"].as_object_mut().unwrap();
    entry.remove("headers");
    entry.insert(
        "read_only_tools".into(),
        json!([fixture::LONG_TOOL, "missing"]),
    );
    config["mcpServers"]["suspended"] =
        suspended.config("http", 3000)["mcpServers"]["remote"].clone();
    std::fs::write(&path, config.to_string()).unwrap();
    let token = CancellationToken::new();
    let manager = Arc::new(McpManager::new(path.clone(), dir.path().to_path_buf()));
    manager.connect_enabled(&token, Access::Full).unwrap();
    assert_eq!(manager.definitions(Access::Full).len(), 4);
    // Plan/Review 只按 access 过滤工具，不断开任何已连接的服务进程。
    assert_eq!(manager.definitions(Access::ReadOnly).len(), 1);
    assert!(
        manager
            .view(Access::ReadOnly)
            .text()
            .contains("suspended (Plan/Review)")
    );

    let name = exposed_name("remote", fixture::LONG_TOOL);
    assert!(name.starts_with("mcp__remote__tool_"));
    assert_eq!(manager.definitions(Access::ReadOnly)[0].name, name);
    assert!(
        manager
            .view(Access::ReadOnly)
            .text()
            .contains("1 configured read-only tools were not discovered")
    );
    let error = manager
        .proxy_call("mcp__remote__fail", "invalid", Access::ReadOnly, &token)
        .unwrap_err();
    assert!(error.to_string().contains("not authorized"));
    assert_eq!(
        manager
            .proxy_call(&name, r#"{"text":"public"}"#, Access::ReadOnly, &token)
            .unwrap()["result"]["content"][0]["text"],
        "public"
    );
    // 进程仍保持连接：Access::Full 立刻恢复完整工具表，且从未重新 initialize，
    // 未授权服务也没有收到终止会话的 DELETE。
    assert_eq!(manager.definitions(Access::Full).len(), 4);
    for (label, server) in [("remote", &remote), ("suspended", &suspended)] {
        let log = server.log();
        assert!(
            !log.iter().any(|entry| entry["method"] == "DELETE"),
            "{label}"
        );
        assert_eq!(
            log.iter()
                .filter(|entry| entry["body"]["method"] == "initialize")
                .count(),
            1,
            "{label}"
        );
    }
    let log = remote.log();
    assert!(
        log.iter()
            .all(|entry| entry["headers"].get("x-test-key").is_none())
    );
    let calls: Vec<_> = log
        .iter()
        .filter(|entry| entry["body"]["method"] == "tools/call")
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["body"]["params"]["name"], fixture::LONG_TOOL);

    let manage = |action| manager.manage(&action, &token, Access::ReadOnly);
    assert!(manage(Action::Reconnect("suspended".into())).is_err());
    manage(Action::Disable("remote".into())).unwrap();
    assert!(manager.definitions(Access::ReadOnly).is_empty());
    manage(Action::Enable("remote".into())).unwrap();
    assert_eq!(manager.definitions(Access::ReadOnly).len(), 1);
    manage(Action::Reconnect("remote".into())).unwrap();
    assert_eq!(manager.definitions(Access::ReadOnly).len(), 1);

    config["mcpServers"]["remote"]
        .as_object_mut()
        .unwrap()
        .remove("read_only_tools");
    std::fs::write(&path, config.to_string()).unwrap();
    manage(Action::Reload).unwrap();
    // 去掉只读授权后，ReadOnly 的 Reload 不再连接任何服务。
    assert!(manager.definitions(Access::Full).is_empty());
    assert_eq!(
        suspended
            .log()
            .iter()
            .filter(|r| r["body"]["method"] == "initialize")
            .count(),
        1
    );
}
