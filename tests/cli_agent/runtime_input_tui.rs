// Real-binary runtime input tests; all state fences use terminal, HTTP, or files.
include!("runtime_input_queue_tui.rs");
include!("runtime_input_resume_tui.rs");

fn runtime_input_home(server: &Server, protocol: &str, settings_extra: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        format!(
            "default_model: fixture/primary\nnotification_timeout_seconds: 2\n{settings_extra}"
        ),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        response_models(server, Some(protocol)),
    )
    .unwrap();
    home
}

fn runtime_input_start(
    server: &Server,
    protocol: &str,
    settings_extra: &str,
) -> (tempfile::TempDir, Pty) {
    let home = runtime_input_home(server, protocol, settings_extra);
    let mut pty = Pty::start_with_args(home.path(), home.path(), &[]);
    pty.ready();
    (home, pty)
}

fn runtime_input_counts(pty: &mut Pty, steering: usize, queued: usize, paused: bool) {
    let expected = format!("Steering: {steering} · Queued: {queued}");
    pty.wait_for("pending input counts and pause state", |screen| {
        let text = screen.contents();
        text.contains(&expected) && text.contains("Paused") == paused
    });
}

fn runtime_input_final(request: Request, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

fn runtime_input_calls(request: Request, protocol: &str, calls: &[(&str, &str, Value)]) {
    if protocol == "completion" {
        calls_reply(
            request,
            Value::Array(
                calls
                    .iter()
                    .map(|(id, name, arguments)| call(id, name, arguments.clone()))
                    .collect(),
            ),
        );
    } else {
        response_reply(
            request,
            Value::Array(
                calls
                    .iter()
                    .map(|(id, name, arguments)| response_call(id, name, arguments.clone()))
                    .collect(),
            ),
        );
    }
}

fn runtime_input_live(pty: &mut Pty, server: &Server, protocol: &str, prompt: &str) -> SseReply {
    pty.input(&format!("{prompt}\r"));
    let (request, body) = pty.request_at(server, api_path(protocol));
    assert_eq!(
        cancel_user_messages(&body, protocol).last().unwrap(),
        prompt
    );
    let mut stream = SseReply::new(request);
    streaming_delta(&mut stream, protocol, "runtime-live-marker");
    pty.text("runtime-live-marker");
    stream
}

fn runtime_input_end_live(mut stream: SseReply, protocol: &str) {
    streaming_complete(&mut stream, protocol, "runtime-live-marker");
    stream.finish();
}

// bash 等 gate 的命令：先落 waiting marker（进入等待阶段的文件栅栏），
// 再轮询 gate 文件，释放后落 released marker（整批结束的文件栅栏）。
const RUNTIME_INPUT_GATE_COMMAND: &str = "printf waiting > bash-waiting; while [ ! -f gate.txt ]; do sleep 0.05; done; printf released > bash-released";

// 排队成功且流仍可见时，待处理计数出现在状态栏，且输入行已清空（❯ 行为空）。
fn runtime_input_wait_queued_with_empty_draft(pty: &mut Pty, visible_marker: &str) {
    pty.wait_for("queued steering shown once with cleared draft", |screen| {
        let text = screen.contents();
        text.contains("Steering: 1 · Queued: 0")
            && text.contains(visible_marker)
            && text.lines().all(|line| {
                !line.starts_with('❯') || line.trim_start_matches('❯').trim().is_empty()
            })
    });
}

// 断言 user 文本恰好出现一次，且位于全部指定 call_id 的 tool result 之后。
fn runtime_input_assert_user_after_outputs(body: &Value, protocol: &str, text: &str, ids: &[&str]) {
    let items = body[if protocol == "completion" {
        "messages"
    } else {
        "input"
    }]
    .as_array()
    .unwrap();
    let mut last_output = None;
    let mut user = None;
    let mut user_count = 0usize;
    for (index, item) in items.iter().enumerate() {
        let output_id = if protocol == "completion" {
            item["tool_call_id"].as_str()
        } else if item["type"] == "function_call_output" {
            item["call_id"].as_str()
        } else {
            None
        };
        if let Some(id) = output_id {
            assert!(ids.contains(&id), "unexpected tool output {id}");
            last_output = Some(index);
        }
        if item["role"] == "user" && slash_message_text(item) == text {
            user_count += 1;
            user = Some(index);
        }
    }
    assert_eq!(
        user_count, 1,
        "user message {text:?} must appear exactly once"
    );
    assert!(
        user.is_some_and(|turn| last_output.is_some_and(|output| turn > output)),
        "steering user message must come after the whole tool result batch"
    );
}

// 场景 1：运行中 Enter 补充进入队列，首轮结束后在同一任务/额度内继续，
// 模型真实执行 write 工具落盘 steering-proof.txt，最终只出现一个 Completed 活动。
#[test]
fn tui_runtime_input_steering_writes_file_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (home, mut pty) = runtime_input_start(&fixture, protocol, "");
        let path = api_path(protocol);
        let stream = runtime_input_live(&mut pty, &fixture, protocol, "task U");
        // SSE 仍打开且 delta 可见时提交 steering；草稿清空、计数恰好 1。
        pty.input("S\r");
        runtime_input_wait_queued_with_empty_draft(&mut pty, "runtime-live-marker");
        cancel_assert_no_request(&fixture);
        // 结束原流（无工具的最终回答）：完成边界原子领取 S，继续当前任务。
        runtime_input_end_live(stream, protocol);
        let (request, body) = pty.request_at(&fixture, path);
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["task U", "S"].map(str::to_string)
        );
        runtime_input_calls(
            request,
            protocol,
            &[(
                "steer-write",
                "write",
                json!({"path":"steering-proof.txt","content":"steering-ok\n"}),
            )],
        );
        // 工具结果提交后的下一请求：历史仍恰含 U、S 一次，且真实文件字节正确。
        let (request, body) = pty.request_at(&fixture, path);
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["task U", "S"].map(str::to_string)
        );
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].0, "steer-write");
        assert_eq!(outputs[0].1["ok"], json!(true));
        assert_eq!(
            std::fs::read_to_string(home.path().join("steering-proof.txt")).unwrap(),
            "steering-ok\n"
        );
        runtime_input_final(request, protocol, "steered answer done");
        pty.wait_for("steered task finished as exactly one activity", |screen| {
            let text = screen.contents();
            text.contains("steered answer done")
                && text.matches("Completed ·").count() == 1
                && !text.contains("Steering: ")
                && input_is_ready(screen)
        });
        cancel_assert_no_request(&fixture);
    }
}

// 场景 2：write A → bash 等 gate → write B 批次执行中提交 steering，
// 新请求只能在整批结果之后携带 user S；A/B 文件字节真实正确。
#[test]
fn tui_runtime_input_tool_batch_steers_only_after_full_results_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (home, mut pty) = runtime_input_start(&fixture, protocol, "");
        let path = api_path(protocol);
        pty.input("batch steer prompt\r");
        let (request, body) = pty.request_at(&fixture, path);
        assert_eq!(
            cancel_user_messages(&body, protocol).last().unwrap(),
            "batch steer prompt"
        );
        runtime_input_calls(
            request,
            protocol,
            &[
                ("batch-a", "write", json!({"path":"a.txt","content":"a"})),
                (
                    "batch-bash",
                    "bash",
                    json!({"command":RUNTIME_INPUT_GATE_COMMAND}),
                ),
                ("batch-b", "write", json!({"path":"b.txt","content":"b"})),
            ],
        );
        // bash 进入等待阶段的文件栅栏：此刻提交 steering，不得触发任何新请求。
        cancel_wait_pid_files(home.path(), &["bash-waiting"]);
        pty.input("S\r");
        runtime_input_counts(&mut pty, 1, 0, false);
        cancel_assert_no_request(&fixture);
        // 释放 gate：整批工具结束后才允许下一请求，S 排在全部结果之后。
        std::fs::write(home.path().join("gate.txt"), "go").unwrap();
        cancel_wait_pid_files(home.path(), &["bash-released"]);
        let (request, body) = pty.request_at(&fixture, path);
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["batch steer prompt", "S"].map(str::to_string)
        );
        let ids = ["batch-a", "batch-bash", "batch-b"];
        assert_eq!(cancel_called_ids(&body, protocol), ids);
        let outputs = cancel_tool_outputs(&body, protocol);
        assert_eq!(
            outputs
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ids
        );
        for (index, id) in ids.iter().enumerate() {
            assert_eq!(outputs[index].0, *id);
            assert_eq!(outputs[index].1["ok"], json!(true), "result for {id}");
        }
        runtime_input_assert_user_after_outputs(&body, protocol, "S", &ids);
        assert_eq!(
            std::fs::read_to_string(home.path().join("a.txt")).unwrap(),
            "a"
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("b.txt")).unwrap(),
            "b"
        );
        runtime_input_final(request, protocol, "batch steered final");
        pty.wait_for("batch task finished as exactly one activity", |screen| {
            let text = screen.contents();
            text.contains("batch steered final")
                && text.matches("Completed ·").count() == 1
                && !text.contains("Steering: ")
                && input_is_ready(screen)
        });
        cancel_assert_no_request(&fixture);
    }
}
