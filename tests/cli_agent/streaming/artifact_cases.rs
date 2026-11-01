use super::*;

#[test]
fn artifact_failed_command_roundtrip_in_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let text = (1..=10000)
            .map(|i| format!("record-{i:05}: test diagnostic\n"))
            .collect::<String>();
        std::fs::write(dir.path().join("fixture.log"), &text).unwrap();
        let cli = protocol.start(&server, dir.path(), Some("inspect failing test logs"));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "large",
                "bash",
                json!({"command":"cat fixture.log; exit 7"})
            )]),
        );
        let (request, body) = protocol.receive(&server);
        let command = protocol.tool_result(&body, "large");
        assert_eq!(command["exit_code"], 7);
        assert_eq!(command["ok"], false);
        assert_eq!(command["truncated"], true);
        assert!(command["output"].as_str().unwrap().len() <= 51200 + 128);
        assert!(!command["output"].as_str().unwrap().contains("record-05000"));
        let uri = command["artifact"]["uri"].as_str().unwrap();
        protocol.calls_reply(
            request,
            json!([
                protocol.call(
                    "range",
                    "read",
                    json!({"path":format!("{uri}:raw:5000-5000")})
                ),
                protocol.call(
                    "search",
                    "grep",
                    json!({"path":uri,"pattern":"record-05000"})
                ),
                protocol.call("tail", "read", json!({"path":format!("{uri}:-1")}))
            ]),
        );
        let (request, body) = protocol.receive(&server);
        assert_eq!(
            protocol.tool_result(&body, "range")["content"],
            "record-05000: test diagnostic\n"
        );
        assert_eq!(
            protocol.tool_result(&body, "search")["details"]["matchCount"],
            1
        );
        assert!(
            protocol.tool_result(&body, "tail")["content"]
                .as_str()
                .unwrap()
                .contains("record-10000")
        );
        protocol.final_reply(request, "failure inspected");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("artifact://0"));
        let session = std::fs::read_dir(config_path(dir.path(), "sessions"))
            .unwrap()
            .flatten()
            .find(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
            .unwrap()
            .path();
        let journal = std::fs::read_to_string(&session).unwrap();
        assert!(!journal.contains("tool_output_snapshot"));
        assert!(
            journal.len() < text.len(),
            "journal unexpectedly retained the full log"
        );
        let id = session.file_stem().unwrap().to_str().unwrap();
        let resumed = LiveCli::new(Cli::spawn(
            dir.path(),
            &["-r", id, "-p", "inspect saved logs"],
            &[],
        ));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([
                protocol.call(
                    "saved",
                    "read",
                    json!({"path":"artifact://0:raw:5000-5000"})
                ),
                protocol.call("next", "bash", json!({"command":"cat fixture.log"}))
            ]),
        );
        let (request, body) = protocol.receive(&server);
        assert_eq!(
            protocol.tool_result(&body, "saved")["content"],
            "record-05000: test diagnostic\n"
        );
        assert_eq!(
            protocol.tool_result(&body, "next")["artifact"]["uri"],
            "artifact://1"
        );
        protocol.final_reply(request, "resumed logs verified");
        assert!(resumed.finish().status.success());
        let fresh = LiveCli::new(Cli::spawn(dir.path(), &["-p", "new root"], &[]));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call("foreign", "read", json!({"path":"artifact://0"}))]),
        );
        let (request, body) = protocol.receive(&server);
        assert_eq!(protocol.tool_result(&body, "foreign")["ok"], false);
        protocol.final_reply(request, "root isolated");
        assert!(fresh.finish().status.success());
    }
}

#[test]
fn artifact_capture_failure_keeps_exit_status_in_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = protocol.start_with_settings(
            &server,
            dir.path(),
            Some("check capture failure"),
            r#"{"tools":{"artifactSpillThreshold":8}}"#,
        );
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call("prepare", "read", json!({"path":"missing"}))]),
        );
        let (request, _) = protocol.receive(&server);
        let session = std::fs::read_dir(config_path(dir.path(), "sessions"))
            .unwrap()
            .flatten()
            .find(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
            .unwrap()
            .path();
        std::fs::write(session.with_extension("artifacts"), "blocked").unwrap();
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "large",
                "bash",
                json!({"command":"printf '0123456789abcdef'; exit 9"})
            )]),
        );
        let (request, body) = protocol.receive(&server);
        let value = protocol.tool_result(&body, "large");
        assert_eq!(value["exit_code"], 9);
        assert_eq!(value["ok"], false);
        assert!(value["capture_error"].is_string());
        assert!(value.get("artifact").is_none());
        protocol.final_reply(request, "exit code preserved");
        assert!(cli.finish().status.success());
    }
}
