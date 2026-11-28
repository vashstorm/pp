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
        let mut stream = request.start_sse().unwrap();
        match api {
            Api::Chat => {
                stream
                    .event(&chat_chunk(
                        json!({"role":"assistant","content":"partial fragment"}),
                        None,
                    ))
                    .unwrap();
                stream
                    .event(&json!({"error":{
                        "message":"private-provider-body fixture-key",
                        "type":"server_error","code":"private-provider-body"
                    }}))
                    .unwrap();
                stream.finish();
            }
            Api::Responses => {
                stream
                    .event(&json!({
                        "type":"response.output_text.delta","sequence_number":0,
                        "item_id":"msg-x","output_index":0,"content_index":0,
                        "delta":"partial fragment","logprobs":[]
                    }))
                    .unwrap();
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
        // The failure prefix only writes the display snapshot and never enters later native requests.
        assert!(file_text.contains("partial fragment"));
        let saved = records(&path);
        assert_session_contract(&saved, &id, project.path());
        assert!(canonical_messages(&saved).iter().any(|message| {
            matches!(message["stopReason"].as_str(), Some("error" | "aborted"))
                && message["content"].to_string().contains("partial fragment")
        }));
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
