// /resume 集成回归：两种协议的推理与工具回放、原生历史续写、持久输入记忆不被
// replay 改写，以及跨目录加载历史时保留当前目录、状态栏、文件候选、模型和思考级别。
// 日志原地追加，旧工具不重复执行；通过真实终端、HTTP 和文件系统观察行为。

fn resume_history_journal_dir(home: &Path) -> std::path::PathBuf {
    home.join(".pp/agent/sessions")
}

fn resume_history_session_files(home: &Path) -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(resume_history_journal_dir(home))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    files
}

// Deterministic listing order without sleeping: fixed, distinct mtimes.
fn resume_history_touch(path: &Path, seconds: u64) {
    let time = std::time::UNIX_EPOCH + Duration::from_secs(seconds);
    let file = std::fs::File::options().read(true).open(path).unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(time)
            .set_accessed(time),
    )
    .unwrap();
}

// Sends one paging key and reports whether the visible screen changed, so
// callers can detect the top/bottom of the transcript without UI internals.
fn resume_history_page(pty: &mut Pty, key: &str) -> bool {
    let before = pty.parser.screen().contents();
    pty.input(key);
    let deadline = Instant::now() + Duration::from_millis(300);
    loop {
        pty.drain();
        if pty.parser.screen().contents() != before {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

// Pages the transcript from top to bottom and returns the markers sorted by
// their first on-screen (page, row, column) position, proving display order
// through the real terminal only.
fn resume_history_collect_order(pty: &mut Pty, markers: &[&str]) -> Vec<String> {
    for _ in 0..80 {
        if !resume_history_page(pty, "\x1b[5~") {
            break;
        }
    }
    let mut found: Vec<(String, usize, usize, usize)> = Vec::new();
    for page in 0..80 {
        let rows: Vec<String> = pty
            .parser
            .screen()
            .contents()
            .lines()
            .map(str::to_owned)
            .collect();
        for (row, line) in rows.iter().enumerate() {
            for marker in markers {
                if line.contains(marker) && !found.iter().any(|(seen, _, _, _)| seen == marker) {
                    let column = line.find(marker).unwrap();
                    found.push(((*marker).to_owned(), page, row, column));
                }
            }
        }
        if found.len() == markers.len() {
            break;
        }
        if !resume_history_page(pty, "\x1b[6~") {
            break;
        }
    }
    assert_eq!(
        found.len(),
        markers.len(),
        "transcript paging did not surface every marker; found {:?} of {:?}\n{}",
        found
            .iter()
            .map(|(marker, _, _, _)| marker.clone())
            .collect::<Vec<_>>(),
        markers,
        pty.diagnostics()
    );
    found.sort_by_key(|(_, page, row, column)| (*page, *row, *column));
    found.into_iter().map(|(marker, _, _, _)| marker).collect()
}

// Common fence for a successful interactive resume: green notification with
// the resumed id, replayed turns visible, welcome page gone.
fn resume_history_wait_resumed(pty: &mut Pty, id: &str, replayed: &[&str]) {
    pty.wait_for(
        "resumed session notification with replayed turns",
        |screen| {
            let text = screen.contents();
            notification_bounds(screen, "Note", vt100::Color::Rgb(127, 166, 106)).is_some()
                && text.contains("Resumed session:")
                && text.split_whitespace().collect::<String>().contains(id)
                && replayed.iter().all(|expected| text.contains(expected))
                && !text.contains("█▀▀█")
        },
    );
}

// 退出后读取运行时已提交的原生历史与最后保留的主屏幕。
fn resume_history_exit_scrollback(pty: &mut Pty, last_answer: &str) -> String {
    pty.input("\x03");
    pty.wait_for("退出后恢复输入模式", |screen| !screen.bracketed_paste());
    let (contents, _) = pty.native_history();
    assert!(contents.contains(last_answer), "{contents}");
    contents
}

// First reply of the saved session: visible reasoning plus a bash+read tool
// batch. The Responses variant carries opaque encrypted reasoning that must
// survive resume verbatim in later native requests.
fn resume_history_tool_batch(request: Request, protocol: &str, reasoning: &str, calls: Value) {
    if protocol == "completion" {
        reply(
            request,
            json!({
                "role": "assistant",
                "content": null,
                "reasoning_content": reasoning,
                "tool_calls": calls
            }),
        );
        return;
    }
    let batch = json!([{
        "type": "reasoning",
        "id": "rh-think",
        "status": "completed",
        "summary": [{"type": "summary_text", "text": reasoning}],
        "encrypted_content": "opaque-resume-history"
    }])
    .as_array()
    .unwrap()
    .iter()
    .cloned()
    .chain(calls.as_array().unwrap().iter().cloned())
    .collect::<Vec<_>>();
    let batch = Value::Array(batch);
    let mut stream = SseReply::new(request);
    stream.event(json!({
        "type": "response.reasoning_summary_text.delta", "sequence_number": 0,
        "item_id": "rh-think", "output_index": 0, "summary_index": 0, "delta": reasoning
    }));
    for (index, item) in batch.as_array().unwrap().iter().enumerate() {
        stream.event(json!({
            "type": "response.output_item.done", "sequence_number": index + 1,
            "output_index": index, "item": item
        }));
    }
    stream.event(json!({
        "type": "response.completed",
        "sequence_number": batch.as_array().unwrap().len() + 1,
        "response": response_body(batch)
    }));
    stream.finish();
}

#[test]
fn tui_slash_resume_history_replays_reasoning_tools_and_appends_native_context() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            r#"{"default_model":"fixture/primary","notification_timeout_seconds":1}"#,
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let path = api_path(protocol);
        let command = "printf 'resume-history evidence\\n' >> side-effect.txt; \
for i in {1..60}; do printf 'resume-history-long-%03d\\n' \"$i\"; done > long.txt; \
for i in {1..60}; do printf 'resume-history-bash-%03d\\n' \"$i\"; done";

        // Phase 1: create a full session in the project directory — user
        // turn, streamed reasoning, a bash+read batch with a long read
        // result, then two more text turns.
        {
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready();
            pty.input("resume-history U1 ask\r");
            let (request, _) = pty.request_at(&server, path);
            let calls = if protocol == "completion" {
                json!([
                    call("rh-b", "bash", json!({"command": command})),
                    call("rh-r", "read", json!({"path": "long.txt"}))
                ])
            } else {
                json!([
                    response_call("rh-b", "bash", json!({"command": command})),
                    response_call("rh-r", "read", json!({"path": "long.txt"}))
                ])
            };
            resume_history_tool_batch(request, protocol, "resume-history think", calls);
            let (request, body) = pty.request_at(&server, path);
            if protocol == "completion" {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(result(&messages[3], "rh-b")["exit_code"], 0);
                assert!(
                    result(&messages[4], "rh-r")["content"]
                        .as_str()
                        .unwrap()
                        .contains("resume-history-long-060")
                );
            } else {
                let input = body["input"].as_array().unwrap();
                assert_eq!(response_result(&input[4], "rh-b")["exit_code"], 0);
                assert!(
                    response_result(&input[5], "rh-r")["content"]
                        .as_str()
                        .unwrap()
                        .contains("resume-history-long-060")
                );
            }
            if protocol == "completion" {
                final_reply(request, "resume-history A1");
            } else {
                response_final(request, "resume-history A1");
            }
            pty.text("resume-history A1");
            pty.completed();
            pty.input("resume-history U2 ask\r");
            let (request, _) = pty.request_at(&server, path);
            if protocol == "completion" {
                final_reply(request, "resume-history A2");
            } else {
                response_final(request, "resume-history A2");
            }
            pty.text("resume-history A2");
            pty.completed();
            pty.input("/quit\r");
            pty.finish();
        }
        no_more(&server);
        let id = saved_session_id(home.path());
        let journal = resume_history_journal_dir(home.path()).join(format!("{id}.jsonl"));
        let before = std::fs::read(&journal).unwrap();
        let side_effect = project.path().join("side-effect.txt");
        assert_eq!(
            std::fs::read_to_string(&side_effect).unwrap(),
            "resume-history evidence\n"
        );

        // Phase 2: reopen interactively and resume through /resume.
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("/resume\r");
        pty.wait_for("session panel listing the saved summary", |screen| {
            let text = screen.contents();
            text.contains("Resume session")
                && text.contains("resume-history U1 ask")
                && text.lines().any(|line| line.starts_with('>'))
        });
        pty.input("\r");
        resume_history_wait_resumed(&mut pty, &id, &["resume-history A2"]);
        // Selecting and resuming a session performs no model request, and the
        // old bash tool side effect was not re-run.
        no_more(&server);
        assert_eq!(
            std::fs::read_to_string(&side_effect).unwrap(),
            "resume-history evidence\n"
        );

        // 恢复只替换可见 transcript；replay 不再写入输入回忆，因此这里不断言
        // ↑ 能取回旧会话的 prompt（持久回忆由独立输入历史回归覆盖）。

        pty.wait_for("resume notification fades before scrolling", |screen| {
            !screen.contents().contains("Resumed session:")
        });

        // The replayed transcript keeps the original order, observable by
        // paging the real terminal from top to bottom.
        let markers = [
            "resume-history U1 ask",
            "resume-history think",
            "$ printf",
            "Read · long.txt",
            "resume-history A1",
            "resume-history U2 ask",
            "resume-history A2",
        ];
        let ordered = resume_history_collect_order(&mut pty, &markers);
        assert_eq!(
            ordered, markers,
            "replayed turns are out of order for {protocol}"
        );

        // 工具输出恢复为折叠状态；Ctrl+O 全局展开和折叠，保留输入。
        for _ in 0..80 {
            pty.drain();
            if pty.parser.screen().contents().contains("$ printf") {
                break;
            }
            if !resume_history_page(&mut pty, "\x1b[5~") {
                break;
            }
        }
        pty.text("$ printf");
        assert!(
            !pty.parser
                .screen()
                .contents()
                .contains("resume-history-bash-001"),
            "the long bash result must replay folded\n{}",
            pty.diagnostics()
        );
        pty.input("\x0f");
        pty.text("Tool output expansion: enabled");
        // 展开会增加卡片高度；先浏览到开头，不依赖展开后的 viewport 位置。
        for _ in 0..80 {
            pty.drain();
            if pty.parser.screen().contents().contains("resume-history-bash-001") {
                break;
            }
            if !resume_history_page(&mut pty, "\x1b[5~") {
                break;
            }
        }
        pty.text("resume-history-bash-001");
        pty.input("\x0f");
        pty.wait_for("tool output folds again", |screen| {
            !screen.contents().contains("resume-history-bash-001")
        });

        // Return to the tail after inspecting older history; new submissions
        // intentionally do not reset a manually scrolled viewport.
        for _ in 0..80 {
            if !resume_history_page(&mut pty, "\x1b[6~") {
                break;
            }
        }

        // Native continuation: U3 carries the full original context — user
        // turns, call ids, tool results, and opaque Responses reasoning — in
        // the original order, with U3 last.
        pty.input("resume-history U3 ask\r");
        let (request, body) = pty.request_at(&server, path);
        if protocol == "completion" {
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 9, "{body}");
            slash_assert_message(&messages[1], "user", "resume-history U1 ask");
            let calls = messages[2]["tool_calls"].as_array().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0]["id"], "rh-b");
            assert_eq!(calls[0]["function"]["name"], "bash");
            assert_eq!(calls[1]["id"], "rh-r");
            assert_eq!(calls[1]["function"]["name"], "read");
            assert_eq!(result(&messages[3], "rh-b")["exit_code"], 0);
            assert!(
                result(&messages[4], "rh-r")["content"]
                    .as_str()
                    .unwrap()
                    .contains("resume-history-long-060")
            );
            slash_assert_message(&messages[5], "assistant", "resume-history A1");
            slash_assert_message(&messages[6], "user", "resume-history U2 ask");
            slash_assert_message(&messages[7], "assistant", "resume-history A2");
            slash_assert_message(&messages[8], "user", "resume-history U3 ask");
            final_reply(request, "resume-history A3");
        } else {
            let input = body["input"].as_array().unwrap();
            assert_eq!(input.len(), 10, "{body}");
            response_user(&input[0], "resume-history U1 ask");
            assert_eq!(input[1]["type"], "reasoning");
            assert_eq!(input[1]["id"], "rh-think");
            assert_eq!(input[1]["encrypted_content"], "opaque-resume-history");
            assert_eq!(input[1]["summary"][0]["text"], "resume-history think");
            assert_eq!(input[2]["type"], "function_call");
            assert_eq!(input[2]["call_id"], "rh-b");
            assert_eq!(input[2]["name"], "bash");
            assert_eq!(input[3]["type"], "function_call");
            assert_eq!(input[3]["call_id"], "rh-r");
            assert_eq!(input[3]["name"], "read");
            assert_eq!(input[4]["type"], "function_call_output");
            assert_eq!(input[5]["type"], "function_call_output");
            assert_eq!(response_result(&input[4], "rh-b")["exit_code"], 0);
            assert!(
                response_result(&input[5], "rh-r")["content"]
                    .as_str()
                    .unwrap()
                    .contains("resume-history-long-060")
            );
            slash_assert_message(&input[6], "assistant", "resume-history A1");
            response_user(&input[7], "resume-history U2 ask");
            slash_assert_message(&input[8], "assistant", "resume-history A2");
            response_user(&input[9], "resume-history U3 ask");
            assert_eq!(body["store"], false);
            assert!(body.get("previous_response_id").is_none());
            assert!(
                body["include"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item == "reasoning.encrypted_content")
            );
            response_final(request, "resume-history A3");
        }
        pty.text("resume-history A3");
        pty.ready();
        no_more(&server);

        // The exit scrollback carries the whole conversation exactly once and
        // in order, including the post-resume turn.
        let scrollback = resume_history_exit_scrollback(&mut pty, "resume-history A3");
        let mut previous = 0;
        for marker in [
            "resume-history U1 ask",
            "resume-history think",
            "$ printf",
            "Read · long.txt",
            "resume-history A1",
            "resume-history U2 ask",
            "resume-history A2",
            "resume-history U3 ask",
            "resume-history A3",
        ] {
            assert_eq!(
                scrollback.matches(marker).count(),
                1,
                "{marker} must appear exactly once in the exit scrollback:\n{scrollback}"
            );
            let at = scrollback.find(marker).unwrap();
            assert!(
                at > previous,
                "{marker} is out of order in the exit scrollback:\n{scrollback}"
            );
            previous = at;
        }
        pty.finish();

        // U3/A3 appended to the same journal in place: no fork, no new file,
        // and the old tool side effect still ran exactly once.
        let after = std::fs::read(&journal).unwrap();
        assert!(
            after.starts_with(&before),
            "journal must be appended in place for {protocol}"
        );
        assert!(String::from_utf8_lossy(&after).contains("resume-history U3 ask"));
        assert!(String::from_utf8_lossy(&after).contains("resume-history A3"));
        assert_eq!(resume_history_session_files(home.path()).len(), 1);
        assert_eq!(
            std::fs::read_to_string(&side_effect).unwrap(),
            "resume-history evidence\n"
        );
    }
}

#[test]
fn tui_slash_resume_history_keeps_current_cwd_status_files_model_and_effort() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("project-only.txt"), "project marker").unwrap();
    std::fs::write(launch.path().join("launch-only.txt"), "launch marker").unwrap();
    let models = json!({"providers": {
        "fixture": slash_provider(&server, "fixture", "response",
            json!({"primary": { "model": "vendor-primary"}})),
        "fixture2": slash_provider(&server, "fixture2", "completion",
            json!({"other": { "model": "vendor-other"}}))}})
    .to_string();
    std::fs::write(config_path(home.path(), "models.yml"), &models).unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        r#"{"default_model":"fixture/primary","think_level":"high","notification_timeout_seconds":1}"#,
    )
    .unwrap();

    // Save a session from the project directory with the response protocol,
    // a dedicated model, and thinking enabled.
    {
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready_effort("high");
        pty.input("resume-history cross U1\r");
        let (request, body) = pty.request_at(&server, "/v1/responses");
        assert_eq!(body["model"], "vendor-primary");
        assert_eq!(body["reasoning"]["effort"], "high");
        response_final(request, "resume-history cross A1");
        pty.text("resume-history cross A1");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
    }
    no_more(&server);
    let id = saved_session_id(home.path());
    let journal = resume_history_journal_dir(home.path()).join(format!("{id}.jsonl"));
    let before = std::fs::read(&journal).unwrap();

    // 删除旧 provider，当前模型改用另一协议；恢复不再依赖保存时的模型配置。
    std::fs::write(
        config_path(home.path(), "models.yml"),
        json!({"providers": {"fixture2": slash_provider(&server, "fixture2", "completion",
            json!({"other": { "model": "vendor-other"}}))}}).to_string(),
    ).unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        r#"{"notification_timeout_seconds":1}"#,
    )
    .unwrap();
    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready_model("fixture2/other");
    pty.ready_effort("high");
    pty.input("/effort none\r");
    pty.ready_effort("no think");

    pty.input("/resume\r");
    pty.text("No sessions in current folder.");
    pty.input("\x1b[C");
    pty.wait_for("panel lists the cross-directory session", |screen| {
        let text = screen.contents();
        text.contains("Resume session")
            && text.contains("resume-history cross U1")
            && text.lines().any(|line| line.starts_with('>'))
    });
    pty.input("\r");
    resume_history_wait_resumed(
        &mut pty,
        &id,
        &["resume-history cross U1", "resume-history cross A1"],
    );
    no_more(&server);

    // 状态栏保持当前目录、模型和思考级别。
    let launch_name = launch
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    pty.ready_model("fixture2/other");
    pty.ready_effort("no think");
    pty.wait_for(
        "状态栏保持当前目录",
        |screen| {
            screen
                .contents()
                .lines()
                .last()
                .is_some_and(|line| line.contains(&launch_name))
        },
    );

    // 文件候选仍来自当前目录。
    pty.input("@launch-only");
    pty.wait_for("当前目录的文件候选", |screen| {
        screen.contents().contains("launch-only.txt")
    });
    pty.input("\x01\x0b");
    pty.draft("");
    pty.input("@project-only");
    pty.draft("@project-only");
    pty.drain();
    assert!(
        !pty.parser.screen().contents().contains("project-only.txt"),
        "恢复历史不应切换文件候选目录\n{}",
        pty.diagnostics()
    );
    pty.input("\x01\x0b");
    pty.draft("");

    // 新请求使用当前模型、协议和思考级别，同时包含旧对话。
    pty.input("resume-history cross U2\r");
    let (request, body) = pty.request_at(&server, "/v1/chat/completions");
    assert_eq!(authorization(&request), "Bearer fixture2-key");
    assert_eq!(body["model"], "vendor-other");
    assert_eq!(body["reasoning_effort"], "none");
    let input = body["messages"].as_array().unwrap();
    assert_eq!(input.len(), 4, "{body}");
    assert!(input[0]["content"].as_str().unwrap().contains(launch.path().to_str().unwrap()));
    slash_assert_message(&input[1], "user", "resume-history cross U1");
    slash_assert_message(&input[2], "assistant", "resume-history cross A1");
    slash_assert_message(&input[3], "user", "resume-history cross U2");
    calls_reply(
        request,
        json!([
            call(
                "cw",
                "write",
                json!({"path": "second.txt", "content": "cross file after resume"})
            ),
            call("cr", "read", json!({"path": "second.txt"})),
            call(
                "cb",
                "bash",
                json!({"command": "printf 'cross-bash-evidence' > cross-bash.txt"})
            )
        ]),
    );
    let (request, body) = pty.request_at(&server, "/v1/chat/completions");
    let input = body["messages"].as_array().unwrap();
    assert_eq!(input.len(), 8, "{body}");
    assert_eq!(result(&input[5], "cw")["bytes_written"], 23);
    assert_eq!(
        result(&input[6], "cr")["content"],
        "cross file after resume"
    );
    assert_eq!(result(&input[7], "cb")["exit_code"], 0);
    final_reply(request, "resume-history cross A2");
    pty.text("resume-history cross A2");
    pty.ready();

    // 相对工具路径仍作用于当前目录。
    assert_eq!(
        std::fs::read_to_string(launch.path().join("second.txt")).unwrap(),
        "cross file after resume"
    );
    assert_eq!(
        std::fs::read_to_string(launch.path().join("cross-bash.txt")).unwrap(),
        "cross-bash-evidence"
    );
    assert!(!project.path().join("second.txt").exists());
    assert!(!project.path().join("cross-bash.txt").exists());
    assert_eq!(
        std::fs::read_to_string(launch.path().join("launch-only.txt")).unwrap(),
        "launch marker"
    );

    pty.input("/quit\r");
    pty.finish();
    no_more(&server);
    let after = std::fs::read(&journal).unwrap();
    assert!(
        after.starts_with(&before),
        "journal must be appended in place"
    );
    assert!(String::from_utf8_lossy(&after).contains("resume-history cross U2"));
    assert_eq!(resume_history_session_files(home.path()).len(), 1);
}

#[test]
fn tui_slash_resume_keeps_selected_model_across_protocols_and_reloads_converted_history() {
    for saved_protocol in ["completion", "response"] {
        for current_protocol in ["completion", "response"] {
            let server = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let settings = config_path(home.path(), "settings.yml");
            let models = config_path(home.path(), "models.yml");
            std::fs::write(&settings,
                r#"{"default_model":"fixture/primary","think_level":"high","edit":"hashline","notification_timeout_seconds":1}"#).unwrap();
            std::fs::write(&models, json!({"providers": {"fixture": slash_provider(
                &server, "fixture", saved_protocol,
                json!({"primary": {"model":"vendor-old"}})
            )}}).to_string()).unwrap();

            // 保存含工具调用和推理输出的历史，随后删除旧模型配置。
            let mut saved = Pty::start_with_args(project.path(), home.path(), &[]);
            saved.ready_model("fixture/primary");
            saved.input("matrix old prompt\r");
            let (request, _) = saved.request_at(&server, api_path(saved_protocol));
            let calls = if saved_protocol == "completion" {
                json!([call("matrix-call", "bash", json!({"command":"printf matrix-output"}))])
            } else {
                json!([response_call("matrix-call", "bash", json!({"command":"printf matrix-output"}))])
            };
            resume_history_tool_batch(request, saved_protocol, "matrix reasoning", calls);
            let (request, _) = saved.request_at(&server, api_path(saved_protocol));
            if saved_protocol == "completion" {
                final_reply(request, "matrix old answer");
            } else {
                response_final(request, "matrix old answer");
            }
            saved.text("matrix old answer");
            saved.completed();
            saved.input("/quit\r");
            saved.finish();
            let id = saved_session_id(home.path());
            let journal = resume_history_journal_dir(home.path()).join(format!("{id}.jsonl"));
            let original = std::fs::read(&journal).unwrap();

            let current_models = json!({"providers": {"fixture": slash_provider(
                &server, "fixture", current_protocol, json!({
                    "primary": {"model":"vendor-default"},
                    "selected": {"model":"vendor-current"}})
            )}}).to_string();
            std::fs::write(&models, &current_models).unwrap();
            std::fs::write(&settings,
                r#"{"edit":"replace","notification_timeout_seconds":1}"#).unwrap();
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready_model("fixture/primary");
            pty.input("/model fixture/selected\r");
            pty.ready_model("fixture/selected");
            pty.input("/effort low\r");
            pty.ready_effort("low");
            // 恢复不读取磁盘配置，也不能把手动选择的模型改回默认模型。
            std::fs::write(&models, "invalid configuration").unwrap();
            pty.input("/resume\r");
            pty.text("matrix old prompt");
            pty.input("\r");
            resume_history_wait_resumed(&mut pty, &id, &["matrix old answer"]);
            pty.ready_model("fixture/selected");
            pty.ready_effort("low");
            assert_eq!(std::fs::read(&journal).unwrap(), original);
            no_more(&server);

            pty.input("matrix new prompt\r");
            let (request, body) = pty.request_at(&server, api_path(current_protocol));
            assert_eq!(body["model"], "vendor-current");
            if current_protocol == "completion" {
                assert_eq!(body["reasoning_effort"], "low");
            } else {
                assert_eq!(body["reasoning"]["effort"], "low");
            }
            let text = body.to_string();
            for expected in ["matrix old prompt", "matrix old answer", "matrix-call", "matrix-output", "matrix new prompt", "oldText"] {
                assert!(text.contains(expected), "{saved_protocol} -> {current_protocol}: {body}");
            }
            assert!(!text.contains("opaque-resume-history"), "{body}");
            if current_protocol == "completion" {
                final_reply(request, "matrix new answer");
            } else {
                response_final(request, "matrix new answer");
            }
            pty.text("matrix new answer");
            pty.completed();
            pty.input("/quit\r");
            pty.finish();

            // 重启后再次加载，验证转换后的完整状态和后续追加没有协议或游标错位。
            std::fs::write(&models, &current_models).unwrap();
            let mut reloaded = Pty::start_with_args(project.path(), home.path(), &["--resume", &id]);
            reloaded.ready_model("fixture/selected");
            reloaded.input("matrix third prompt\r");
            let (request, body) = reloaded.request_at(&server, api_path(current_protocol));
            for expected in ["matrix old prompt", "matrix old answer", "matrix-output", "matrix new prompt", "matrix new answer", "matrix third prompt"] {
                assert!(body.to_string().contains(expected), "{body}");
            }
            if current_protocol == "completion" {
                final_reply(request, "matrix third answer");
            } else {
                response_final(request, "matrix third answer");
            }
            reloaded.text("matrix third answer");
            reloaded.ready();
            reloaded.input("/quit\r");
            reloaded.finish();
            no_more(&server);
            assert_eq!(resume_history_session_files(home.path()).len(), 1);
        }
    }
}

#[test]
fn tui_slash_resume_history_smoke_selects_older_session_and_continues() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        r#"{"default_model":"fixture/primary","notification_timeout_seconds":1}"#,
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    let alpha = tempfile::tempdir().unwrap();
    let beta = tempfile::tempdir().unwrap();

    // Two real sessions in different directories; the journals get fixed,
    // distinct mtimes so the panel order is deterministic without sleeping.
    for (dir, ask, answer) in [
        (&alpha, "resume-history alpha U1", "resume-history alpha A1"),
        (&beta, "resume-history beta U1", "resume-history beta A1"),
    ] {
        let mut pty = Pty::start_with_args(dir.path(), home.path(), &[]);
        pty.ready();
        pty.input(format!("{ask}\r").as_str());
        let (request, body) = pty.request(&server);
        println!("[smoke] creation request from {:?}: {body}", dir.path());
        final_reply(request, answer);
        pty.text(answer);
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
    }
    no_more(&server);

    let files = resume_history_session_files(home.path());
    assert_eq!(files.len(), 2, "expected exactly two saved sessions");
    let alpha_journal = files
        .iter()
        .find(|path| {
            std::fs::read_to_string(path)
                .unwrap()
                .contains("resume-history alpha U1")
        })
        .unwrap()
        .clone();
    let beta_journal = files
        .iter()
        .find(|path| {
            std::fs::read_to_string(path)
                .unwrap()
                .contains("resume-history beta U1")
        })
        .unwrap()
        .clone();
    let alpha_id = alpha_journal
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    resume_history_touch(&alpha_journal, 1_600_000_000);
    resume_history_touch(&beta_journal, 1_600_000_010);
    let alpha_before = std::fs::read(&alpha_journal).unwrap();
    let beta_before = std::fs::read(&beta_journal).unwrap();

    // Third launch from yet another directory: list, walk down to the older
    // session, resume it, continue it, and exit.
    let launch = tempfile::tempdir().unwrap();
    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    pty.input("/resume\r");
    pty.text("No sessions in current folder.");
    pty.input("\x1b[C");
    pty.wait_for("panel lists both sessions newest-first", |screen| {
        let text = screen.contents();
        text.contains("Resume session")
            && text.contains("resume-history alpha U1")
            && text.contains("resume-history beta U1")
            && text.lines().any(|line| line.starts_with('>'))
            && text
                .find("resume-history beta U1")
                .zip(text.find("resume-history alpha U1"))
                .is_some_and(|(beta_at, alpha_at)| beta_at < alpha_at)
    });
    println!("[smoke] session panel:\n{}", pty.parser.screen().contents());
    pty.input("\x1b[B");
    pty.input("\r");
    resume_history_wait_resumed(
        &mut pty,
        &alpha_id,
        &["resume-history alpha U1", "resume-history alpha A1"],
    );
    pty.wait_for("beta session is not the resumed one", |screen| {
        !screen.contents().contains("resume-history beta U1")
    });
    println!(
        "[smoke] screen after selecting the older session:\n{}",
        pty.parser.screen().contents()
    );
    no_more(&server);

    // 恢复只替换历史，状态栏仍显示当前工作目录。
    let launch_name = launch
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    pty.wait_for("状态栏保持当前目录", |screen| {
        screen
            .contents()
            .lines()
            .last()
            .is_some_and(|line| line.contains(&launch_name))
    });

    pty.input("resume-history alpha U3\r");
    let (request, body) = pty.request(&server);
    println!("[smoke] continuation request after /resume: {body}");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4, "{body}");
    slash_assert_message(&messages[1], "user", "resume-history alpha U1");
    slash_assert_message(&messages[2], "assistant", "resume-history alpha A1");
    slash_assert_message(&messages[3], "user", "resume-history alpha U3");
    final_reply(request, "resume-history alpha A3");
    pty.text("resume-history alpha A3");
    pty.ready();
    no_more(&server);

    let scrollback = resume_history_exit_scrollback(&mut pty, "resume-history alpha A3");
    println!("[smoke] exit scrollback:\n{scrollback}");
    for marker in [
        "resume-history alpha U1",
        "resume-history alpha A1",
        "resume-history alpha U3",
        "resume-history alpha A3",
    ] {
        assert_eq!(
            scrollback.matches(marker).count(),
            1,
            "{marker}\n{scrollback}"
        );
    }
    assert!(!scrollback.contains("resume-history beta"));
    pty.finish();

    // Evidence: the selected journal is appended in place, the unselected
    // journal is untouched, and no forked session exists.
    let alpha_after = std::fs::read(&alpha_journal).unwrap();
    let beta_after = std::fs::read(&beta_journal).unwrap();
    assert!(alpha_after.starts_with(&alpha_before));
    println!(
        "[smoke] selected journal {} grew from {} to {} bytes; appended records:\n{}",
        alpha_journal.display(),
        alpha_before.len(),
        alpha_after.len(),
        String::from_utf8_lossy(&alpha_after[alpha_before.len()..])
    );
    assert!(String::from_utf8_lossy(&alpha_after).contains("resume-history alpha U3"));
    assert!(String::from_utf8_lossy(&alpha_after).contains("resume-history alpha A3"));
    assert_eq!(
        beta_after, beta_before,
        "unselected journal must stay untouched"
    );
    assert_eq!(resume_history_session_files(home.path()).len(), 2);
}

// /resume 的检索来自 history.db 派生索引：只有第二条 user 正文命中的关键词也能
// 列出会话；2 字中文与长词分别走字面 instr 与 trigram FTS；范围切换、关闭重开
// 和持锁删除都在真实终端上验证，检索/恢复不发送模型请求。
#[test]
fn tui_slash_resume_searches_full_prompt_history_and_blocks_locked_delete() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let saved = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        r#"{"default_model":"fixture/primary","notification_timeout_seconds":1}"#,
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();

    // 标题/预览只包含首条 user；检索词只出现在第二条 user 正文。
    let id = {
        let mut pty = Pty::start_with_args(saved.path(), home.path(), &[]);
        pty.ready();
        pty.input("resume-search first title only\r");
        let (request, _) = pty.request(&server);
        final_reply(request, "resume-search first answer");
        pty.text("resume-search first answer");
        pty.completed();
        pty.input("修复模型切换失败 needle_42\r");
        let (request, _) = pty.request(&server);
        final_reply(request, "resume-search second answer");
        pty.text("resume-search second answer");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        saved_session_id(home.path())
    };
    no_more(&server);
    let journal = resume_history_journal_dir(home.path()).join(format!("{id}.jsonl"));
    let before = std::fs::read(&journal).unwrap();

    // 另一进程持锁：会话可被搜索，但删除必须被拒绝且保留行与文件。
    let lock_dir = home.path().join(".pp/run/sessions");
    std::fs::create_dir_all(&lock_dir).unwrap();
    let lock = std::fs::File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_dir.join(format!("{id}.lock")))
        .unwrap();
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();

    let mut pty = Pty::start_with_args(launch.path(), home.path(), &[]);
    pty.ready();
    pty.input("/resume\r");
    pty.text("No sessions in current folder.");
    pty.input("\x1b[C");
    pty.wait_for("all projects lists the saved session", |screen| {
        let text = screen.contents();
        text.contains("Resume session")
            && text.contains("resume-search first title only")
            && text.lines().any(|line| line.starts_with('>'))
    });

    // 2 字中文走字面 instr；继续输入到 3 字走 trigram FTS；结果都来自第二条 prompt。
    // 同时等待查询行，确保整串输入已处理且最新请求已完成（pending 时不显示结果）。
    pty.input("模型");
    pty.wait_for("two-character query hits the second prompt", |screen| {
        let text = screen.contents();
        text.contains("> 模型") && text.contains("resume-search first title only")
    });
    pty.input("切");
    pty.wait_for("three-character query still hits", |screen| {
        let text = screen.contents();
        text.contains("> 模型切") && text.contains("resume-search first title only")
    });
    pty.input("\x15");
    pty.input("needle_42");
    pty.wait_for("full-text needle hits the second prompt", |screen| {
        let text = screen.contents();
        text.contains("> needle_42")
            && text.contains("resume-search first title only")
            && !text.contains("No matching sessions.")
    });

    // 关闭重开：新面板从当前目录隔离开始；范围切换重跑同一查询并恢复结果。
    pty.input("\x1b");
    pty.wait_for("escape returns to the main screen", |screen| {
        !screen.contents().contains("Resume session")
    });
    pty.input("/resume\r");
    pty.text("No sessions in current folder.");
    pty.input("needle_42");
    pty.wait_for("current folder query finds nothing", |screen| {
        let text = screen.contents();
        text.contains("> needle_42") && text.contains("No matching sessions.")
    });
    pty.input("\x1b[C");
    pty.wait_for("scope change reruns the query", |screen| {
        screen.contents().contains("resume-search first title only")
    });

    // 持锁会话：Delete 确认后删除失败，错误可见且行仍在。
    pty.input("\x1b[3~");
    pty.wait_for("delete confirmation", |screen| {
        screen.contents().contains("Delete selected session?")
    });
    pty.input("y");
    pty.wait_for("locked delete rejected", |screen| {
        let text = screen.contents();
        text.contains("Failed to delete session")
            && text.contains("resume-search first title only")
    });
    assert!(journal.exists());
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    no_more(&server);

    // 释放锁后从搜索结果恢复：transcript 显示第二条 prompt，且不发送模型请求。
    drop(lock);
    pty.input("\x1b");
    pty.wait_for("panel closes after the failed delete", |screen| {
        !screen.contents().contains("Resume session")
    });
    pty.input("/resume\r");
    pty.input("\x1b[C");
    pty.wait_for("session listed again", |screen| {
        screen.contents().contains("resume-search first title only")
    });
    pty.input("\r");
    resume_history_wait_resumed(
        &mut pty,
        &id,
        &["resume-search second answer", "needle_42"],
    );
    no_more(&server);
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    pty.input("/quit\r");
    pty.finish();
}
