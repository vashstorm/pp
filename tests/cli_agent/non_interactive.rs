use super::*;
use axum::{body::Body, response::Response};
use std::collections::BTreeSet;

fn tool_names(body: &Value, protocol: &str) -> BTreeSet<String> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|tool| {
            if protocol == "completion" {
                tool["function"]["name"].as_str().unwrap().to_owned()
            } else {
                tool["name"].as_str().unwrap().to_owned()
            }
        })
        .collect()
}

fn read_tool_description(body: &Value, protocol: &str) -> String {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|tool| {
            let entry = if protocol == "completion" {
                &tool["function"]
            } else {
                tool
            };
            (entry["name"] == "read").then(|| entry["description"].as_str().unwrap().to_owned())
        })
        .unwrap()
}

#[test]
fn non_interactive_defaults_to_read_grep_glob_and_disables_subagents() {
    let protocol = "response";
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "hello world\n").unwrap();
    let models = if protocol == "completion" {
        fixture_models(&server)
    } else {
        response_models(&server, Some("response"))
    };
    write_settings(dir.path(), task_fixture_settings()).unwrap();
    std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();

    let cli = Cli::spawn(dir.path(), &["-p", "inspect workspace"], &[]);
    let path = if protocol == "completion" {
        "/v1/chat/completions"
    } else {
        "/v1/responses"
    };
    let (request, body) = receive_at(&server, path);
    assert_eq!(
        tool_names(&body, protocol),
        BTreeSet::from(["read".into(), "rg".into(), "glob".into()])
    );
    assert!(!read_tool_description(&body, protocol).contains("agent://"));

    // 模型尝试调用未授权写工具/Bash/Subagent 时，执行层硬门禁拒绝且不创建 .subagents。
    if protocol == "completion" {
        calls_reply(
            request,
            json!([
                call("c-read", "read", json!({"path": "note.txt"})),
                call("c-bash", "bash", json!({"command": "touch forbidden"})),
                call(
                    "c-task",
                    "task",
                    json!({"context": "x", "tasks": [{"role": "fast", "task": "y"}]})
                )
            ]),
        );
    } else {
        response_reply(
            request,
            json!([
                response_call("c-read", "read", json!({"path": "note.txt"})),
                response_call("c-bash", "bash", json!({"command": "touch forbidden"})),
                response_call(
                    "c-task",
                    "task",
                    json!({"context": "x", "tasks": [{"role": "fast", "task": "y"}]})
                )
            ]),
        );
    }

    let (request, body) = receive_at(&server, path);
    let extract = |id: &str| -> Value {
        if protocol == "completion" {
            let msg = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
                .unwrap();
            result(msg, id)
        } else {
            let item = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|m| m["type"] == "function_call_output" && m["call_id"] == id)
                .unwrap();
            response_result(item, id)
        }
    };
    assert_eq!(extract("c-read")["ok"], true);
    for blocked in ["c-bash", "c-task"] {
        let res = extract(blocked);
        assert_eq!(res["ok"], false, "{res}");
        assert!(
            res["error"]
                .as_str()
                .unwrap()
                .contains("outside this agent capability set"),
            "{res}"
        );
    }
    if protocol == "completion" {
        final_reply(request, "read only ok");
    } else {
        response_final(request, "read only ok");
    }
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"read only ok\n");
    assert!(!dir.path().join("forbidden").exists());
    let has_subagents = std::fs::read_dir(config_path(dir.path(), "sessions"))
        .unwrap()
        .filter_map(Result::ok)
        .any(|e| e.path().extension().is_some_and(|x| x == "subagents"));
    assert!(!has_subagents);
    no_more(&server);
}

#[test]
fn allowed_tools_preapproves_in_always_ask_and_gates_subagent_via_task() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("target.txt"), "before\n").unwrap();
    let settings = task_fixture_settings_with_tools("  permission:\n    write: deny\n");
    write_settings(dir.path(), &settings).unwrap();
    std::fs::write(
        config_path(dir.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();

    // 1. `--allowed_tools "Read,Grep,Edit,Bash"` 在 always-ask 下免审批放行 edit 与 bash，
    //    但仍拦截 critical bash 命令，且不暴露 task。
    let cli = Cli::spawn(
        dir.path(),
        &[
            "-p",
            "--allowed_tools",
            "Read,Grep,Edit,Bash",
            "edit and run bash",
        ],
        &[],
    );
    let (request, body) = receive(&server);
    assert_eq!(
        tool_names(&body, "completion"),
        BTreeSet::from([
            "read".into(),
            "rg".into(),
            "edit".into(),
            "bash".into(),
            "bash_task".into(),
            "wait".into(),
        ])
    );
    calls_reply(
        request,
        json!([
            call("r", "read", json!({"path": "target.txt"})),
            call(
                "e",
                "edit",
                json!({"path": "target.txt", "edits": [{"oldText": "before", "newText": "after"}]})
            ),
            call("b-ok", "bash", json!({"command": "printf ok"})),
            call("b-crit", "bash", json!({"command": "rm -rf /"})),
        ]),
    );
    let (request, body) = receive(&server);
    let messages = body["messages"].as_array().unwrap();
    let find_res = |id: &str| {
        result(
            messages
                .iter()
                .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
                .unwrap(),
            id,
        )
    };
    assert_eq!(find_res("e")["ok"], true);
    assert_eq!(find_res("b-ok")["ok"], true);
    assert_eq!(find_res("b-ok")["output"], "ok");
    assert_eq!(find_res("b-crit")["ok"], false);
    assert!(
        find_res("b-crit")["error"]
            .as_str()
            .unwrap()
            .contains("no interactive UI")
    );
    final_reply(request, "option b verified");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("target.txt")).unwrap(),
        "after\n"
    );

    // 2. 显式 `tools.permission: { write: deny }` 仍拦截即使列入 `--allowed-tools` 的工具。
    let cli = Cli::spawn(
        dir.path(),
        &["-p", "--allowed-tools", "Read,Write", "blocked write"],
        &[],
    );
    let (request, _) = receive(&server);
    calls_reply(
        request,
        json!([call(
            "w",
            "write",
            json!({"path": "denied.txt", "content": "nope"})
        )]),
    );
    let (request, body) = receive(&server);
    let w_res = result(
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool")
            .unwrap(),
        "w",
    );
    assert_eq!(w_res["ok"], false);
    assert!(w_res["error"].as_str().unwrap().contains("blocked by"));
    final_reply(request, "write blocked");
    assert!(cli.finish().status.success());
    assert!(!dir.path().join("denied.txt").exists());

    // 3. `--allowed-tools Read,Task` 开启 subagent，`*` 放行常规工具但不包含 task。
    let cli = Cli::spawn(
        dir.path(),
        &["-p", "--allowed-tools", "Read,Task", "use subagent"],
        &[],
    );
    let (request, body) = receive(&server);
    let names = tool_names(&body, "completion");
    assert!(names.contains("task"), "{names:?}");
    assert!(names.contains("wait"), "{names:?}");
    assert!(read_tool_description(&body, "completion").contains("agent://"));
    calls_reply(
        request,
        json!([call(
            "spawn",
            "task",
            json!({"context": "check", "tasks": [{"role": "fast", "task": "report result"}]})
        )]),
    );
    loop {
        let (request, body) = receive(&server);
        let system = body["messages"][0]["content"].as_str().unwrap();
        if system.contains("You are subagent") {
            final_reply(request, "CHILD_DONE_MARKER");
        } else {
            let parent_done = body.to_string().contains("CHILD_DONE_MARKER");
            final_reply(request, "parent ok");
            if parent_done {
                break;
            }
        }
    }
    assert!(cli.finish().status.success());

    let cli = Cli::spawn(
        dir.path(),
        &["-p", "--allowed-tools", "*", "wildcard no task"],
        &[],
    );
    let (request, body) = receive(&server);
    let names = tool_names(&body, "completion");
    assert!(names.contains("write"));
    assert!(names.contains("bash"));
    assert!(!names.contains("task"));
    final_reply(request, "wildcard ok");
    assert!(cli.finish().status.success());

    // 4. 非法工具名立即失败。
    let bad = Cli::spawn(
        dir.path(),
        &["-p", "--allowed-tools", "NotATool", "fail"],
        &[],
    )
    .finish();
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown tool"));
    no_more(&server);
}

#[test]
fn output_mode_json_and_stream_json_emit_events_and_terminal_results() {
    let protocol = "response";
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let models = if protocol == "completion" {
        fixture_models(&server)
    } else {
        response_models(&server, Some("response"))
    };
    write_settings(dir.path(), fixture_settings()).unwrap();
    std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();

    // 1. `--mode stream-json`：验证 init、过程事件与末行 result。
    let cli = Cli::spawn(
        dir.path(),
        &[
            "-p",
            "create file via stream",
            "--mode",
            "stream-json",
            "--allowed-tools",
            "Read,Write",
        ],
        &[],
    );
    let path = if protocol == "completion" {
        "/v1/chat/completions"
    } else {
        "/v1/responses"
    };
    let (request, _) = receive_at(&server, path);
    let write_args = json!({"path": "out.txt", "content": "hello jsonl\n"});
    if protocol == "completion" {
        let mut stream = request.start_sse().unwrap();
        stream
                .event(&json!({
                    "id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"fixture-model",
                    "choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"w1","type":"function","function":{"name":"write","arguments":write_args.to_string()}}]},"finish_reason":null}]
                }))
                .unwrap();
        stream
                .event(&json!({
                    "id":"chatcmpl-1","object":"chat.completion.chunk","created":1,"model":"fixture-model",
                    "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
                    "usage":{"prompt_tokens":100,"completion_tokens":15,"total_tokens":115}
                }))
                .unwrap();
        stream.frame("[DONE]").unwrap();
        stream.finish();
    } else {
        let item = response_call("w1", "write", write_args);
        let mut stream = request.start_sse().unwrap();
        stream
            .event(&json!({
                "type":"response.output_item.done","sequence_number":0,
                "output_index":0,"item":item
            }))
            .unwrap();
        let mut resp = response_body(json!([item]));
        resp["usage"] = json!({
            "input_tokens":100,"output_tokens":15,"total_tokens":115,
            "input_tokens_details":{"cached_tokens":0},
            "output_tokens_details":{"reasoning_tokens":0}
        });
        stream
            .event(&json!({
                "type":"response.completed","sequence_number":1,
                "response":resp
            }))
            .unwrap();
        stream.finish();
    }
    let (request, _) = receive_at(&server, path);
    if protocol == "completion" {
        let mut stream = request.start_sse().unwrap();
        stream
            .event(&chat_chunk(
                json!({"role":"assistant","content":"stream "}),
                None,
            ))
            .unwrap();
        stream
            .event(&chat_chunk(json!({"content":"done"}), Some("stop")))
            .unwrap();
        stream
                .event(&json!({
                    "id":"chatcmpl-2","object":"chat.completion.chunk","created":1,"model":"fixture-model",
                    "choices":[],
                    "usage":{"prompt_tokens":120,"completion_tokens":8,"total_tokens":128}
                }))
                .unwrap();
        stream.frame("[DONE]").unwrap();
        stream.finish();
    } else {
        let msg = response_message("msg-final", json!([response_text("stream done")]));
        let mut stream = request.start_sse().unwrap();
        stream
            .event(&json!({
                "type":"response.output_text.delta","sequence_number":0,
                "item_id":"msg-final","output_index":0,"content_index":0,
                "delta":"stream ","logprobs":[]
            }))
            .unwrap();
        stream
            .event(&json!({
                "type":"response.output_text.delta","sequence_number":1,
                "item_id":"msg-final","output_index":0,"content_index":0,
                "delta":"done","logprobs":[]
            }))
            .unwrap();
        stream
            .event(&json!({
                "type":"response.output_item.done","sequence_number":2,
                "output_index":0,"item":msg
            }))
            .unwrap();
        let mut resp = response_body(json!([msg]));
        resp["usage"] = json!({
            "input_tokens":120,"output_tokens":8,"total_tokens":128,
            "input_tokens_details":{"cached_tokens":0},
            "output_tokens_details":{"reasoning_tokens":0}
        });
        stream
            .event(&json!({
                "type":"response.completed","sequence_number":3,
                "response":resp
            }))
            .unwrap();
        stream.finish();
    }
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("write/edit only modify"), "{stderr}");
    assert!(!stderr.contains("Resume with:"), "{stderr}");
    let lines: Vec<Value> = std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines[0]["type"], "init");
    assert_eq!(lines[0]["resumed"], false);
    assert_eq!(lines[0]["tools"], json!(["read", "write"]));
    assert!(lines.iter().any(|e| e["type"] == "tool_started"
        && e["name"] == "write"
        && e["arguments"]["path"] == "out.txt"));
    assert!(
        lines
            .iter()
            .any(|e| e["type"] == "file_changed" && e["path"] == "out.txt")
    );
    assert!(
        lines
            .iter()
            .any(|e| e["type"] == "tool_finished" && e["name"] == "write" && e["ok"] == true)
    );
    assert!(
        lines
            .iter()
            .any(|e| e["type"] == "assistant_finished" && e["text"] == "stream done")
    );
    let last = lines.last().unwrap();
    assert_eq!(last["type"], "result");
    assert_eq!(last["status"], "ok");
    assert_eq!(last["text"], "stream done");
    assert_eq!(last["turns"], 2);
    assert_eq!(last["usage"]["input_tokens"], 220);
    assert_eq!(last["usage"]["output_tokens"], 23);
    let session_id = last["session_id"].as_str().unwrap().to_owned();

    // 2. `--mode json` + `-p -r <id> "prompt"`（且把 `--mode json` 放在位置参数之后）：
    //    stdout 仅输出单条终态汇总 JSON。
    let cli = Cli::spawn(
        dir.path(),
        &["-p", "-r", &session_id, "summarize json", "--mode", "json"],
        &[],
    );
    let (request, _) = receive_at(&server, path);
    if protocol == "completion" {
        final_reply(request, "single json ok");
    } else {
        response_final(request, "single json ok");
    }
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    let stdout_str = std::str::from_utf8(&output.stdout).unwrap();
    assert_eq!(stdout_str.lines().count(), 1, "{stdout_str}");
    let summary: Value = serde_json::from_str(stdout_str.trim()).unwrap();
    assert_eq!(summary["type"], "result");
    assert_eq!(summary["status"], "ok");
    assert_eq!(summary["session_id"], session_id);
    assert_eq!(summary["text"], "single json ok");
    assert_eq!(summary["turns"], 1);

    // 3. 运行期 HTTP 500 错误：json 与 stream-json 均向 stdout 写终态 error JSON 且脱敏。
    for mode in ["json", "stream-json"] {
        let cli = Cli::spawn(dir.path(), &["-p", "explode", "--mode", mode], &[]);
        let (request, _) = receive_at(&server, path);
        request
            .respond(
                Response::builder()
                    .status(500)
                    .body(Body::from(
                        json!({"error":{"message":"upstream leaked fixture-key"}}).to_string(),
                    ))
                    .unwrap(),
            )
            .unwrap();
        let output = cli.finish();
        assert!(!output.status.success());
        let err_lines: Vec<Value> = std::str::from_utf8(&output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(!err_lines.is_empty());
        let terminal = err_lines.last().unwrap();
        assert_eq!(terminal["type"], "result");
        assert_eq!(terminal["status"], "error");
        let err_msg = terminal["error"].as_str().unwrap();
        assert!(!err_msg.contains("fixture-key"), "{err_msg}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("error:"),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // 4. 启动前失败（非法 --resume ID）：stdout 仍输出终态 error JSON。
    let early = Cli::spawn(
        dir.path(),
        &["-p", "-r", "../escape", "bad", "--mode", "json"],
        &[],
    )
    .finish();
    assert!(!early.status.success());
    let early_json: Value =
        serde_json::from_str(std::str::from_utf8(&early.stdout).unwrap().trim()).unwrap();
    assert_eq!(early_json["type"], "result");
    assert_eq!(early_json["status"], "error");
    assert!(
        early_json["error"]
            .as_str()
            .unwrap()
            .contains("invalid session ID")
    );
    no_more(&server);
}
