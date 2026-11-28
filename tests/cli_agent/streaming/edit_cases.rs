use super::*;

#[test]
fn hashline_edit_roundtrip_and_stale_snapshot_recovery() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let demo = dir.path().join("demo.txt");
        std::fs::write(&demo, "one\ntwo\nthree\n").unwrap();
        let cli = protocol.start_with_settings(
            &server,
            dir.path(),
            Some("edit demo.txt"),
            r#"{"edit":"hashline"}"#,
        );
        let (request, body) = protocol.receive(&server);
        assert_edit_schema(&protocol.edit_tool(&body), false);
        protocol.calls_reply(
            request,
            json!([protocol.call("read-first", "read", json!({"path":"demo.txt"}))]),
        );
        let (request, body) = protocol.receive(&server);
        let read = protocol.tool_result(&body, "read-first");
        let content = read["content"].as_str().unwrap();
        assert_eq!(
            hashline_anchors(content)
                .iter()
                .map(|(_, s)| s.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
        let header = hashline_header(content).to_owned();
        // Deliberately scramble the operation order; all line numbers still belong to the original snapshot.
        protocol.calls_reply(request,json!([protocol.call("edit-multi","edit",json!({"input":format!("{header}\nPUT 2.=2:\n+TWO\nPUT <1:\n+zero\nPUT >3:\n+four")}))]));
        let (request, body) = protocol.receive(&server);
        let result = protocol.tool_result(&body, "edit-multi");
        assert_eq!(
            result,
            json!({"ok":true,"edits_applied":3,"bytes_written":24}),
            "{protocol:?}: {result}"
        );
        assert_eq!(
            std::fs::read(&demo).unwrap(),
            b"zero\none\nTWO\nthree\nfour\n"
        );
        // After success the old snapshot is stale, so a fresh read is required to continue.
        protocol.calls_reply(
            request,
            json!([
                protocol.call(
                    "edit-without-read",
                    "edit",
                    json!({"input":format!("{header}\nCUT 1.=1")})
                ),
                protocol.call("read-second", "read", json!({"path":"demo.txt"}))
            ]),
        );
        let (request, body) = protocol.receive(&server);
        assert_eq!(
            protocol.tool_result(&body, "edit-without-read")["ok"],
            false
        );
        let read = protocol.tool_result(&body, "read-second");
        let header = hashline_header(read["content"].as_str().unwrap()).to_owned();
        std::fs::write(&demo, "zero\none\nexternal\nthree\nfour\n").unwrap();
        protocol.calls_reply(
            request,
            json!([
                protocol.call(
                    "edit-stale",
                    "edit",
                    json!({"input":format!("{header}\nPUT 2.=2:\n+uno\nPUT 3.=3:\n+recovered")})
                ),
                protocol.call("read-third", "read", json!({"path":"demo.txt"}))
            ]),
        );
        let (request, body) = protocol.receive(&server);
        let failed = protocol.tool_result(&body, "edit-stale");
        assert_eq!(failed["ok"], false);
        assert!(
            failed["error"]
                .as_str()
                .unwrap()
                .contains("snapshot changed")
        );
        assert_eq!(
            std::fs::read(&demo).unwrap(),
            b"zero\none\nexternal\nthree\nfour\n"
        );
        let read = protocol.tool_result(&body, "read-third");
        let header = hashline_header(read["content"].as_str().unwrap());
        protocol.calls_reply(request,json!([protocol.call("edit-retry","edit",json!({"input":format!("*** Begin Patch\n{header}\nPUT 3.=3:\n+recovered\n*** End Patch")}))]));
        let (request, body) = protocol.receive(&server);
        assert_eq!(protocol.tool_result(&body, "edit-retry")["ok"], true);
        assert_eq!(
            std::fs::read(&demo).unwrap(),
            b"zero\none\nrecovered\nthree\nfour\n"
        );
        protocol.final_reply(request, "hashline-ok");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        assert_eq!(output.stdout, b"hashline-ok\n");
        assert_clean_pipes(&output);
    }
}

#[test]
fn hardened_tools_recover_after_limits_timeout_and_rejected_edits_in_both_protocols() {
    let protocol = Protocol::Responses;
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let outside = outside_tempdir();
    let external = outside.path().join("external");
    std::fs::write(&external, "keep").unwrap();
    let ext_link = dir.path().join("ext-link");
    std::os::unix::fs::symlink(&external, &ext_link).unwrap();
    std::fs::write(dir.path().join("demo"), "one\ntwo\nthree\n").unwrap();
    let settings = r#"{"read_max_bytes":8,"read_max_lines":1,"bash_timeout_seconds":1}"#;
    let cli = protocol.start_with_settings(&server, dir.path(), Some("hardened tools"), settings);
    let (request, _) = protocol.receive(&server);
    protocol.calls_reply(
        request,
        json!([
            protocol.call("page", "read", json!({"path":"demo"})),
            protocol.call("bad-limit", "read", json!({"path":"demo","limit":0})),
            protocol.call(
                "outside",
                "write",
                json!({"path":"ext-link","content":"bad"})
            ),
            protocol.call(
                "bad-edit",
                "edit",
                json!({"path":"demo","edits":[
                    {"oldText":"one","newText":"bad"}, {"oldText":"missing","newText":"bad"}
                ]})
            ),
            protocol.call(
                "timeout",
                "bash",
                json!({"command":"printf before; printf error >&2; sleep 10; printf late > late"})
            ),
            protocol.call(
                "after-timeout",
                "write",
                json!({"path":"after","content":"kept\n"})
            )
        ]),
    );
    let (request, body) = protocol.receive(&server);
    assert_eq!(
        protocol.tool_result(&body, "page"),
        json!({"ok":true,"source":"demo","content":"one\n","truncated":true,"lines_read":1,"next_offset":2,"next_path":"demo:2-","ranges":[{"start":1,"end":1}]})
    );
    for id in ["bad-limit", "outside", "bad-edit"] {
        assert_eq!(
            protocol.tool_result(&body, id)["ok"],
            false,
            "{protocol:?}: {id}"
        );
    }
    let bad_limit = protocol.tool_result(&body, "bad-limit");
    assert!(
        bad_limit["error"]
            .as_str()
            .is_some_and(|error| error.contains("limit must be >= 1")),
        "{bad_limit}"
    );
    assert_eq!(
        std::fs::read(dir.path().join("demo")).unwrap(),
        b"one\ntwo\nthree\n"
    );
    assert_eq!(std::fs::read(&external).unwrap(), b"keep");
    let timeout = protocol.tool_result(&body, "timeout");
    assert_eq!(timeout["ok"], false);
    assert_eq!(timeout["timed_out"], true);
    assert!(timeout["output"].as_str().unwrap().contains("before"));
    assert!(timeout["output"].as_str().unwrap().contains("error"));
    assert!(!dir.path().join("late").exists());
    assert_eq!(protocol.tool_result(&body, "after-timeout")["ok"], true);
    protocol.calls_reply(
        request,
        json!([
            protocol.call("next-page", "read", json!({"path":"demo:2"})),
            protocol.call(
                "repair",
                "edit",
                json!({"path":"demo","edits":[{"oldText":"two","newText":"fixed"}]})
            ),
            protocol.call("verify", "read", json!({"path":"demo:2"})),
            protocol.call("verify-after", "read", json!({"path":"after"}))
        ]),
    );
    let (request, body) = protocol.receive(&server);
    assert_eq!(protocol.tool_result(&body, "next-page")["content"], "two\n");
    assert_eq!(protocol.tool_result(&body, "repair")["ok"], true);
    assert_eq!(protocol.tool_result(&body, "verify")["content"], "fixed\n");
    assert_eq!(
        protocol.tool_result(&body, "verify-after")["content"],
        "kept\n"
    );
    assert_eq!(protocol.tool_result(&body, "timeout")["timed_out"], true);
    protocol.final_reply(request, "recovered");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"recovered\n");
    assert_clean_pipes(&output);
    no_more(&server);
}
