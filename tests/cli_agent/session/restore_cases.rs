use super::*;

#[test]
fn session_failures_keep_executed_tool_results_and_display_only_fragments() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();

        let mut cli = start_configured(api, &server, home.path(), project.path(), &[]);
        cli.input("use the tools\n");
        let (request, _) = api.receive(&server);
        api.calls_reply(
            request,
            json!([api.call("w", "write", json!({"path":"kept.txt","content":"kept"}))]),
        );
        let (request, body) = api.receive(&server);
        assert_eq!(api.tool_result(&body, "w")["bytes_written"], 4);
        // The next model answer fails after streaming a partial fragment.
        let mut stream = SseReply::new(request);
        match api {
            Api::Chat => {
                stream.event(chat_chunk(
                    json!({"role":"assistant","content":"partial fragment"}),
                    None,
                ));
                stream.event(json!({"error":{
                    "message":"private-provider-body fixture-key",
                    "type":"server_error","code":"private-provider-body"
                }}));
                stream.finish();
            }
            Api::Responses => {
                stream.event(json!({
                    "type":"response.output_text.delta","sequence_number":0,
                    "item_id":"msg-x","output_index":0,"content_index":0,
                    "delta":"partial fragment","logprobs":[]
                }));
                // End HTTP before response.completed: a complete transport with an interrupted protocol turn.
                stream.finish();
            }
        }
        let output = cli.finish();
        assert!(
            output.status.success(),
            "line mode must survive a task error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("partial fragment"));
        assert!(!stderr.contains("private-provider-body"));
        assert!(!stderr.contains("fixture-key"));

        let id = only_session_id(home.path());
        let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
        let file_text = fs::read_to_string(&path).unwrap();
        // The partial fragment is display log only: present as an event
        // record, absent from the rebuilt native history.
        assert!(file_text.contains("partial fragment"));
        let saved = records(&path);
        assert_session_contract(&saved, &id, project.path());
        assert!(
            !serde_json::to_string(&canonical_messages(&saved))
                .unwrap()
                .contains("partial fragment")
        );
        assert!(
            records(&path)
                .iter()
                .any(|record| record["customType"] == "pp.event")
        );
        assert!(
            records(&path)
                .iter()
                .any(|record| record["customType"] == "pp.error")
        );
        assert!(!records(&path).iter().any(|record| {
            record["customType"] == "pp.error"
                && record["data"]["message"]
                    .to_string()
                    .contains("private-provider-body")
        }));
        assert!(
            !rebuild_request(&records(&path))
                .to_string()
                .contains("partial fragment")
        );
        assert!(
            fs::read_to_string(project.path().join("kept.txt"))
                .is_ok_and(|content| content == "kept")
        );

        // A fully written readable projection followed by a killed native
        // commit must still not become model context. Long nonempty entry
        // IDs are accepted on load even though pp generates eight hex digits.
        with_mutated_records(&path, |records| {
            push_entry(
                records,
                json!({
                    "type":"message",
                    "message":{
                        "role":"assistant", "api":if api == Api::Chat { "openai-completions" } else { "openai-responses" },
                        "provider":"fixture", "model":"fixture-model",
                        "content":[{"type":"text","text":"uncommitted projection"}],
                        "stopReason":"stop", "timestamp":1792224000000_i64
                    }
                }),
            );
        });
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":\"custom\",\"customType\":\"pp.native_append\",\"data\":")
            .unwrap();
        // After restart the executed tool result, but neither the streamed
        // fragment nor the uncommitted canonical answer, is native history.
        let cli = start_configured(
            api,
            &server,
            home.path(),
            launch.path(),
            &["-r", &id, "-p", "what happened"],
        );
        let (request, body) = api.receive(&server);
        assert_eq!(api.tool_result(&body, "w")["bytes_written"], 4);
        let (users, _) = api.turns(&body);
        assert_eq!(users, ["use the tools", "what happened"]);
        assert!(!body.to_string().contains("partial fragment"));
        assert!(!body.to_string().contains("uncommitted projection"));
        api.final_reply(request, "tools ran");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"tools ran\n");
        no_more(&server);
    }
}

#[test]
fn session_broken_stdout_still_persists_executed_tool_results() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    configure(home.path(), fixture_settings(), &fixture_models(&server));
    let mut cli = Cli(Some(
        command(
            home.path(),
            project.path(),
            &["-p", "record with closed display"],
        )
        .spawn()
        .unwrap(),
    ));
    drop(cli.0.as_mut().unwrap().stdout.take());
    let (request, _) = receive(&server);
    calls_reply(
        request,
        json!([call(
            "write",
            "write",
            json!({"path":"saved-record","content":"record me"})
        )]),
    );
    let (request, body) = receive(&server);
    assert_eq!(result(&body["messages"][3], "write")["bytes_written"], 9);
    final_reply(request, "finished history");
    let output = cli.finish();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("writing task output"));
    assert_eq!(
        fs::read_to_string(project.path().join("saved-record")).unwrap(),
        "record me"
    );
    no_more(&server);

    // The executed tool result survives the broken display and is native
    // history for the next run.
    let id = only_session_id(home.path());
    let cli = start_configured(
        Api::Chat,
        &server,
        home.path(),
        launch.path(),
        &["-r", &id, "-p", "after broken display"],
    );
    let (request, body) = receive(&server);
    assert_eq!(result(&body["messages"][3], "write")["bytes_written"], 9);
    final_reply(request, "recovered");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"recovered\n");
    no_more(&server);
}

#[test]
fn plan_headless_restore_refuses_before_mcp_or_model_start() {
    for api in APIS {
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let (id, path) = make_saved_session(
            api,
            &provider,
            home.path(),
            project.path(),
            "Plan restore fixture",
            "saved",
        );
        with_mutated_records(&path, |records| {
            push_entry(
                records,
                json!({"type":"custom","customType":"pp.workflow","data":{"version":1,"workflow":{"mode":"plan","plan":null,"review":null}}}),
            );
        });
        configure(home.path(), &json!({"mcp_servers":{"must_not_spawn":{"command":"bash","args":["-c","touch forbidden-mcp; exit 1"]}}}).to_string(), &api.models(&provider));
        for args in [
            vec!["-r", id.as_str()],
            vec!["-c", id.as_str()],
            vec!["-r", id.as_str(), "-p", "not approval"],
        ] {
            expect_restore_failure(
                &provider,
                home.path(),
                project.path(),
                &args,
                &["Saved session is in Plan mode; resume it in the TUI."],
                &[],
            );
            assert!(!project.path().join("forbidden-mcp").exists());
        }
    }
}

#[test]
fn session_loader_rejects_corrupt_or_unavailable_logs() {
    let server = server();
    let cwd = tempfile::tempdir().unwrap();

    // A well-formed but absent ID fails to open the file.
    {
        let home = tempfile::tempdir().unwrap();
        configure(home.path(), fixture_settings(), &fixture_models(&server));
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &[
                "-r",
                "2026-10-17T01-02-03-00000000000000000000000000000000",
                "-p",
                "hi",
            ],
            &["opening saved session"],
            &["fixture-key"],
        );
        // Malformed and traversal IDs are rejected before any filesystem
        // access.
        for id in [
            "../escape",
            "id.jsonl",
            "back\\slash",
            "2026-10-17T01-02-03-0123456789ABCDEF0123456789ABCDEF",
            "2026-10-17T01-02-03-0123456789abcdef0123456789abcde",
            "2026-10-17T01-02-03",
            "2026-10-17T01-02-03-0123456789abcdef0123456789abcdeg",
            "2026-02-29T01-02-03-0123456789abcdef0123456789abcdef",
            "1792195200000-0123456789abcdef0123456789abcdef",
        ] {
            expect_restore_failure(
                &server,
                home.path(),
                cwd.path(),
                &["-r", id, "-p", "hi"],
                &["invalid session ID"],
                &["fixture-key"],
            );
        }
        // -c validates a supplied ID the same way.
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-c", "nope"],
            &["invalid session ID"],
            &["fixture-key"],
        );
        assert!(session_ids(home.path()).is_empty());
    }

    // Empty and missing session directories hold nothing to continue.
    {
        let home = tempfile::tempdir().unwrap();
        configure(home.path(), fixture_settings(), &fixture_models(&server));
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-c"],
            &["no saved sessions found"],
            &["fixture-key"],
        );
        let empty = tempfile::tempdir().unwrap();
        configure(empty.path(), fixture_settings(), &fixture_models(&server));
        fs::create_dir_all(sessions_dir(empty.path())).unwrap();
        expect_restore_failure(
            &server,
            empty.path(),
            cwd.path(),
            &["-c"],
            &["no saved sessions found"],
            &["fixture-key"],
        );
    }

    // Empty, non-header, and explicitly unsupported legacy files.
    {
        let home = tempfile::tempdir().unwrap();
        configure(home.path(), fixture_settings(), &fixture_models(&server));
        fs::create_dir_all(sessions_dir(home.path())).unwrap();
        let id = "2026-10-17T01-02-03-00000000000000000000000000000000";
        fs::write(sessions_dir(home.path()).join(format!("{id}.jsonl")), "").unwrap();
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", id, "-p", "hi"],
            &["missing session header"],
            &["fixture-key"],
        );
        fs::write(
            sessions_dir(home.path()).join(format!("{id}.jsonl")),
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"x\",\"timestamp\":0}}\n",
        )
        .unwrap();
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", id, "-p", "hi"],
            &["first record must be a session header"],
            &["fixture-key"],
        );
        fs::write(
            sessions_dir(home.path()).join(format!("{id}.jsonl")),
            "{\"type\":\"checkpoint\",\"version\":1,\"secret\":\"fixture-key\"}\n",
        )
        .unwrap();
        for args in [vec!["-r", id, "-p", "hi"], vec!["-c"]] {
            expect_restore_failure(
                &server,
                home.path(),
                cwd.path(),
                &args,
                &["unsupported legacy session format"],
                &["fixture-key"],
            );
        }
    }

    // Structural corruption of otherwise valid session logs: every case is a
    // fresh session, mutated, and expected to fail without touching files.
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            records[0]["version"] = json!(2)
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["unsupported session version"],
            &["fixture-key"],
        );
    }
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            records[0]["id"] = json!("2026-10-17T01-02-04-00000000000000000000000000000000");
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["session ID does not match filename"],
            &["fixture-key"],
        );
    }
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            session_state_mut(records)["toolLimits"]["read_max_bytes"] = json!(0);
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["read_max_bytes must be greater than zero"],
            &["fixture-key"],
        );
    }
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            push_entry(
                records,
                json!({"type":"custom","customType":"pp.native_append","data":{"protocol":"response","items":[]}}),
            );
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["append protocol mismatch"],
            &["fixture-key"],
        );
    }
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            session_state_mut(records)["session"]["request"]["messages"]
                .as_array_mut()
                .unwrap()
                .remove(0);
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["Completion first message must be system"],
            &["fixture-key"],
        );
    }
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            records[0]["cwd"] = json!("relative")
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["session working directory must be absolute"],
            &["fixture-key"],
        );
    }
    // Unknown record type: rejected without echoing the record body.
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            push_entry(
                records,
                json!({"type":"mystery","secret-record-body":"fixture-key"}),
            );
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["invalid session record"],
            &["secret-record-body", "fixture-key"],
        );
    }
    // Every semantic mutation starts with a valid generated file. Added rows
    // get real chain envelopes, and intentional reordering is relinked.
    type MutationCase = (&'static str, fn(&mut Vec<Value>));
    let cases: &[MutationCase] = &[
        ("unsupported pp session version", |records| {
            session_state_mut(records)["version"] = json!(2);
        }),
        ("missing pp session state", |records| {
            records.truncate(1);
        }),
        ("invalid session entry chain", |records| {
            records[1]["parentId"] = json!("not-the-parent");
        }),
        ("invalid session entry chain", |records| {
            records[2]["parentId"] = Value::Null;
        }),
        ("invalid session entry chain", |records| {
            records[2]["id"] = records[1]["id"].clone();
        }),
        ("invalid session entry chain", |records| {
            records[1]["id"] = json!("");
        }),
        ("invalid session record", |records| {
            records[1].as_object_mut().unwrap().remove("id");
        }),
        ("invalid session", |records| {
            records[1].as_object_mut().unwrap().remove("parentId");
        }),
        ("invalid session", |records| {
            records[2].as_object_mut().unwrap().remove("parentId");
        }),
        ("invalid session record", |records| {
            push_entry(
                records,
                json!({"type":"custom","customType":"pp.future","data":{"secret-record-body":"fixture-key"}}),
            );
        }),
        ("invalid session record", |records| {
            push_entry(
                records,
                json!({"type":"session","version":3,"cwd":"/","secret-record-body":"fixture-key"}),
            );
        }),
        ("invalid session record", |records| {
            records[1].as_object_mut().unwrap().remove("timestamp");
        }),
        ("invalid session record", |records| {
            records[1]["timestamp"] = json!("not-a-timestamp fixture-key");
        }),
        ("invalid session record", |records| {
            records[0]["timestamp"] = json!("not-a-timestamp fixture-key");
        }),
        ("invalid session record", |records| {
            records[0]["timestamp"] = json!("1969-12-31T23:59:59.999Z");
        }),
    ];
    for &(expected, mutate) in cases {
        let (home, id) = corrupted_base(&server, cwd.path(), mutate);
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &[expected],
            &["secret-record-body", "fixture-key"],
        );
    }
    // State-dependent records must not precede the first native snapshot.
    for entry in [
        json!({"type":"thinking_level_change","thinkingLevel":"high"}),
        json!({"type":"custom","customType":"pp.instructions","data":{"system":"x"}}),
        json!({"type":"custom","customType":"pp.native_append","data":{"protocol":"completion","messages":[]}}),
    ] {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            push_entry(records, entry);
            let entry = records.pop().unwrap();
            records.insert(1, entry);
            relink_entries(records);
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["before"],
            &["fixture-key"],
        );
    }
    // A complete (newline-terminated) invalid JSON line in the middle names
    // its line number and leaks nothing.
    {
        let home = tempfile::tempdir().unwrap();
        let (id, path) = make_saved_session(
            Api::Chat,
            &server,
            home.path(),
            cwd.path(),
            "remember base",
            "base noted",
        );
        insert_raw_line(&path, 2, "not-json secret-record-body fixture-key");
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["invalid session record", "line 3"],
            &["secret-record-body", "fixture-key"],
        );
    }

    // A Responses checkpoint whose input is not an item array is corrupt.
    {
        let home = tempfile::tempdir().unwrap();
        let (id, path) = make_saved_session(
            Api::Responses,
            &server,
            home.path(),
            cwd.path(),
            "remember base",
            "base noted",
        );
        with_mutated_records(&path, |records| {
            session_state_mut(records)["session"]["request"]["input"] = json!("plain text");
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["Responses input must be items"],
            &["fixture-key"],
        );
    }

    // The saved model must resolve to the same protocol and model string.
    {
        let home = tempfile::tempdir().unwrap();
        let (id, _path) = make_saved_session(
            Api::Chat,
            &server,
            home.path(),
            cwd.path(),
            "remember base",
            "base noted",
        );
        let variants = [
            // Reference gone from the catalog.
            json!({"providers":{"other": slash_provider(
                &server, "other", "completion",
                json!({"primary": {"model":"fixture-model"}})
            )}})
            .to_string(),
            // Same reference, different model string.
            json!({"providers":{"fixture": slash_provider(
                &server, "fixture", "completion",
                json!({"primary": {"model":"changed-model"}})
            )}})
            .to_string(),
            // Same reference, different protocol.
            json!({"providers":{"fixture": slash_provider(
                &server, "fixture", "response",
                json!({"primary": {"model":"fixture-model"}})
            )}})
            .to_string(),
        ];
        for models in variants {
            let other = tempfile::tempdir().unwrap();
            copy_session(home.path(), other.path(), &id);
            configure(other.path(), fixture_settings(), &models);
            expect_restore_failure(
                &server,
                other.path(),
                cwd.path(),
                &["-r", &id, "-p", "hi"],
                &["saved session model is unavailable or has changed"],
                &["fixture-key"],
            );
        }
    }

    // A session held by another process is explicitly in use.
    {
        let home = tempfile::tempdir().unwrap();
        let (id, _path) = make_saved_session(
            Api::Chat,
            &server,
            home.path(),
            cwd.path(),
            "remember base",
            "base noted",
        );
        let mut holder =
            start_configured(Api::Chat, &server, home.path(), cwd.path(), &["-r", &id]);
        holder.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["session is already in use"],
            &["fixture-key"],
        );
        holder.input("/quit\n");
        holder.finish();
    }

    // Session files are never followed through symlinks.
    {
        let home = tempfile::tempdir().unwrap();
        let (id, path) = make_saved_session(
            Api::Chat,
            &server,
            home.path(),
            cwd.path(),
            "remember base",
            "base noted",
        );
        let real = home.path().join("elsewhere.jsonl");
        fs::rename(&path, &real).unwrap();
        symlink(&real, &path).unwrap();
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["opening saved session"],
            &["fixture-key"],
        );
        assert!(fs::read_to_string(&real).is_ok_and(|content| content.contains("remember base")));
    }
}

#[test]
fn session_truncated_tail_is_ignored_and_repaired_on_first_append() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    let (id, path) = make_saved_session(
        Api::Chat,
        &server,
        home.path(),
        project.path(),
        "remember tail",
        "tail noted",
    );
    let complete = fs::read(&path).unwrap();
    let partial = b"{\"type\":\"message\",\"id\":\"unfin";
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(partial)
        .unwrap();

    // Loading tolerates the incomplete tail and does not modify the file.
    let mut cli = start_configured(Api::Chat, &server, home.path(), launch.path(), &["-r", &id]);
    cli.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
    let stderr = cli.text(Pipe::Err);
    assert!(
        stderr.contains("Ignoring incomplete final record"),
        "{stderr}"
    );
    assert_eq!(
        fs::read(&path).unwrap().len(),
        complete.len() + partial.len()
    );

    // The first real append truncates the partial tail in place.
    cli.input("after repair\n");
    let (request, body) = receive(&server);
    let (users, assistants) = Api::Chat.turns(&body);
    assert_eq!(users, ["remember tail", "after repair"]);
    assert_eq!(assistants, ["tail noted"]);
    final_reply(request, "repaired answer");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = fs::read(&path).unwrap();
    assert!(
        after.starts_with(&complete),
        "repair keeps committed records"
    );
    // 检查实际损坏记录，避免误匹配系统提示词中的 unfinished 等普通单词。
    assert!(!after.windows(partial.len()).any(|window| window == partial));
    assert!(after.ends_with(b"\n"));
    assert_session_contract(&records(&path), &id, project.path());

    // The repaired file loads again without any incomplete-tail notice.
    let cli = start_configured(
        Api::Chat,
        &server,
        home.path(),
        launch.path(),
        &["-r", &id, "-p", "again"],
    );
    let (request, body) = receive(&server);
    let (users, assistants) = Api::Chat.turns(&body);
    assert_eq!(users, ["remember tail", "after repair", "again"]);
    assert_eq!(assistants, ["tail noted", "repaired answer"]);
    final_reply(request, "done again");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Ignoring incomplete"));
    assert_eq!(output.stdout, b"done again\n");
    assert_eq!(only_session_id(home.path()), id);
    no_more(&server);
}
