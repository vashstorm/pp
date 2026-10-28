use super::*;

#[test]
fn tui_session_resume_replays_history_without_request() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let id = create_tool_session(&server, home.path(), protocol);
        let path = api_path(protocol);

        let mut pty = Pty::start_with_args(home.path(), home.path(), &["-r", &id]);
        pty.wait_for("恢复的对话与绿色恢复提示弹窗", |screen| {
            let text = screen.contents();
            text.contains("remember tui-marker")
                && text.contains("tui-remembered")
                && notification_bounds(screen, "Note", vt100::Color::Rgb(127, 166, 106)).is_some()
                && text.contains("Resumed session:")
                && text.split_whitespace().collect::<String>().contains(&id)
        });
        // 无欢迎页；工具卡片重放保存的退出状态，不伪造 wall duration。
        let text = pty.parser.screen().contents();
        assert!(!text.contains("▄▄▄██▀▀██"));
        assert!(text.contains("$ printf"));
        assert!(text.contains("Read · tui-read.txt"));
        assert!(text.contains("tool-out"));
        assert!(text.contains("Exit code: 0"));
        assert!(!text.contains("Wall:"));
        // 输入框为空且未处于历史浏览状态。
        pty.ready();
        let prompt_line = pty
            .parser
            .screen()
            .contents()
            .lines()
            .find(|line| line.starts_with('❯'))
            .unwrap()
            .trim()
            .to_owned();
        assert_eq!(prompt_line, "❯");
        // 恢复不发送任何请求。
        no_more(&server);
        // input history 已恢复：↑ 直接回填旧输入。
        pty.input("\x1b[A");
        pty.draft("remember tui-marker");
        pty.input("\x1b[B");
        pty.draft("");

        // 新输入沿用原上下文，追加到原文件且 ID 不变。
        pty.input("what did I say\r");
        let (request, body) = pty.request_at(&server, path);
        if protocol == "completion" {
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 7);
            assert_eq!(messages[1]["content"], "remember tui-marker");
            assert_eq!(messages[2]["tool_calls"][0]["function"]["name"], "bash");
            assert_eq!(messages[2]["tool_calls"][1]["function"]["name"], "read");
            assert_eq!(messages[3]["role"], "tool");
            assert_eq!(messages[4]["role"], "tool");
            assert_eq!(messages[5]["content"], "tui-remembered");
            assert_eq!(messages[6]["content"], "what did I say");
        } else {
            let input = body["input"].as_array().unwrap();
            assert_eq!(input.len(), 7);
            response_user(&input[0], "remember tui-marker");
            assert_eq!(input[1]["type"], "function_call");
            assert_eq!(input[1]["call_id"], "b1");
            assert_eq!(input[2]["type"], "function_call");
            assert_eq!(input[2]["call_id"], "r1");
            assert_eq!(input[3]["type"], "function_call_output");
            assert_eq!(input[4]["type"], "function_call_output");
            slash_assert_message(&input[5], "assistant", "tui-remembered");
            response_user(&input[6], "what did I say");
        }
        if protocol == "completion" {
            final_reply(request, "you said tui-marker");
        } else {
            response_final(request, "you said tui-marker");
        }
        pty.text("you said tui-marker");
        pty.ready();
        // 已恢复的 session 不再重复提示新建 Session ID。
        assert!(!pty.parser.screen().contents().contains("Session:"));
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
        assert_eq!(saved_session_id(home.path()), id);
        let journal = std::fs::read_to_string(
            home.path()
                .join(".pp/agent/sessions")
                .join(format!("{id}.jsonl")),
        )
        .unwrap();
        assert!(journal.contains("what did I say"));

        // A separately isolated crash keeps streamed display text, but no
        // partial assistant turn may become native request history.
        let interrupted_home = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(interrupted_home.path(), "settings.yml"),
            fixture_settings(),
        )
        .unwrap();
        std::fs::write(
            config_path(interrupted_home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let mut interrupted = Pty::start_with_existing_files(interrupted_home.path());
        interrupted.ready();
        interrupted.input("interrupted input\r");
        let (request, _) = receive_at(&server, path);
        let mut stream = SseReply::new(request);
        streaming_delta(&mut stream, protocol, "partial-session-marker");
        // Observed terminal output fences event persistence; do not finish
        // the stream before killing the process.
        interrupted.text("partial-session-marker");
        drop(interrupted);
        drop(stream);
        let interrupted_id = saved_session_id(interrupted_home.path());
        let interrupted_path = interrupted_home
            .path()
            .join(".pp/agent/sessions")
            .join(format!("{interrupted_id}.jsonl"));
        let before = std::fs::read(&interrupted_path).unwrap();
        let saved: Vec<Value> = String::from_utf8(before.clone())
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!saved.iter().any(|entry| {
            entry["type"] == "message" && entry["message"]["role"] == "assistant"
        }));
        assert!(!saved.iter().any(|entry| {
            entry["type"] == "custom" && entry["customType"] == "pp.native_append"
        }));
        let mut resumed = Pty::start_with_args(
            interrupted_home.path(),
            interrupted_home.path(),
            &["-r", &interrupted_id],
        );
        resumed.text("partial-session-marker");
        resumed.wait_for("恢复中断会话显示红色弹窗", |screen| {
            notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
                && screen
                    .contents()
                    .contains("Previous request was interrupted.")
        });
        resumed.ready();
        let screen = resumed.parser.screen().contents();
        assert_eq!(screen.matches("interrupted input").count(), 1);
        assert_eq!(screen.matches("partial-session-marker").count(), 1);
        no_more(&server);
        resumed.input("/quit\r");
        resumed.finish();
        assert_eq!(std::fs::read(&interrupted_path).unwrap(), before);
        no_more(&server);
    }
}

#[test]
fn tui_session_continue_sends_saved_request_without_keypress() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let id = create_tool_session(&server, home.path(), protocol);
        let path = api_path(protocol);

        let mut pty = Pty::start_with_args(home.path(), home.path(), &["-c", &id]);
        // 不按任何键即发出请求；先看到恢复的对话再断言请求体。
        pty.wait_for("恢复的对话", |screen| {
            screen.contents().contains("remember tui-marker")
        });
        let (request, body) = pty.request_at(&server, path);
        if protocol == "completion" {
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 6);
            assert_eq!(messages.iter().filter(|m| m["role"] == "user").count(), 1);
            assert_eq!(messages[2]["tool_calls"].as_array().unwrap().len(), 2);
            assert_eq!(messages.last().unwrap()["role"], "assistant");
            assert_eq!(messages.last().unwrap()["content"], "tui-remembered");
        } else {
            let input = body["input"].as_array().unwrap();
            assert_eq!(input.len(), 6);
            assert_eq!(
                input.iter().filter(|item| item["role"] == "user").count(),
                1
            );
            assert_eq!(
                input
                    .iter()
                    .filter(|item| item["type"] == "function_call")
                    .count(),
                2
            );
            slash_assert_message(input.last().unwrap(), "assistant", "tui-remembered");
        }
        let body = serde_json::to_string(&body).unwrap();
        assert!(!body.contains("please continue"));
        assert!(!body.contains("请继续"));
        // You 卡片不增加：旧输入只出现一次。
        assert_eq!(
            pty.parser
                .screen()
                .contents()
                .matches("remember tui-marker")
                .count(),
            1
        );
        if protocol == "completion" {
            final_reply(request, "continued-answer");
        } else {
            response_final(request, "continued-answer");
        }
        pty.text("continued-answer");
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&server);
        assert_eq!(saved_session_id(home.path()), id);
    }
}

#[test]
fn tui_session_effort_write_failure_keeps_status_and_shows_error() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let id = create_tool_session(&server, home.path(), "completion");
    let path = home
        .path()
        .join(".pp/agent/sessions")
        .join(format!("{id}.jsonl"));
    // 用一条展示记录把日志补齐到恰好等于 RLIMIT_FSIZE，使首次 /effort 追加得到 EFBIG。
    let len = std::fs::metadata(&path).unwrap().len();
    let limit = (len + 512).div_ceil(1024) * 1024;
    let journal = std::fs::read_to_string(&path).unwrap();
    let last: Value = serde_json::from_str(journal.lines().last().unwrap()).unwrap();
    let mut padding_entry = json!({
        "type":"message", "id":"padding-entry", "parentId":last["id"],
        "timestamp":last["timestamp"],
        "message":{"role":"user", "content":"", "timestamp":1}
    });
    let overhead = serde_json::to_vec(&padding_entry).unwrap().len() as u64 + 1;
    padding_entry["message"]["content"] = json!("p".repeat((limit - len - overhead) as usize));
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{padding_entry}").unwrap();
    }
    assert_eq!(std::fs::metadata(&path).unwrap().len(), limit);

    let mut pty = Pty::start_with_file_limit(home.path(), home.path(), limit / 1024, &["-r", &id]);
    pty.ready();
    pty.ready_effort("no think");
    pty.input("/effort high\r");
    pty.text("persisting session");
    pty.ready();
    pty.ready_effort("no think");
    assert!(pty.parser.screen().contents().contains(" off ·"));
    // 设置写失败不触发任何模型请求。
    no_more(&server);
    pty.input("/quit\r");
    pty.finish();
}
