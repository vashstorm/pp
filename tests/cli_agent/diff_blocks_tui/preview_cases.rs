fn diff_blocks_stream_delta(
    stream: &mut SseWriter,
    protocol: &str,
    name: &str,
    call_id: &str,
    item_id: &str,
    text: &str,
    first: bool,
) {
    if protocol == "completion" {
        let function = if first { json!({"name":name,"arguments":text}) } else { json!({"arguments":text}) };
        let mut call = json!({"index":0,"function":function});
        if first {
            call["id"] = json!(call_id);
            call["type"] = json!("function");
        }
        stream.event(&chat_chunk(json!({"tool_calls":[call]}), None)).unwrap();
    } else {
        if first {
            stream.event(&json!({"type":"response.output_item.added","sequence_number":0,"output_index":0,
                "item":{"type":"function_call","id":item_id,"call_id":call_id,"name":name,"arguments":""}})).unwrap();
        }
        stream.event(&json!({"type":"response.function_call_arguments.delta","sequence_number":1,
            "output_index":0,"item_id":item_id,"delta":text})).unwrap();
    }
}

#[test]
fn edit_preview_tui_streams_before_commit_both_protocols_and_edit_modes() {
    let protocol = "response";
        for mode in ["replace", "hashline"] {
            let provider = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            std::fs::write(config_path(home.path(), "settings.yml"), diff_blocks_settings(mode)).unwrap();
            std::fs::write(config_path(home.path(), "models.yml"), response_models(&provider, Some(protocol))).unwrap();
            let target = project.path().join("demo.txt");
            std::fs::write(&target, "old\nkeep\n").unwrap();
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready();
            pty.input("stream-edit-task\r");
            let path = api_path(protocol);
            let (mut request, _) = pty.request_at(&provider, path);
            let args = if mode == "replace" {
                json!({"path":"demo.txt","edits":[{"oldText":"old","newText":"typing-中-READY"}]})
            } else {
                diff_blocks_calls_reply(request, protocol, json!([diff_blocks_call(protocol, "read-anchor", "read", json!({"path":"demo.txt"}))]));
                let body;
                (request, body) = pty.request_at(&provider, path);
                let result = diff_blocks_tool_result(&body, protocol, "read-anchor");
                let header = result["content"].as_str().unwrap().lines().next().unwrap();
                json!({"input":format!("{header}\nPUT 1.=1:\n+typing-中-READY")})
            };
            let arguments = args.to_string();
            let split = arguments.find("-READY").unwrap();
            let end = split + "-READY".len();
            let mut stream = request.start_sse().unwrap();
            diff_blocks_stream_delta(
                &mut stream,
                protocol,
                "edit",
                "stream-edit",
                "fc-edit",
                &arguments[..split],
                true,
            );
            pty.wait_for("完整参数到达前显示编辑预览", |screen| {
                let text = screen.contents();
                text.contains("preview · not saved") && text.contains("typing-中") && !text.contains("READY")
            });
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\nkeep\n");
            pty.input("draft-preserved");
            pty.draft("draft-preserved");
            diff_blocks_stream_delta(
                &mut stream,
                protocol,
                "edit",
                "stream-edit",
                "fc-edit",
                &arguments[split..end],
                false,
            );
            pty.text("typing-中-READY");
            pty.draft("draft-preserved");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\nkeep\n");
            diff_blocks_stream_delta(
                &mut stream,
                protocol,
                "edit",
                "stream-edit",
                "fc-edit",
                &arguments[end..],
                false,
            );
            if protocol == "completion" {
                stream.event(&chat_chunk(json!({}), Some("tool_calls"))).unwrap();
                stream.frame("[DONE]").unwrap();
            } else {
                stream.event(&json!({"type":"response.completed","sequence_number":2,
                    "response":response_body(json!([response_call("stream-edit", "edit", args)]))})).unwrap();
            }
            stream.finish();
            let (request, body) = pty.request_at(&provider, path);
            assert_eq!(diff_blocks_tool_result(&body, protocol, "stream-edit")["ok"], true);
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "typing-中-READY\nkeep\n");
            pty.wait_for("临时预览替换为唯一真实提交卡片", |screen| {
                let text = screen.contents();
                text.contains("+1/-1") && !text.contains("not saved") && text.matches("Edit:").count() == 1
            });
            diff_blocks_final(request, protocol, "stream-edit-done");
            pty.text("stream-edit-done");
            pty.ready();
            pty.draft("draft-preserved");
            let (history, _) = diff_blocks_exit_scrollback(&mut pty, "stream-edit-done");
            assert!(!history.contains("not saved"), "{history}");
            pty.finish();
            for entry in std::fs::read_dir(resume_history_journal_dir(home.path())).unwrap() {
                let entry = entry.unwrap();
                if entry.path().extension().is_some_and(|extension| extension == "jsonl") {
                    let journal = std::fs::read_to_string(entry.path()).unwrap();
                    assert!(!journal.contains("tool_arguments_delta"));
                }
            }
            no_more(&provider);
        }
}

