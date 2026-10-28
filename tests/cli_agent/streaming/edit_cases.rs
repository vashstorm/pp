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
        // 故意打乱操作顺序，所有行号仍属于原始快照。
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
        // 成功后旧快照失效，重新读取才能继续。
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
fn replace_edit_roundtrip_and_atomic_failure() {
    for protocol in PROTOCOLS {
        for settings in [
            fixture_settings(), // no edit field: replace is the default
            r#"{"default_model":"fixture/primary","edit":"replace"}"#,
        ] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let demo = dir.path().join("demo.txt");
            let original = "keep “Ｋ”  \nsay “Hi”—x  \ntail\n";
            std::fs::write(&demo, original).unwrap();
            let cli =
                protocol.start_with_settings(&server, dir.path(), Some("edit demo.txt"), settings);

            // The schema actually sent exposes only replace parameters.
            let (request, body) = protocol.receive(&server);
            assert_edit_schema(&protocol.edit_tool(&body), true);

            // Plain read: the original text without anchors or prefixes.
            protocol.calls_reply(
                request,
                json!([protocol.call("read-first", "read", json!({"path":"demo.txt"}))]),
            );
            let (request, body) = protocol.receive(&server);
            let read = protocol.tool_result(&body, "read-first");
            assert_eq!(read["ok"], true, "{protocol:?}: {read}");
            assert_eq!(read["truncated"], false);
            assert_eq!(read["content"], original);

            // One batch with an exact and a loose match, deliberately out of
            // file order; both entries locate the original snapshot.
            protocol.calls_reply(
                request,
                json!([protocol.call(
                    "edit-batch",
                    "edit",
                    json!({
                        "path":"demo.txt",
                        "edits":[
                            {"oldText":"tail","newText":"TAIL"},
                            {"oldText":"say \"Hi\"-x","newText":"said"}
                        ]
                    })
                )]),
            );
            let (request, body) = protocol.receive(&server);
            let edited = protocol.tool_result(&body, "edit-batch");
            let expected = "keep “Ｋ”  \nsaid  \nTAIL\n";
            assert_eq!(edited["ok"], true, "{protocol:?}: {edited}");
            assert_eq!(edited["edits_applied"], 2);
            assert_eq!(edited["bytes_written"], expected.len());
            assert_eq!(std::fs::read(&demo).unwrap(), expected.as_bytes());

            // A valid entry plus a missing oldText rejects the whole batch.
            protocol.calls_reply(
                request,
                json!([protocol.call(
                    "edit-atomic",
                    "edit",
                    json!({
                        "path":"demo.txt",
                        "edits":[
                            {"oldText":"said","newText":"SHOUT"},
                            {"oldText":"not-in-the-file","newText":"x"}
                        ]
                    })
                )]),
            );
            let (request, body) = protocol.receive(&server);
            let failed = protocol.tool_result(&body, "edit-atomic");
            assert_eq!(failed["ok"], false, "{protocol:?}: {failed}");
            assert_eq!(std::fs::read(&demo).unwrap(), expected.as_bytes());

            // The corrected edit and a verifying read both succeed.
            protocol.calls_reply(
                request,
                json!([protocol.call(
                    "edit-retry",
                    "edit",
                    json!({
                        "path":"demo.txt",
                        "edits":[{"oldText":"said","newText":"recovered"}]
                    })
                )]),
            );
            let (request, body) = protocol.receive(&server);
            let recovered = protocol.tool_result(&body, "edit-retry");
            assert_eq!(recovered["ok"], true, "{protocol:?}: {recovered}");
            let final_text = "keep “Ｋ”  \nrecovered  \nTAIL\n";
            assert_eq!(std::fs::read(&demo).unwrap(), final_text.as_bytes());
            protocol.calls_reply(
                request,
                json!([protocol.call("read-final", "read", json!({"path":"demo.txt"}))]),
            );
            let (request, body) = protocol.receive(&server);
            let read = protocol.tool_result(&body, "read-final");
            assert_eq!(read["ok"], true);
            assert_eq!(read["content"], final_text);
            protocol.final_reply(request, "replace-ok");
            let output = cli.finish();
            assert!(output.status.success(), "{protocol:?}: {output:?}");
            assert_eq!(output.stdout, b"replace-ok\n");
            assert_clean_pipes(&output);
            no_more(&server);
        }
    }
}

#[test]
fn edit_mode_survives_model_switch() {
    for replace in [true, false] {
        let a = server();
        let b = server();
        let dir = tempfile::tempdir().unwrap();
        let demo = dir.path().join("demo.txt");
        std::fs::write(&demo, "one\n").unwrap();
        let models = json!({"providers": {
            "a": slash_provider(&a, "a", "completion", json!({"primary": {"model":"vendor-a"}})),
            "b": slash_provider(&b, "b", "response", json!({"secondary": {"model":"vendor-b"}}))}})
        .to_string();
        let settings = if replace {
            r#"{"default_model":"a/primary","edit":"replace"}"#
        } else {
            r#"{"default_model":"a/primary","edit":"hashline"}"#
        };
        let mut cli = Cli::start_with(dir.path(), None, settings, &models);
        let mut current = "one";

        // Completion -> Responses -> Completion with the same edit mode.
        for (step, (protocol, switch)) in [
            (Protocol::Chat, ""),
            (Protocol::Responses, "/model b/secondary\n"),
            (Protocol::Chat, "/model a/primary\n"),
        ]
        .into_iter()
        .enumerate()
        {
            let next = ["two", "three", "four"][step];
            cli.input(&format!("{switch}stage {step}\n"));
            let stage_server = match protocol {
                Protocol::Chat => &a,
                Protocol::Responses => &b,
            };

            // The schema still exposes the configured mode after each switch.
            let (request, body) = protocol.receive(stage_server);
            assert_edit_schema(&protocol.edit_tool(&body), replace);

            // A fresh read, never a replayed tool result from history.
            protocol.calls_reply(
                request,
                json!([protocol.call(&format!("read-{step}"), "read", json!({"path":"demo.txt"}))]),
            );
            let (request, body) = protocol.receive(stage_server);
            let read = protocol.tool_result(&body, &format!("read-{step}"));
            assert_eq!(read["ok"], true, "{protocol:?}: {read}");
            let edit_args = if replace {
                json!({"path":"demo.txt","edits":[
                    {"oldText":current,"newText":next}
                ]})
            } else {
                // Hashline anchors come from this stage's read.
                let anchors = hashline_anchors(read["content"].as_str().unwrap());
                assert_eq!(anchors.len(), 1, "{protocol:?}: {read}");
                assert_eq!(anchors[0].1, current);
                json!({"input":format!("{}\nPUT 1.=1:\n+{next}",hashline_header(read["content"].as_str().unwrap()))})
            };
            protocol.calls_reply(
                request,
                json!([protocol.call(&format!("edit-{step}"), "edit", edit_args)]),
            );
            let (request, body) = protocol.receive(stage_server);
            let edited = protocol.tool_result(&body, &format!("edit-{step}"));
            assert_eq!(edited["ok"], true, "{protocol:?}: {edited}");
            assert_eq!(edited["edits_applied"], 1);
            assert_eq!(
                std::fs::read(&demo).unwrap(),
                format!("{next}\n").as_bytes()
            );
            protocol.final_reply(request, &format!("done-{step}"));
            current = next;
        }

        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"done-0\ndone-1\ndone-2\n");
        assert_clean_pipes(&output);
        no_more(&a);
        no_more(&b);
    }
}

#[test]
fn hardened_tools_recover_after_limits_timeout_and_rejected_edits_in_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let external = outside.path().join("external");
        std::fs::write(&external, "keep").unwrap();
        std::fs::write(dir.path().join("demo"), "one\ntwo\nthree\n").unwrap();
        let settings = r#"{"read_max_bytes":8,"read_max_lines":1,"bash_timeout_seconds":1}"#;
        let cli =
            protocol.start_with_settings(&server, dir.path(), Some("hardened tools"), settings);
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(request, json!([
            protocol.call("page", "read", json!({"path":"demo"})),
            protocol.call("bad-limit", "read", json!({"path":"demo","limit":2})),
            protocol.call("outside", "write", json!({"path":external,"content":"bad"})),
            protocol.call("bad-edit", "edit", json!({"path":"demo","edits":[
                {"oldText":"one","newText":"bad"}, {"oldText":"missing","newText":"bad"}
            ]})),
            protocol.call("timeout", "bash", json!({"command":"printf before; printf error >&2; sleep 10; printf late > late"})),
            protocol.call("after-timeout", "write", json!({"path":"after","content":"kept\n"}))
        ]));
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
        assert_eq!(
            std::fs::read(dir.path().join("demo")).unwrap(),
            b"one\ntwo\nthree\n"
        );
        assert_eq!(std::fs::read(&external).unwrap(), b"keep");
        let timeout = protocol.tool_result(&body, "timeout");
        assert_eq!(timeout["ok"], false);
        assert_eq!(timeout["timed_out"], true);
        assert_eq!(timeout["stdout"], "before");
        assert_eq!(timeout["stderr"], "error");
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
}

#[test]
fn tool_limits_remain_startup_snapshots_across_model_switch_and_new() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("demo"), "one\ntwo\n").unwrap();
    let models = json!({"providers":{
        "a": slash_provider(&a, "a", "completion", json!({"primary": {"model":"vendor-a"}})),
        "b": slash_provider(&b, "b", "response", json!({"secondary": {"model":"vendor-b"}}))}})
    .to_string();
    let settings = r#"{"read_max_bytes":4,"read_max_lines":1,"bash_timeout_seconds":1}"#;
    let mut cli = Cli::start_with(dir.path(), None, settings, &models);
    for (step, protocol) in [Protocol::Chat, Protocol::Responses, Protocol::Chat]
        .into_iter()
        .enumerate()
    {
        let prefix = match step {
            0 => "",
            1 => "/model b/secondary\n/new\n",
            _ => "/model a/primary\n/new\n",
        };
        cli.input(&format!("{prefix}stage {step}\n"));
        let server = match protocol {
            Protocol::Chat => &a,
            Protocol::Responses => &b,
        };
        let (request, _) = protocol.receive(server);
        // 启动后改配置，切换模型和新建会话均不得放宽工具限制。
        std::fs::write(
            config_path(dir.path(), "settings.yml"),
            r#"{"read_max_bytes":65536,"read_max_lines":2000,"bash_timeout_seconds":120}"#,
        )
        .unwrap();
        let page = format!("page-{step}");
        let limit = format!("limit-{step}");
        let timeout = format!("timeout-{step}");
        protocol.calls_reply(
            request,
            json!([
                protocol.call(&page, "read", json!({"path":"demo"})),
                protocol.call(&limit, "read", json!({"path":"demo","limit":2})),
                protocol.call(&timeout, "bash", json!({"command":"sleep 10"}))
            ]),
        );
        let (request, body) = protocol.receive(server);
        assert_eq!(protocol.tool_result(&body, &page)["content"], "one\n");
        assert_eq!(protocol.tool_result(&body, &page)["next_offset"], 2);
        assert_eq!(protocol.tool_result(&body, &limit)["ok"], false);
        assert_eq!(protocol.tool_result(&body, &timeout)["timed_out"], true);
        protocol.final_reply(request, &format!("done-{step}"));
    }
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"done-0\ndone-1\ndone-2\n");
}

#[test]
fn glob_roundtrip_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("space dir")).unwrap();
        std::fs::write(
            dir.path().join("space dir/demo.txt"),
            "needle\nother\nneedle\n",
        )
        .unwrap();
        let models = match protocol {
            Protocol::Chat => fixture_models(&server),
            Protocol::Responses => response_models(&server, Some("response")),
        };
        std::fs::write(config_path(dir.path(), "settings.yml"), fixture_settings()).unwrap();
        std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
        let mut cli = LiveCli::new(Cli::spawn(dir.path(), &["-p", "search files"], &[]));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([
                protocol.call(
                    "match",
                    "grep",
                    json!({"pattern":"needle","path":"space dir/demo.txt"})
                ),
                protocol.call("paths", "glob", json!({"path":"space dir/*.txt"})),
                protocol.call(
                    "absent",
                    "grep",
                    json!({"pattern":"absent","path":"space dir/demo.txt"})
                ),
                protocol.call(
                    "bad-regex",
                    "grep",
                    json!({"pattern":"[","path":"space dir/demo.txt"})
                ),
                protocol.call("bad-args", "grep", json!({"args":[]}))
            ]),
        );
        cli.wait_for(Pipe::Stderr, "*1|needle");
        cli.assert_running();
        let (request, body) = protocol.receive(&server);
        let matched = protocol.tool_result(&body, "match");
        assert_eq!(matched["ok"], true);
        assert!(matched["content"].as_str().unwrap().contains("*1|needle"));
        let found = protocol.tool_result(&body, "paths");
        assert_eq!(found["ok"], true);
        assert!(found["content"].as_str().unwrap().contains("demo.txt"));
        assert_eq!(
            protocol.tool_result(&body, "absent")["content"],
            "No matches found"
        );
        assert_eq!(
            protocol.tool_result(&body, "bad-regex")["content"],
            "No matches found"
        );
        assert_eq!(protocol.tool_result(&body, "bad-args")["ok"], false);
        protocol.final_reply(request, "search-complete");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        assert_eq!(output.stdout, b"search-complete\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("*1|needle"), "{stderr}");
        assert!(stderr.contains("demo.txt"), "{stderr}");
        assert!(!stderr.contains("[bash"), "{stderr}");
        assert!(stderr.contains("tool grep: ok"), "{stderr}");
        assert!(stderr.contains("invalid grep arguments"), "{stderr}");
        assert!(stderr.contains("tool glob: ok"), "{stderr}");
        assert_clean_pipes(&output);
        no_more(&server);
    }
}
