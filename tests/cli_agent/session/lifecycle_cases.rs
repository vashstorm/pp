use super::*;

#[test]
fn session_lazy_creation_persist_and_unique_ids_for_both_apis() {
    for api in APIS {
        // Line commands, blank input, /new and /quit never create the
        // sessions directory.
        {
            let server = server();
            let home = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let mut cli = start_configured(api, &server, home.path(), cwd.path(), &[]);
            cli.input("\n/help\n/model\n/effort high\n/effort\n/skills\n/new\n/quit\n");
            let output = cli.finish();
            assert!(output.status.success());
            assert!(!sessions_dir(home.path()).exists());
            assert!(!String::from_utf8_lossy(&output.stderr).contains("Resume with:"));
            no_more(&server);
        }
        // Before any prompt the process is idle: no request, no file.
        {
            let server = server();
            let home = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let cli = start_configured(api, &server, home.path(), cwd.path(), &[]);
            no_more(&server);
            assert!(!sessions_dir(home.path()).exists());
            cli.finish();
        }
        // A real prompt creates exactly one session, only after the HTTP
        // request reaches the provider: headers plus the first visible frame
        // prove the create-on-open hook ran before the answer completed.
        {
            let server = server();
            let home = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let mut cli = start_configured(
                api,
                &server,
                home.path(),
                cwd.path(),
                &["-p", "remember alpha"],
            );
            let (request, _) = api.receive(&server);
            assert!(!sessions_dir(home.path()).exists());
            let mut stream = request.start_sse().unwrap();
            match api {
                Api::Chat => stream
                    .event(&chat_chunk(
                        json!({"role":"assistant","content":"alpha noted"}),
                        None,
                    ))
                    .unwrap(),
                Api::Responses => stream
                    .event(&json!({
                        "type":"response.output_text.delta","sequence_number":0,
                        "item_id":"msg-final","output_index":0,"content_index":0,
                        "delta":"alpha noted","logprobs":[]
                    }))
                    .unwrap(),
            }
            cli.wait_for(Pipe::Out, "alpha noted");
            let id = only_session_id(home.path());
            let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
            assert_eq!(
                fs::metadata(sessions_dir(home.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let saved = records(&path);
            assert_session_contract(&saved, &id, cwd.path());
            let state = &saved
                .iter()
                .find(|record| record["customType"] == "pp.session_state")
                .unwrap()["data"];
            assert!(state.get("version").is_none());
            assert_eq!(state["modelReference"], "fixture/primary");
            assert!(state["thinkLevel"].is_string());
            assert!(state["editMode"].is_string());
            assert!(state["toolLimits"].is_object());
            assert!(state["baseSystem"].is_string());
            assert_eq!(
                state["session"]["protocol"],
                if api == Api::Chat {
                    "completion"
                } else {
                    "response"
                }
            );
            let messages = canonical_messages(&saved);
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0]["role"], "user");
            assert_eq!(messages[0]["content"], "remember alpha");
            assert_eq!(saved[1]["customType"], "pp.session_state");
            assert_eq!(saved[2]["type"], "model_change");
            assert_eq!(saved[2]["model"], "fixture/primary");
            assert_eq!(saved[3]["type"], "thinking_level_change");
            assert!(
                saved
                    .iter()
                    .any(|record| record["customType"] == "pp.event")
            );
            // Complete the stream and finish the task.
            match api {
                Api::Chat => {
                    stream.event(&chat_chunk(json!({}), Some("stop"))).unwrap();
                    stream.frame("[DONE]").unwrap();
                }
                Api::Responses => {
                    let part = response_text("alpha noted");
                    let message = response_message("msg-final", json!([part.clone()]));
                    stream
                        .event(&json!({
                            "type":"response.content_part.done","sequence_number":1,
                            "item_id":"msg-final","output_index":0,"content_index":0,"part":part
                        }))
                        .unwrap();
                    stream
                        .event(&json!({
                            "type":"response.output_item.done","sequence_number":2,
                            "output_index":0,"item":message
                        }))
                        .unwrap();
                    stream
                        .event(&json!({
                            "type":"response.completed","sequence_number":3,
                            "response":response_body(json!([message]))
                        }))
                        .unwrap();
                }
            }
            stream.finish();
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"alpha noted\n");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(stderr.matches(&format!("Session: {id}")).count(), 1);
            let resume_hint = format!("Resume with: pp --resume {id}");
            assert_eq!(stderr.matches(&resume_hint).count(), 1);
            assert_eq!(stderr.lines().last(), Some(resume_hint.as_str()));
            let saved = records(&path);
            assert_session_contract(&saved, &id, cwd.path());
            let messages = canonical_messages(&saved);
            assert_eq!(messages.len(), 2);
            let assistant_message = &messages[1];
            assert_eq!(assistant_message["role"], "assistant");
            assert_eq!(assistant_message["provider"], "fixture");
            assert_eq!(assistant_message["model"], "fixture-model");
            assert_eq!(
                assistant_message["api"],
                if api == Api::Chat {
                    "openai-completions"
                } else {
                    "openai-responses"
                }
            );
            assert_eq!(assistant_message["stopReason"], "stop");
            assert_eq!(
                assistant_message["content"],
                json!([{"type":"text","text":"alpha noted"}])
            );
            // The completed answer is loadable native history.
            let (user, assistant) = match api {
                Api::Chat => {
                    let messages = rebuild_request(&records(&path))["messages"]
                        .as_array()
                        .unwrap()
                        .clone();
                    (
                        messages[1]["content"].as_str().unwrap().to_owned(),
                        messages[2]["content"].as_str().unwrap().to_owned(),
                    )
                }
                Api::Responses => {
                    let input = rebuild_request(&records(&path))["input"]
                        .as_array()
                        .unwrap()
                        .clone();
                    (
                        input[0]["content"].as_str().unwrap().to_owned(),
                        input[1]["content"][0]["text"].as_str().unwrap().to_owned(),
                    )
                }
            };
            assert_eq!(user, "remember alpha");
            assert_eq!(assistant, "alpha noted");
        }
        // Two processes sharing one HOME create distinct sessions, proven by
        // distinct valid IDs whose checkpoints carry each process's prompt.
        {
            let server = server();
            let home = tempfile::tempdir().unwrap();
            let first = tempfile::tempdir().unwrap();
            let second = tempfile::tempdir().unwrap();
            // Finish the shared config store initialization first; this scenario only covers parallel sessions' ID and persistence isolation.
            configure(home.path(), fixture_settings(), &api.models(&server));
            pp_config::agent_store::initialize(
                &home.path().join(".pp/agent"),
                &pp_core::cancellation::CancellationToken::new(),
            )
            .unwrap();
            let cli_a = start(home.path(), first.path(), &["-p", "from first"]);
            let cli_b = start(home.path(), second.path(), &["-p", "from second"]);
            for _ in 0..2 {
                let (request, body) = api.receive(&server);
                let (users, _) = api.turns(&body);
                assert_eq!(users.len(), 1);
                if users[0] == "from first" {
                    api.final_reply(request, "first done");
                } else {
                    assert_eq!(users[0], "from second");
                    api.final_reply(request, "second done");
                }
            }
            let output_a = cli_a.finish();
            let output_b = cli_b.finish();
            assert!(
                output_a.status.success(),
                "{}",
                String::from_utf8_lossy(&output_a.stderr)
            );
            assert!(
                output_b.status.success(),
                "{}",
                String::from_utf8_lossy(&output_b.stderr)
            );
            assert_eq!(output_a.stdout, b"first done\n");
            assert_eq!(output_b.stdout, b"second done\n");
            let ids = session_ids(home.path());
            assert_eq!(ids.len(), 2, "both processes must persist a session");
            assert_ne!(ids[0], ids[1]);
            let texts: Vec<String> = ids
                .iter()
                .map(|id| {
                    fs::read_to_string(sessions_dir(home.path()).join(format!("{id}.jsonl")))
                        .unwrap()
                })
                .collect();
            for prompt in ["from first", "from second"] {
                assert_eq!(
                    texts.iter().filter(|text| text.contains(prompt)).count(),
                    1,
                    "prompt {prompt:?} must appear in exactly one session"
                );
            }
            for (id, text) in ids.iter().zip(&texts) {
                let header = &records(&sessions_dir(home.path()).join(format!("{id}.jsonl")))[0];
                assert_eq!(header["id"], *id);
                assert!(text.contains(&format!("\"id\":\"{id}\"")));
            }
        }
    }
    // A blank -p prompt is rejected before any session exists.
    {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let cli = start_configured(Api::Chat, &server, home.path(), cwd.path(), &["-p", "   "]);
        let output = cli.finish();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("prompt must not be blank"));
        assert!(!sessions_dir(home.path()).exists());
        no_more(&server);
    }
    // A broken configuration still runs an interactive line mode without
    // creating any session.
    {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        configure(home.path(), fixture_settings(), "{ not valid yaml");
        let mut cli = start(home.path(), cwd.path(), &[]);
        cli.wait_for(Pipe::Err, "Configuration unavailable");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success());
        assert!(!sessions_dir(home.path()).exists());
        no_more(&server);
    }
}

#[test]
fn session_continue_sends_exactly_the_saved_native_request() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();

        // Save a session containing a completed write+read tool round trip
        // (and opaque reasoning for Responses).
        let cli = start_configured(
            api,
            &server,
            home.path(),
            project.path(),
            &[
                "--allowed-tools",
                "*,Task",
                "-p",
                "create and verify a file",
            ],
        );
        let (request, _) = api.receive(&server);
        let write_call = api.call(
            "w",
            "write",
            json!({"path":"marker.txt","content":"tool evidence"}),
        );
        let read_call = api.call("r", "read", json!({"path":"marker.txt"}));
        let batch = match api {
            Api::Chat => json!([write_call, read_call]),
            Api::Responses => json!([
                {
                    "type":"reasoning","id":"rs-session",
                    "summary":[{"type":"summary_text","text":"plan the writes"}],
                    "content":[{"type":"reasoning_text","text":"plan"}],
                    "encrypted_content":"opaque-session-reasoning","status":"completed"
                },
                write_call, read_call
            ]),
        };
        match api {
            Api::Chat => reply(
                request,
                json!({
                    "role":"assistant", "content":null,
                    "reasoning_content":"plan the writes", "tool_calls":batch
                }),
            ),
            Api::Responses => api.calls_reply(request, batch),
        }
        let (request, body) = api.receive(&server);
        assert_eq!(api.tool_result(&body, "w")["bytes_written"], 13);
        assert_eq!(api.tool_result(&body, "r")["content"], "tool evidence");
        api.final_reply(request, "file done");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"file done\n");
        assert_eq!(
            fs::read_to_string(project.path().join("marker.txt")).unwrap(),
            "tool evidence"
        );
        let id = only_session_id(home.path());
        let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
        let saved = records(&path);
        assert_session_contract(&saved, &id, project.path());
        let original_messages = canonical_messages(&saved);
        assert_eq!(
            original_messages
                .iter()
                .map(|message| message["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["user", "assistant", "toolResult", "toolResult", "assistant"]
        );
        let assistant = &original_messages[1];
        assert_eq!(
            assistant["api"],
            if api == Api::Chat {
                "openai-completions"
            } else {
                "openai-responses"
            }
        );
        assert_eq!(assistant["provider"], "fixture");
        assert_eq!(assistant["model"], "fixture-model");
        assert_eq!(assistant["stopReason"], "toolUse");
        let content = assistant["content"].as_array().unwrap();
        let thinking: Vec<_> = content
            .iter()
            .filter(|part| part["type"] == "thinking")
            .collect();
        if api == Api::Chat {
            assert_eq!(thinking.len(), 1);
            assert_eq!(thinking[0]["thinking"], "plan the writes");
            assert_eq!(thinking[0]["summary"], false);
        } else {
            assert_eq!(thinking.len(), 2);
            assert_eq!(thinking[0]["thinking"], "plan the writes");
            assert_eq!(thinking[0]["summary"], true);
            assert_eq!(thinking[1]["thinking"], "\n\nplan");
            assert_eq!(thinking[1]["summary"], false);
        }
        let calls: Vec<_> = content
            .iter()
            .filter(|part| part["type"] == "toolCall")
            .collect();
        assert_eq!(calls.len(), 2);
        for (index, (id, name, arguments)) in [
            (
                "w",
                "write",
                json!({"path":"marker.txt","content":"tool evidence"}),
            ),
            ("r", "read", json!({"path":"marker.txt"})),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                *calls[index],
                json!({"type":"toolCall","id":id,"name":name,"rawArguments":arguments.to_string()})
            );
            let tool = &original_messages[index + 2];
            assert_eq!(tool["toolCallId"], id);
            assert_eq!(tool["toolName"], name);
            assert_eq!(tool["isError"], false);
            let raw = tool["content"][0]["text"].as_str().unwrap();
            assert_eq!(tool["content"][0]["type"], "text");
            let details: Value = serde_json::from_str(raw).unwrap();
            assert!(tool.get("details").is_none());
            assert_eq!(details["ok"], true);
            if index == 0 {
                assert_eq!(details["bytes_written"], 13);
            } else {
                assert_eq!(details["content"], "tool evidence");
            }
        }
        assert_eq!(
            original_messages[4]["content"],
            json!([{"type":"text","text":"file done"}])
        );
        assert!(
            !serde_json::to_string(&original_messages)
                .unwrap()
                .contains("opaque-session-reasoning")
        );
        if api == Api::Responses {
            let opaque: Vec<_> = saved
                .iter()
                .filter(|record| record.to_string().contains("opaque-session-reasoning"))
                .collect();
            assert!(!opaque.is_empty());
            assert!(
                opaque
                    .iter()
                    .all(|record| record["customType"] == "pp.native_append")
            );
        }
        let rebuilt = rebuild_request(&saved);

        // Remove the tool's file: if restoring replayed old tool calls the
        // file would reappear before any new model answer.
        fs::remove_file(project.path().join("marker.txt")).unwrap();

        let mut cli = start_configured(api, &server, home.path(), launch.path(), &["-c", &id]);
        cli.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
        let (request, body) = api.receive(&server);
        // The continued request is the saved native request rebuilt from the
        // JSONL (instructions, think level, checkpoints and appended slices
        // included): no new user message, no "please continue" text, and the
        // last completed assistant output present.
        assert_eq!(
            body, rebuilt,
            "continued request must equal the saved native request"
        );
        let (users, assistants) = api.turns(&body);
        assert_eq!(users, ["create and verify a file"]);
        assert_eq!(assistants, ["file done"]);
        if api == Api::Responses {
            let input = body["input"].as_array().unwrap();
            let reasoning = input
                .iter()
                .find(|item| item["type"] == "reasoning")
                .expect("reasoning item must survive restore");
            assert_eq!(reasoning["encrypted_content"], "opaque-session-reasoning");
            assert_eq!(reasoning["id"], "rs-session");
            let call = input
                .iter()
                .find(|item| item["type"] == "function_call" && item["call_id"] == "w")
                .expect("function call must keep its call_id");
            assert_eq!(call["call_id"], "w");
            assert_eq!(body["store"], false);
            assert!(
                body["include"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| { value.as_str() == Some("reasoning.encrypted_content") })
            );
        }
        // Restoring executes no old tools.
        assert!(!project.path().join("marker.txt").exists());

        // A new tool call from the model does execute after restore, in the
        // saved working directory rather than the launch directory.
        api.calls_reply(
            request,
            json!([api.call(
                "w2",
                "write",
                json!({"path":"second.txt","content":"after restore"})
            )]),
        );
        let (request, body) = api.receive(&server);
        assert_eq!(api.tool_result(&body, "w2")["bytes_written"], 13);
        api.final_reply(request, "done after restore");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(project.path().join("second.txt")).unwrap(),
            "after restore"
        );
        assert!(!launch.path().join("second.txt").exists());
        assert!(!launch.path().join("marker.txt").exists());
        // Continue adds no user projection and never reprojects old native history.
        let continued = records(&path);
        assert_session_contract(&continued, &id, project.path());
        assert_eq!(&continued[..saved.len()], saved.as_slice());
        let messages = canonical_messages(&continued);
        assert_eq!(
            &messages[..original_messages.len()],
            original_messages.as_slice()
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "user")
                .count(),
            1
        );
        assert!(
            messages[original_messages.len()..]
                .iter()
                .all(|message| { !message["content"].to_string().contains("plan the writes") }),
            "thinking buffers must not leak into subsequent turns"
        );
        // Still the same session file: continuing never forks.
        assert_eq!(only_session_id(home.path()), id);
        no_more(&server);
    }
}

#[test]
fn session_resume_prompt_conflicts_and_reset_boundaries() {
    let server = server();

    // clap rejects -p over -c and -r together with -c; neither may silently
    // drop the resume arguments. Bare -r rejection is covered by the resume_cli cases.
    {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        configure(home.path(), fixture_settings(), &fixture_models(&server));
        let valid = "2026-10-17T01-02-03-00000000000000000000000000000000";
        for args in [vec!["-c", "-p", "text"], vec!["-r", valid, "-c"]] {
            let cli = start(home.path(), cwd.path(), &args);
            let output = cli.finish();
            assert!(
                !output.status.success(),
                "clap must reject {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            no_more(&server);
        }
        assert!(session_ids(home.path()).is_empty());
    }

    // -r with -p appends the new prompt once, keeping saved context.
    for api in APIS {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();
        let (id, _path) = make_saved_session(
            api,
            &server,
            home.path(),
            project.path(),
            "remember old",
            "old noted",
        );
        let cli = start_configured(
            api,
            &server,
            home.path(),
            launch.path(),
            &["-r", &id, "-p", "brand new"],
        );
        let (request, body) = api.receive(&server);
        let (users, assistants) = api.turns(&body);
        assert_eq!(users, ["remember old", "brand new"]);
        assert_eq!(assistants, ["old noted"]);
        api.final_reply(request, "new done");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"new done\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&format!("Resumed session: {id}")));
        assert_eq!(only_session_id(home.path()), id);
    }

    // /new on a persisted session discards the active journal without
    // creating an empty session; the next prompt starts a new ID, and the
    // old session remains resumable.
    {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let (old_id, _path) = make_saved_session(
            Api::Chat,
            &server,
            home.path(),
            project.path(),
            "first task",
            "first answer",
        );
        let mut cli = start_configured(Api::Chat, &server, home.path(), project.path(), &[]);
        cli.input("/new\n");
        cli.wait_for(Pipe::Err, "Session reset.");
        assert_eq!(
            session_ids(home.path()),
            vec![old_id.clone()],
            "/new must not create an empty session file"
        );
        cli.input("fresh start\n");
        let (request, body) = receive(&server);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "fresh start");
        assert!(!body.to_string().contains("first task"));
        final_reply(request, "fresh answer");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let ids = session_ids(home.path());
        assert_eq!(ids.len(), 2, "next prompt creates a new session ID");
        let new_id = ids.iter().find(|id| **id != old_id).unwrap();
        // The old session is untouched and still resumable.
        let cli = start_configured(
            Api::Chat,
            &server,
            home.path(),
            project.path(),
            &["-r", &old_id, "-p", "recall"],
        );
        let (request, body) = receive(&server);
        let (users, assistants) = Api::Chat.turns(&body);
        assert_eq!(users, ["first task", "recall"]);
        assert_eq!(assistants, ["first answer"]);
        assert!(!body.to_string().contains("fresh start"));
        final_reply(request, "recalled");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"recalled\n");
        assert_ne!(*new_id, old_id);
        no_more(&server);
    }
}
