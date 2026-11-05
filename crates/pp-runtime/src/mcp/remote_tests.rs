use super::*;
#[path = "../../../../tests/fixtures/mcp_remote.rs"]
mod fixture;
use fixture::RemoteServer;

fn load(server: &RemoteServer, transport: &str, timeout: u64) -> (tempfile::TempDir, Registry) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mcp.json");
    std::fs::write(&path, server.config(transport, timeout).to_string()).unwrap();
    let registry = Registry::load(&path, dir.path(), &CancellationToken::new(), true).unwrap();
    assert_eq!(
        registry.definitions().len(),
        2,
        "{}",
        registry.view(true).text()
    );
    (dir, registry)
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
        let (_dir, mut registry) = load(&server, transport, 3000);
        let token = CancellationToken::new();
        let result = registry
            .call("mcp__remote__echo", r#"{"text":"中文\n第二行"}"#, &token)
            .unwrap();
        assert_eq!(result["result"]["content"][0]["text"], "中文\n第二行");
        assert_eq!(result["result"]["structuredContent"]["count"], 1);
        assert_eq!(result["ok"], true);
        assert_eq!(
            registry.call("mcp__remote__fail", "{}", &token).unwrap()["ok"],
            false
        );
        registry.shutdown().unwrap();
        assert!(registry.tracking.lock().unwrap().connections.is_empty());
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
        let (_dir, mut registry) = load(&server, "http", 1000);
        let start = Instant::now();
        let error = registry
            .call("mcp__remote__echo", "{}", &CancellationToken::new())
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
            assert!(registry.definitions().is_empty(), "{mode}");
            assert!(
                registry
                    .call("mcp__remote__echo", "{}", &CancellationToken::new())
                    .is_err()
            );
        }
        registry.shutdown().unwrap();
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
    let error = Registry::load(&path, dir.path(), &token, true)
        .err()
        .unwrap();
    canceller.join().unwrap();
    assert!(error.is::<Cancelled>());
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn mcp_remote_cancel_lazy_restart_checks_complete_tool_definitions() {
    for transport in ["http", "sse"] {
        for changed in [false, true] {
            let server = RemoteServer::new("stall_once");
            let (_dir, mut registry) = load(&server, transport, 3000);
            let token = CancellationToken::new();
            let call = registry.clone();
            let cancel = token.clone();
            let worker = thread::spawn(move || call.call("mcp__remote__echo", "{}", &cancel));
            server.wait_calls(1);
            token.cancel();
            assert!(worker.join().unwrap().unwrap_err().is::<Cancelled>());
            if changed {
                server.changed();
            }
            let result = registry.call(
                "mcp__remote__echo",
                r#"{"text":"restarted"}"#,
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
            registry.shutdown().unwrap();
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
    let mut registry = Registry::load(&path, dir.path(), &token, false).unwrap();
    let cleanup = registry.cleanup().unwrap();
    for action in [
        Action::List,
        Action::Disable("remote".into()),
        Action::Enable("remote".into()),
        Action::Reload,
    ] {
        registry.manage(&action, dir.path(), &token, false).unwrap();
    }
    assert!(server.log().is_empty());
    registry
        .manage(
            &Action::Reconnect("remote".into()),
            dir.path(),
            &token,
            true,
        )
        .unwrap();
    assert_eq!(registry.definitions().len(), 2);
    std::fs::write(&path, "invalid secret").unwrap();
    assert!(
        registry
            .manage(&Action::Reload, dir.path(), &token, true)
            .is_err()
    );
    assert_eq!(
        registry.call("mcp__remote__echo", "{}", &token).unwrap()["ok"],
        true
    );
    std::fs::write(&path, server.config("http", 1000).to_string()).unwrap();
    server.changed();
    registry
        .manage(&Action::Reload, dir.path(), &token, true)
        .unwrap();
    assert!(
        registry
            .definitions()
            .iter()
            .any(|t| t.description.as_deref() == Some("changed"))
    );
    drop(cleanup);
    assert!(
        registry
            .tracking
            .lock()
            .unwrap()
            .connections
            .iter()
            .all(|s| !s.lock().unwrap().is_live())
    );
    let before = server.log().len();
    assert!(registry.call("mcp__remote__echo", "{}", &token).is_err());
    assert_eq!(server.log().len(), before);
}

#[test]
fn mcp_remote_cleanup_interrupts_active_requests_without_waiting_for_state_lock() {
    for transport in ["http", "sse"] {
        let server = RemoteServer::new("stall_call");
        let (_dir, registry) = load(&server, transport, 30000);
        let cleanup = registry.cleanup().unwrap();
        let worker = thread::spawn(move || {
            registry.call("mcp__remote__echo", "{}", &CancellationToken::new())
        });
        server.wait_calls(1);
        let start = Instant::now();
        drop(cleanup);
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
    let registry = Registry::load(&path, dir.path(), &CancellationToken::new(), true).unwrap();
    assert!(registry.definitions().is_empty());
    assert_eq!(server.log().len(), 1);
    assert!(!registry.view(true).text().contains("secret"));
}
