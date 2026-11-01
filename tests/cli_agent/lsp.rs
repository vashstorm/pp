#[cfg(unix)]
use super::tui::Pty;
use super::*;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tiny_http::Request;

const LSP_DEADLINE: Duration = Duration::from_secs(10);
const LSP_HELPER: &str = "pub fn value() -> i32 {\n    \"wrong\"\n}\n";
const LSP_LIB: &str = "mod helper;\npub fn answer() -> i32 {\n    helper::value()\n}\n";
const LSP_HELPER_FIXED: &str = "pub fn value() -> i32 {\n    42\n}\n";

fn lsp_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lsp_server.py")
}

fn lsp_api_path(protocol: &str) -> &'static str {
    match protocol {
        "completion" => "/v1/chat/completions",
        _ => "/v1/responses",
    }
}

fn lsp_models(server: &Server, protocol: &str) -> String {
    match protocol {
        "completion" => fixture_models(server),
        _ => response_models(server, Some("response")),
    }
}

fn lsp_settings(format_on_write: bool, diagnostics_on_edit: bool) -> String {
    json!({
        "rate_limit_retry_budget_seconds": 0,
        "lsp": {
            "requestTimeout": 5,
            "formatOnWrite": format_on_write,
            "diagnosticsOnWrite": true,
            "diagnosticsOnEdit": diagnostics_on_edit,
        }
    })
    .to_string()
}

fn lsp_project(dir: &Path) {
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("lspfixture.toml"), "").unwrap();
    fs::write(dir.join("src/helper.lspfixture"), LSP_HELPER).unwrap();
    fs::write(dir.join("src/lib.lspfixture"), LSP_LIB).unwrap();
}

fn lsp_uri(path: &Path) -> String {
    let path = fs::canonicalize(path).unwrap();
    format!("file://{}", path.display())
}

fn lsp_config(log: &Path, barrier: &Path, extra: &[&str]) -> String {
    let mut args = vec![
        lsp_script().display().to_string(),
        "--log".to_owned(),
        log.display().to_string(),
        "--barrier-dir".to_owned(),
        barrier.display().to_string(),
        "--real-navigation".to_owned(),
        "--scan-extension".to_owned(),
        ".lspfixture".to_owned(),
    ];
    args.extend(extra.iter().map(|value| (*value).to_owned()));
    json!({
        "servers": {
            "fixture": {
                "command": "python3",
                "args": args,
                "fileTypes": [".lspfixture"],
                "rootMarkers": ["lspfixture.toml"],
            }
        }
    })
    .to_string()
}

fn lsp_write_config(home: &Path, log: &Path, barrier: &Path, extra: &[&str]) {
    fs::create_dir_all(barrier).unwrap();
    fs::write(
        config_path(home, "lsp.json"),
        lsp_config(log, barrier, extra),
    )
    .unwrap();
}

fn lsp_receive(server: &Server, protocol: &str) -> (Request, Value) {
    receive_at(server, lsp_api_path(protocol))
}

fn lsp_call(protocol: &str, id: &str, arguments: Value) -> Value {
    match protocol {
        "completion" => call(id, "lsp", arguments),
        _ => response_call(id, "lsp", arguments),
    }
}

fn lsp_calls_reply(request: Request, protocol: &str, calls: Value) {
    match protocol {
        "completion" => calls_reply(request, calls),
        _ => response_reply(request, calls),
    }
}

fn lsp_final(request: Request, protocol: &str, text: &str) {
    match protocol {
        "completion" => final_reply(request, text),
        _ => response_final(request, text),
    }
}

fn lsp_tool(body: &Value, protocol: &str) -> Option<Value> {
    body["tools"].as_array().and_then(|tools| {
        tools.iter().find_map(|tool| {
            if protocol == "completion" {
                (tool["function"]["name"] == "lsp").then(|| tool["function"].clone())
            } else {
                (tool["name"] == "lsp").then(|| tool.clone())
            }
        })
    })
}

fn lsp_action_enum(tool: &Value) -> Vec<String> {
    tool["parameters"]["properties"]["action"]["enum"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_owned())
        .collect()
}

fn lsp_assert_schema(body: &Value, protocol: &str, read_only: bool) -> Value {
    let tool = lsp_tool(body, protocol).unwrap_or_else(|| panic!("missing lsp tool: {body}"));
    let parameters = &tool["parameters"];
    assert_eq!(parameters["type"], "object");
    assert_eq!(parameters["required"], json!(["action"]));
    assert_eq!(parameters["additionalProperties"], false);
    let mut fields: Vec<&str> = parameters["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "action",
            "file",
            "line",
            "new_name",
            "query",
            "selection",
            "symbol",
            "timeout"
        ],
        "{parameters}"
    );
    assert_eq!(parameters["properties"]["line"]["minimum"], 1);
    assert_eq!(parameters["properties"]["timeout"]["minimum"], 5);
    assert_eq!(parameters["properties"]["timeout"]["maximum"], 300);
    let actions = lsp_action_enum(&tool);
    if read_only {
        let expected = [
            "diagnostics",
            "definition",
            "type_definition",
            "implementation",
            "references",
            "hover",
            "symbols",
            "status",
            "capabilities",
            "rename",
            "rename_file",
            "code_actions",
            "format",
        ];
        assert_eq!(
            actions, expected,
            "read-only lsp schema leaked a mutating action"
        );
    } else {
        for action in [
            "diagnostics",
            "definition",
            "references",
            "hover",
            "symbols",
            "rename",
            "rename_file",
            "code_actions",
            "type_definition",
            "implementation",
            "status",
            "reload",
            "capabilities",
            "format",
        ] {
            assert!(
                actions.iter().any(|value| value.as_str() == action),
                "{actions:?}"
            );
        }
    }
    tool
}

fn lsp_tool_result(protocol: &str, body: &Value, id: &str) -> Value {
    match protocol {
        "completion" => {
            let message = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
                .unwrap_or_else(|| panic!("missing tool result {id}: {body}"));
            result(message, id)
        }
        _ => {
            let item = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["type"] == "function_call_output" && item["call_id"] == id)
                .unwrap_or_else(|| panic!("missing tool result {id}: {body}"));
            response_result(item, id)
        }
    }
}

fn lsp_output_ids(protocol: &str, body: &Value) -> HashSet<String> {
    match protocol {
        "completion" => body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .map(|message| message["tool_call_id"].as_str().unwrap().to_owned())
            .collect(),
        _ => body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "function_call_output")
            .map(|item| item["call_id"].as_str().unwrap().to_owned())
            .collect(),
    }
}

fn lsp_assert_pairs(protocol: &str, body: &Value) {
    let mut calls: HashMap<String, usize> = HashMap::new();
    let mut outputs: HashMap<String, usize> = HashMap::new();
    match protocol {
        "completion" => {
            for (index, message) in body["messages"].as_array().unwrap().iter().enumerate() {
                if let Some(items) = message["tool_calls"].as_array() {
                    for item in items {
                        calls.insert(item["id"].as_str().unwrap().to_owned(), index);
                    }
                }
                if message["role"] == "tool" {
                    outputs.insert(message["tool_call_id"].as_str().unwrap().to_owned(), index);
                }
            }
        }
        _ => {
            for (index, item) in body["input"].as_array().unwrap().iter().enumerate() {
                match item["type"].as_str() {
                    Some("function_call") => {
                        calls.insert(item["call_id"].as_str().unwrap().to_owned(), index);
                    }
                    Some("function_call_output") => {
                        outputs.insert(item["call_id"].as_str().unwrap().to_owned(), index);
                    }
                    _ => {}
                }
            }
        }
    }
    assert_eq!(
        calls.keys().collect::<HashSet<_>>(),
        outputs.keys().collect::<HashSet<_>>(),
        "native tool call/result pairing is broken: {body}"
    );
    for (id, call_index) in &calls {
        assert!(
            outputs.get(id).is_some_and(|output| output > call_index),
            "tool result {id} does not follow its native call: {body}"
        );
    }
}

fn lsp_log(path: &Path) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn lsp_wait_log(
    path: &Path,
    description: &str,
    condition: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = Instant::now() + LSP_DEADLINE;
    loop {
        let events = lsp_log(path);
        if condition(&events) {
            return events;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}: {events:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn lsp_started(event: &Value) -> bool {
    event["event"] == "started"
}

fn lsp_started_pids(events: &[Value]) -> Vec<i32> {
    events
        .iter()
        .filter(|event| lsp_started(event))
        .map(|event| event["pid"].as_i64().unwrap() as i32)
        .collect()
}

fn lsp_wait_started(path: &Path) -> Vec<Value> {
    lsp_wait_log(path, "language server start", |events| {
        events.iter().any(lsp_started)
    })
}

fn lsp_fifo(path: &Path) {
    let status = Command::new("mkfifo").arg(path).status().unwrap();
    assert!(status.success(), "mkfifo failed: {status}");
}

fn lsp_release_fifo(path: &Path) {
    // O_RDWR 打开 FIFO 不会等待读者；读者随后仍能读到这一字节。
    let mut stream = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    stream.write_all(b"x").unwrap();
    stream.flush().unwrap();
}

fn lsp_assert_pid_dead(pid: i32) {
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    let deadline = Instant::now() + LSP_DEADLINE;
    while rustix::process::test_kill_process(pid).is_ok() {
        assert!(
            Instant::now() < deadline,
            "fixture process {pid:?} still runs"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn lsp_methods(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| event["method"].as_str().map(str::to_owned))
        .collect()
}

fn lsp_assert_no_method(events: &[Value], forbidden: &[&str]) {
    let methods = lsp_methods(events);
    for method in forbidden {
        assert!(
            !methods.iter().any(|value| value == *method),
            "fixture received forbidden {method}: {methods:?}"
        );
    }
}

fn lsp_apply_response(events: &[Value], applied: bool) -> Value {
    events
        .iter()
        .find(|event| {
            event["id"] == "fixture-apply-edit" && event["result"]["applied"] == json!(applied)
        })
        .unwrap_or_else(|| panic!("missing applyEdit response applied={applied}: {events:?}"))
        .clone()
}

fn lsp_is_child(body: &Value) -> bool {
    let system = if body["instructions"].is_string() {
        body["instructions"].to_string()
    } else {
        body["messages"][0].to_string()
    };
    system.contains("You are subagent")
}

fn lsp_dev_message(protocol: &str, body: &Value) -> String {
    let message_text = |value: &Value| match value {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| {
                part["text"]
                    .as_str()
                    .or_else(|| part["refusal"].as_str())
                    .unwrap_or("")
            })
            .collect::<Vec<_>>()
            .join("\n"),
        value => value.to_string(),
    };
    match protocol {
        "completion" => body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "developer")
            .map(|message| message_text(&message["content"]))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["role"] == "developer")
            .map(|item| message_text(&item["content"]))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn lsp_no_more_tui(server: &Server) {
    while let Some(mut request) = server.recv_timeout(Duration::from_millis(100)).unwrap() {
        let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
        assert!(
            is_task_title(&body),
            "unexpected extra model request: {body}"
        );
        let _ = request.respond(tiny_http::Response::empty(204));
    }
}

/// 一次完整的 `-p` 会话：query → modify → diagnostics → final，全部使用真实
/// 磁盘与真实 stdio 协议；工具结果按 native call/result 成对校验。
#[test]
fn lsp_roundtrip_chat_and_responses_query_modify_diagnostic() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        lsp_project(dir.path());
        let log = dir.path().join("lsp-log.jsonl");
        let barrier = dir.path().join("barriers");
        lsp_write_config(dir.path(), &log, &barrier, &[]);
        let cli = Cli::start_with(
            dir.path(),
            Some("lsp roundtrip"),
            &lsp_settings(false, false),
            &lsp_models(&provider, protocol),
        );

        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_schema(&body, protocol, false);
        lsp_calls_reply(
            request,
            protocol,
            json!([lsp_call(
                protocol,
                "lsp-def",
                json!({"action":"definition","file":"src/lib.lspfixture","line":3,"symbol":"value","timeout":10})
            )]),
        );

        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_pairs(protocol, &body);
        let definition = lsp_tool_result(protocol, &body, "lsp-def");
        assert_eq!(definition["ok"], true, "{definition}");
        let content = definition["content"].as_str().unwrap();
        assert!(content.contains("src/helper.lspfixture:1:8"), "{content}");
        assert!(content.contains("pub fn value"), "{content}");
        lsp_calls_reply(
            request,
            protocol,
            json!([lsp_call(
                protocol,
                "lsp-diag",
                json!({"action":"diagnostics","file":"src/helper.lspfixture","timeout":10})
            )]),
        );

        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_pairs(protocol, &body);
        let diagnostics = lsp_tool_result(protocol, &body, "lsp-diag");
        assert_eq!(diagnostics["ok"], true, "{diagnostics}");
        assert_eq!(
            diagnostics["diagnostics"]["servers"][0]["status"],
            "reported_issues"
        );
        let content = diagnostics["content"].as_str().unwrap();
        assert!(
            content.contains("[error] [fixture] fixture: wrong value"),
            "{content}"
        );
        lsp_calls_reply(
            request,
            protocol,
            json!([match protocol {
                "completion" => call(
                    "lsp-write",
                    "write",
                    json!({"path":"src/helper.lspfixture","content":LSP_HELPER_FIXED}),
                ),
                _ => response_call(
                    "lsp-write",
                    "write",
                    json!({"path":"src/helper.lspfixture","content":LSP_HELPER_FIXED}),
                ),
            }]),
        );

        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_pairs(protocol, &body);
        let write = lsp_tool_result(protocol, &body, "lsp-write");
        assert_eq!(write["ok"], true, "{write}");
        assert_eq!(write["bytes_written"], json!(LSP_HELPER_FIXED.len()));
        assert_eq!(
            write["diagnostics"]["servers"][0]["status"], "reported_clean",
            "{write}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/helper.lspfixture")).unwrap(),
            LSP_HELPER_FIXED
        );
        lsp_calls_reply(
            request,
            protocol,
            json!([lsp_call(
                protocol,
                "lsp-refs",
                json!({"action":"references","file":"src/lib.lspfixture","line":3,"symbol":"value","timeout":10})
            )]),
        );

        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_pairs(protocol, &body);
        let references = lsp_tool_result(protocol, &body, "lsp-refs");
        assert_eq!(references["ok"], true, "{references}");
        let content = references["content"].as_str().unwrap();
        assert!(content.contains("Found 2 reference(s)"), "{content}");
        assert!(content.contains("src/helper.lspfixture:1:8"), "{content}");
        assert!(content.contains("src/lib.lspfixture:3:13"), "{content}");
        lsp_calls_reply(
            request,
            protocol,
            json!([lsp_call(
                protocol,
                "lsp-preview",
                json!({"action":"rename","file":"src/helper.lspfixture","line":1,"symbol":"value","new_name":"number","apply":false,"timeout":10})
            )]),
        );

        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_pairs(protocol, &body);
        let preview = lsp_tool_result(protocol, &body, "lsp-preview");
        assert_eq!(preview["ok"], true, "{preview}");
        assert!(
            preview["content"]
                .as_str()
                .unwrap()
                .contains("Modification suggestions only"),
            "{preview}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/helper.lspfixture")).unwrap(),
            LSP_HELPER_FIXED,
            "preview must not write"
        );
        lsp_calls_reply(
            request,
            protocol,
            json!([lsp_call(
                protocol,
                "lsp-rename",
                json!({"action":"rename","file":"src/helper.lspfixture","line":1,"symbol":"value","new_name":"number","timeout":10})
            )]),
        );

        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_pairs(protocol, &body);
        let rename = lsp_tool_result(protocol, &body, "lsp-rename");
        assert_eq!(rename["ok"], true, "{rename}");
        assert_eq!(rename["details"]["applied"], false);
        assert_eq!(
            fs::read_to_string(dir.path().join("src/helper.lspfixture")).unwrap(),
            LSP_HELPER_FIXED
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/lib.lspfixture")).unwrap(),
            LSP_LIB
        );
        let tool_call = |id: &str, name: &str, args: Value| match protocol {
            "completion" => call(id, name, args),
            _ => response_call(id, name, args),
        };
        lsp_calls_reply(
            request,
            protocol,
            json!([
                tool_call(
                    "read-helper",
                    "read",
                    json!({"path":"src/helper.lspfixture"})
                ),
                tool_call("read-lib", "read", json!({"path":"src/lib.lspfixture"}))
            ]),
        );
        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_pairs(protocol, &body);
        for id in ["read-helper", "read-lib"] {
            assert_eq!(lsp_tool_result(protocol, &body, id)["ok"], true);
        }
        lsp_calls_reply(
            request,
            protocol,
            json!([
                tool_call(
                    "edit-helper",
                    "edit",
                    json!({"path":"src/helper.lspfixture","edits":[{"oldText":"fn value","newText":"fn number"}]})
                ),
                tool_call(
                    "edit-lib",
                    "edit",
                    json!({"path":"src/lib.lspfixture","edits":[{"oldText":"helper::value","newText":"helper::number"}]})
                )
            ]),
        );
        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_pairs(protocol, &body);
        for id in ["edit-helper", "edit-lib"] {
            assert_eq!(lsp_tool_result(protocol, &body, id)["ok"], true);
        }
        assert_eq!(
            fs::read_to_string(dir.path().join("src/helper.lspfixture")).unwrap(),
            LSP_HELPER_FIXED.replace("fn value", "fn number")
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/lib.lspfixture")).unwrap(),
            LSP_LIB.replace("helper::value", "helper::number")
        );
        lsp_calls_reply(
            request,
            protocol,
            json!([lsp_call(
                protocol,
                "lsp-clean",
                json!({"action":"diagnostics","file":"src/helper.lspfixture","timeout":10})
            )]),
        );

        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_pairs(protocol, &body);
        let clean = lsp_tool_result(protocol, &body, "lsp-clean");
        assert_eq!(clean["ok"], true, "{clean}");
        assert_eq!(
            clean["diagnostics"]["servers"][0]["status"], "reported_clean",
            "{clean}"
        );
        lsp_final(request, protocol, "lsp roundtrip complete");

        let events = lsp_wait_started(&log);
        let pids = lsp_started_pids(&events);
        assert_eq!(pids.len(), 1, "{events:?}");
        lsp_assert_no_method(&events, &["textDocument/codeAction"]);
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("lsp roundtrip complete"),
            "{output:?}"
        );
        lsp_assert_pid_dead(pids[0]);
        no_more(&provider);
    }
}

/// 命令型 code action 只报告不支持，不发送 executeCommand 或触发编辑回调。
#[test]
fn lsp_roundtrip_code_action_command_is_not_executed() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        lsp_project(dir.path());
        let log = dir.path().join("lsp-log.jsonl");
        let barrier = dir.path().join("barriers");
        let callback_uri = lsp_uri(&dir.path().join("src/lib.lspfixture"));
        lsp_write_config(
            dir.path(),
            &log,
            &barrier,
            &[
                "--apply-edit-on",
                "workspace/executeCommand",
                "--apply-edit-uri",
                &callback_uri,
                "--apply-edit-text",
                "callback applied\n",
            ],
        );
        let cli = Cli::start_with(
            dir.path(),
            Some("lsp callback inside"),
            &lsp_settings(false, false),
            &lsp_models(&provider, protocol),
        );
        let (request, _) = lsp_receive(&provider, protocol);
        lsp_calls_reply(
            request,
            protocol,
            json!([lsp_call(
                protocol,
                "lsp-code",
                json!({"action":"code_actions","file":"src/helper.lspfixture","line":1,"selection":"fixture apply edit","timeout":10})
            )]),
        );

        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_pairs(protocol, &body);
        let result = lsp_tool_result(protocol, &body, "lsp-code");
        assert_eq!(result["ok"], false, "{result}");
        assert_eq!(result["details"]["applied"], false);
        assert!(
            result["content"]
                .as_str()
                .unwrap()
                .contains("will not execute")
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("src/lib.lspfixture")).unwrap(),
            LSP_LIB
        );
        lsp_final(request, protocol, "lsp callback complete");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        let events = lsp_log(&log);
        lsp_assert_no_method(&events, &["workspace/executeCommand"]);
        let pids = lsp_started_pids(&events);
        assert_eq!(pids.len(), 1);
        lsp_assert_pid_dead(pids[0]);
        no_more(&provider);
    }
}

/// rename 查询期间服务器主动要求编辑：一律拒绝，工作区内外文件均不改变。
#[test]
fn lsp_roundtrip_rename_apply_edit_outside_workspace_rejected() {
    let provider = server();
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    lsp_project(dir.path());
    let outside_file = outside.path().join("outside.lspfixture");
    fs::write(&outside_file, "outside original\n").unwrap();
    let outside_uri = lsp_uri(&outside_file);
    let log = dir.path().join("lsp-log.jsonl");
    let barrier = dir.path().join("barriers");
    lsp_write_config(
        dir.path(),
        &log,
        &barrier,
        &[
            "--apply-edit-on",
            "textDocument/rename",
            "--apply-edit-uri",
            &outside_uri,
            "--apply-edit-text",
            "escaped\n",
        ],
    );
    let cli = Cli::start_with(
        dir.path(),
        Some("lsp callback outside"),
        &lsp_settings(false, false),
        &lsp_models(&provider, "completion"),
    );
    let (request, _) = lsp_receive(&provider, "completion");
    lsp_calls_reply(
        request,
        "completion",
        json!([lsp_call(
            "completion",
            "lsp-rename-outside",
            json!({"action":"rename","file":"src/helper.lspfixture","line":1,"symbol":"value","new_name":"number","timeout":10})
        )]),
    );
    let (request, body) = lsp_receive(&provider, "completion");
    lsp_assert_pairs("completion", &body);
    let result = lsp_tool_result("completion", &body, "lsp-rename-outside");
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(
        fs::read_to_string(&outside_file).unwrap(),
        "outside original\n",
        "applyEdit escaped the workspace"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.lspfixture")).unwrap(),
        LSP_LIB
    );
    lsp_final(request, "completion", "lsp outside rejected");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    let events = lsp_wait_log(&log, "rejected applyEdit response", |events| {
        events
            .iter()
            .any(|event| event["id"] == "fixture-apply-edit")
    });
    lsp_apply_response(&events, false);
    let pids = lsp_started_pids(&events);
    assert_eq!(pids.len(), 1);
    lsp_assert_pid_dead(pids[0]);
    no_more(&provider);
}

/// Plan 会话：受限 schema + 执行层门禁；服务端 callback 在只读操作中被拒绝。
#[cfg(unix)]
#[test]
fn lsp_roundtrip_plan_readonly_denies_mutations_and_server_apply_edit() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    lsp_project(project.path());
    let log = home.path().join("lsp-log.jsonl");
    let barrier = home.path().join("barriers");
    let callback_uri = lsp_uri(&project.path().join("src/lib.lspfixture"));
    lsp_write_config(
        home.path(),
        &log,
        &barrier,
        &[
            "--apply-edit-on",
            "textDocument/definition",
            "--apply-edit-uri",
            &callback_uri,
            "--apply-edit-text",
            "must not apply\n",
        ],
    );
    write_settings(home.path(), lsp_settings(false, false)).unwrap();
    fs::write(
        config_path(home.path(), "models.yml"),
        lsp_models(&provider, "completion"),
    )
    .unwrap();
    let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
    pty.ready();
    pty.input("/plan readonly lsp task\r");
    let (request, body) = pty.request_at(&provider, lsp_api_path("completion"));
    pty.wait_for("plan mode active", |screen| {
        screen
            .contents()
            .lines()
            .last()
            .is_some_and(|line| line.contains(" Plan · "))
    });
    lsp_assert_schema(&body, "completion", true);
    lsp_calls_reply(
        request,
        "completion",
        json!([
            lsp_call(
                "completion",
                "plan-rename",
                json!({"action":"rename","file":"src/helper.lspfixture","line":1,"symbol":"value","new_name":"number","apply":true})
            ),
            lsp_call(
                "completion",
                "plan-request",
                json!({"action":"request","file":"src/lib.lspfixture","query":"textDocument/hover"})
            ),
            lsp_call("completion", "plan-reload", json!({"action":"reload"})),
            lsp_call(
                "completion",
                "plan-code",
                json!({"action":"code_actions","file":"src/helper.lspfixture","apply":true})
            ),
            lsp_call(
                "completion",
                "plan-workspace",
                json!({"action":"diagnostics","file":"*"})
            ),
        ]),
    );
    let (request, body) = pty.request_at(&provider, lsp_api_path("completion"));
    for id in [
        "plan-rename",
        "plan-request",
        "plan-reload",
        "plan-code",
        "plan-workspace",
    ] {
        let result = lsp_tool_result("completion", &body, id);
        assert_eq!(result["ok"], false, "{id}: {result}");
        assert!(
            result["error"].as_str().unwrap().contains("read-only")
                || result["error"].as_str().unwrap().contains("not available")
                || result["error"].as_str().unwrap().contains("unsupported")
                || result["error"]
                    .as_str()
                    .unwrap()
                    .contains("unknown lsp action")
                || result["error"].as_str().unwrap().contains("not run by LSP"),
            "{id}: {result}"
        );
    }
    assert!(
        !log.exists() || lsp_started_pids(&lsp_log(&log)).is_empty(),
        "denied Plan actions must not start a language server"
    );
    assert!(!project.path().join("target").exists());
    assert!(!project.path().join("Cargo.lock").exists());
    lsp_calls_reply(
        request,
        "completion",
        json!([lsp_call(
            "completion",
            "plan-def",
            json!({"action":"definition","file":"src/lib.lspfixture","line":3,"symbol":"value","timeout":10})
        )]),
    );
    let (request, body) = pty.request_at(&provider, lsp_api_path("completion"));
    let definition = lsp_tool_result("completion", &body, "plan-def");
    assert_eq!(definition["ok"], true, "{definition}");
    assert!(
        definition["content"]
            .as_str()
            .unwrap()
            .contains("src/helper.lspfixture:1:8"),
        "{definition}"
    );
    assert_eq!(
        fs::read_to_string(project.path().join("src/lib.lspfixture")).unwrap(),
        LSP_LIB,
        "read-only server applyEdit changed the workspace"
    );
    lsp_final(request, "completion", "plan readonly done");
    pty.text("plan readonly done");
    pty.ready();
    pty.input("\u{3}");
    pty.finish();

    let events = lsp_wait_log(&log, "readonly applyEdit response", |events| {
        events
            .iter()
            .any(|event| event["id"] == "fixture-apply-edit")
    });
    lsp_apply_response(&events, false);
    assert_eq!(lsp_started_pids(&events).len(), 1, "{events:?}");
    lsp_assert_no_method(
        &events,
        &[
            "textDocument/rename",
            "textDocument/codeAction",
            "workspace/executeCommand",
        ],
    );
    lsp_no_more_tui(&provider);
}

/// scout/reviewer 与 Plan 同源：execution 层强制 ReadOnly，共享同一个真实
/// 服务器进程，拒绝全部 mutating 动作且不启动任何 checker。
#[test]
fn lsp_roundtrip_readonly_subagents_share_server_and_deny_mutations() {
    let provider = server();
    let dir = tempfile::tempdir().unwrap();
    lsp_project(dir.path());
    let log = dir.path().join("lsp-log.jsonl");
    let barrier = dir.path().join("barriers");
    lsp_write_config(dir.path(), &log, &barrier, &[]);
    let mut cli = super::streaming::LiveCli::new(Cli::start_with(
        dir.path(),
        Some("readonly subagents"),
        &lsp_settings(false, false),
        &lsp_models(&provider, "completion"),
    ));
    let (request, body) = lsp_receive(&provider, "completion");
    assert!(body.to_string().contains("\"task\""));
    lsp_calls_reply(
        request,
        "completion",
        json!([call(
            "spawn-readonly",
            "task",
            json!({"context":"readonly","tasks":[{"agent":"scout","task":"inspect only"},{"agent":"reviewer","task":"review only"}]})
        )]),
    );
    let mut children_done = 0;
    let mut parent_done = false;
    for _ in 0..30 {
        let (request, body) = lsp_receive(&provider, "completion");
        if lsp_is_child(&body) {
            let outputs = lsp_output_ids("completion", &body);
            if !outputs.contains("child-bad") {
                lsp_assert_schema(&body, "completion", true);
                lsp_calls_reply(
                    request,
                    "completion",
                    json!([
                        call(
                            "child-bad",
                            "lsp",
                            json!({"action":"rename","file":"src/helper.lspfixture","line":1,"symbol":"value","new_name":"number","apply":true})
                        ),
                        call(
                            "child-request",
                            "lsp",
                            json!({"action":"request","query":"textDocument/hover"})
                        ),
                        call("child-reload", "lsp", json!({"action":"reload"})),
                        call(
                            "child-code",
                            "lsp",
                            json!({"action":"code_actions","file":"src/helper.lspfixture","apply":true})
                        ),
                        call(
                            "child-workspace",
                            "lsp",
                            json!({"action":"diagnostics","file":"*"})
                        ),
                    ]),
                );
            } else if !outputs.contains("child-def") {
                for id in [
                    "child-bad",
                    "child-request",
                    "child-reload",
                    "child-code",
                    "child-workspace",
                ] {
                    let result = lsp_tool_result("completion", &body, id);
                    assert_eq!(result["ok"], false, "{id}: {result}");
                    assert!(
                        result["error"].as_str().unwrap().contains("read-only")
                            || result["error"].as_str().unwrap().contains("not available")
                            || result["error"].as_str().unwrap().contains("unsupported")
                            || result["error"]
                                .as_str()
                                .unwrap()
                                .contains("unknown lsp action")
                            || result["error"].as_str().unwrap().contains("not run by LSP"),
                        "{id}: {result}"
                    );
                }
                lsp_calls_reply(
                    request,
                    "completion",
                    json!([
                        call(
                            "child-def",
                            "lsp",
                            json!({"action":"definition","file":"src/lib.lspfixture","line":3,"symbol":"value","timeout":10})
                        ),
                        call(
                            "child-advice",
                            "lsp",
                            json!({"action":"rename","file":"src/helper.lspfixture","line":1,"symbol":"value","new_name":"number","timeout":10})
                        )
                    ]),
                );
            } else {
                let definition = lsp_tool_result("completion", &body, "child-def");
                assert_eq!(definition["ok"], true, "{definition}");
                let advice = lsp_tool_result("completion", &body, "child-advice");
                assert_eq!(advice["ok"], true, "{advice}");
                assert_eq!(advice["details"]["applied"], false);
                assert_eq!(
                    fs::read_to_string(dir.path().join("src/helper.lspfixture")).unwrap(),
                    LSP_HELPER
                );
                children_done += 1;
                lsp_final(request, "completion", "readonly-child-done");
            }
        } else if children_done == 2
            && lsp_dev_message("completion", &body)
                .matches("pp-subagent-result:")
                .count()
                == 2
        {
            lsp_final(request, "completion", "readonly subagents done");
            parent_done = true;
            break;
        } else {
            lsp_final(request, "completion", "waiting for readonly children");
        }
    }
    assert!(parent_done && children_done == 2);
    cli.wait_for(super::streaming::Pipe::Stdout, "readonly subagents done");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert!(!dir.path().join("target").exists());
    let events = lsp_wait_started(&log);
    let pids = lsp_started_pids(&events);
    assert_eq!(
        pids.len(),
        1,
        "readonly subagents must share one server: {events:?}"
    );
    lsp_assert_no_method(
        &events,
        &["textDocument/codeAction", "workspace/executeCommand"],
    );
    lsp_assert_pid_dead(pids[0]);
    no_more(&provider);
}

/// 行模式：慢诊断跨过内联 500ms 窗口后，空闲时显示迟到报告但不触发模型
/// 请求；下一次用户任务的首个请求恰好包含一次该报告，native 配对不拆。
#[test]
fn lsp_roundtrip_writethrough_late_line_idle_and_next_request() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        lsp_project(dir.path());
        let log = dir.path().join("lsp-log.jsonl");
        let barrier = dir.path().join("barriers");
        lsp_write_config(dir.path(), &log, &barrier, &[]);
        lsp_fifo(&barrier.join("diagnostics"));
        let mut cli = super::streaming::LiveCli::new(Cli::start_with(
            dir.path(),
            None,
            &lsp_settings(false, false),
            &lsp_models(&provider, protocol),
        ));
        cli.input("late diagnostics task\n");
        let (request, _) = lsp_receive(&provider, protocol);
        lsp_calls_reply(
            request,
            protocol,
            json!([match protocol {
                "completion" => call(
                    "late-write",
                    "write",
                    json!({"path":"src/helper.lspfixture","content":"pub fn value() -> i32 { \"wrong\" }\n"}),
                ),
                _ => response_call(
                    "late-write",
                    "write",
                    json!({"path":"src/helper.lspfixture","content":"pub fn value() -> i32 { \"wrong\" }\n"}),
                ),
            }]),
        );
        lsp_wait_log(&log, "diagnostics barrier", |events| {
            events.iter().any(|event| event["barrier"] == "diagnostics")
        });
        let (request, body) = lsp_receive(&provider, protocol);
        lsp_assert_pairs(protocol, &body);
        let write = lsp_tool_result(protocol, &body, "late-write");
        assert_eq!(write["ok"], true, "{write}");
        assert!(
            write["warning"]
                .as_str()
                .is_some_and(|warning| warning.contains("still running in the background")),
            "inline path must hand off instead of blocking: {write}"
        );
        assert!(write.get("diagnostics").is_none(), "{write}");
        lsp_final(request, protocol, "late task first pass");

        // final reply 之后没有任何新输入：先确认没有 provider 请求，再释放
        // FIFO，让后台诊断在空闲轮询中被展示。
        no_more(&provider);
        lsp_release_fifo(&barrier.join("diagnostics"));
        cli.wait_for(super::streaming::Pipe::Stderr, "LSP diagnostics (late)");
        cli.wait_for(super::streaming::Pipe::Stderr, "fixture: wrong value");
        no_more(&provider);

        cli.input("late follow-up task\n");
        let (request, body) = lsp_receive(&provider, protocol);
        let text = body.to_string();
        assert_eq!(
            text.matches("LSP diagnostics (late):").count(),
            1,
            "late report must be injected exactly once: {body}"
        );
        assert_eq!(
            text.matches("fixture: wrong value").count(),
            1,
            "diagnostic message repeated or missing: {body}"
        );
        let developer = lsp_dev_message(protocol, &body);
        assert!(developer.contains("src/helper.lspfixture"), "{developer}");
        lsp_assert_pairs(protocol, &body);
        lsp_final(request, protocol, "late follow-up done");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        let events = lsp_wait_started(&log);
        let pids = lsp_started_pids(&events);
        assert_eq!(pids.len(), 1, "{events:?}");
        lsp_assert_pid_dead(pids[0]);
    }
}

/// 非隔离子 Agent 与父 Agent 在同一 cwd 共享同一个服务器 PID，但各自持有
/// 独立的诊断 owner/inbox。
#[test]
fn lsp_roundtrip_parent_child_owner_reports_share_fixture() {
    let provider = server();
    let dir = tempfile::tempdir().unwrap();
    lsp_project(dir.path());
    let log = dir.path().join("lsp-log.jsonl");
    let barrier = dir.path().join("barriers");
    lsp_write_config(dir.path(), &log, &barrier, &[]);
    let cli = Cli::start_with(
        dir.path(),
        Some("owner reports"),
        &lsp_settings(false, false),
        &lsp_models(&provider, "completion"),
    );
    let (request, _) = lsp_receive(&provider, "completion");
    lsp_calls_reply(
        request,
        "completion",
        json!([lsp_call(
            "completion",
            "parent-diag-1",
            json!({"action":"diagnostics","file":"src/helper.lspfixture","timeout":10})
        )]),
    );
    let (request, body) = lsp_receive(&provider, "completion");
    let first = lsp_tool_result("completion", &body, "parent-diag-1");
    let parent_owner = first["diagnostics"]["owner"].as_str().unwrap().to_owned();
    lsp_calls_reply(
        request,
        "completion",
        json!([call(
            "spawn-owner",
            "task",
            json!({"context":"owner check","tasks":[{"agent":"scout","task":"diagnostics only"}]})
        )]),
    );
    let mut child_owner = None;
    for _ in 0..20 {
        let (request, body) = lsp_receive(&provider, "completion");
        if lsp_is_child(&body) {
            let outputs = lsp_output_ids("completion", &body);
            if !outputs.contains("child-diag") {
                lsp_calls_reply(
                    request,
                    "completion",
                    json!([call(
                        "child-diag",
                        "lsp",
                        json!({"action":"diagnostics","file":"src/helper.lspfixture","timeout":10})
                    )]),
                );
            } else {
                let result = lsp_tool_result("completion", &body, "child-diag");
                let owner = result["diagnostics"]["owner"].as_str().unwrap().to_owned();
                assert_ne!(owner, parent_owner, "child reused the parent owner");
                child_owner = Some(owner.clone());
                lsp_final(request, "completion", &format!("child owner {owner}"));
            }
        } else {
            let outputs = lsp_output_ids("completion", &body);
            if outputs.contains("parent-diag-2") {
                let result = lsp_tool_result("completion", &body, "parent-diag-2");
                let owner = result["diagnostics"]["owner"].as_str().unwrap();
                assert_eq!(owner, parent_owner);
                assert_ne!(owner, child_owner.as_deref().unwrap());
                lsp_final(request, "completion", "owner reports done");
                break;
            }
            if child_owner.is_some() {
                lsp_calls_reply(
                    request,
                    "completion",
                    json!([lsp_call(
                        "completion",
                        "parent-diag-2",
                        json!({"action":"diagnostics","file":"src/helper.lspfixture","timeout":10})
                    )]),
                );
            } else {
                lsp_final(request, "completion", "waiting for child owner");
            }
        }
    }
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    let events = lsp_wait_started(&log);
    let pids = lsp_started_pids(&events);
    assert_eq!(
        pids.len(),
        1,
        "same-cwd child must share the parent PID: {events:?}"
    );
    lsp_assert_pid_dead(pids[0]);
    no_more(&provider);
}

/// isolated 子 Agent 使用 prepare 后的独立 cwd：不同根、不同服务器 PID。
#[test]
fn lsp_roundtrip_isolated_subagent_uses_separate_client() {
    let provider = server();
    let project = tempfile::tempdir().unwrap();
    lsp_project(project.path());
    let status = Command::new("git")
        .current_dir(project.path())
        .args(["init", "-q"])
        .status()
        .unwrap();
    assert!(status.success());
    for (key, value) in [
        ("user.email", "fixture@example.com"),
        ("user.name", "fixture"),
    ] {
        let status = Command::new("git")
            .current_dir(project.path())
            .args(["config", key, value])
            .status()
            .unwrap();
        assert!(status.success());
    }
    let status = Command::new("git")
        .current_dir(project.path())
        .args(["add", "."])
        .status()
        .unwrap();
    assert!(status.success());
    let status = Command::new("git")
        .current_dir(project.path())
        .args(["commit", "-qm", "fixture"])
        .status()
        .unwrap();
    assert!(status.success());

    let log = project.path().join("lsp-log.jsonl");
    let barrier = project.path().join("barriers");
    lsp_write_config(project.path(), &log, &barrier, &[]);
    let cli = Cli::start_with(
        project.path(),
        Some("isolated client"),
        &lsp_settings(false, false),
        &lsp_models(&provider, "completion"),
    );
    let (request, _) = lsp_receive(&provider, "completion");
    lsp_calls_reply(
        request,
        "completion",
        json!([lsp_call(
            "completion",
            "parent-def",
            json!({"action":"definition","file":"src/lib.lspfixture","line":3,"symbol":"value","timeout":10})
        )]),
    );
    let (request, body) = lsp_receive(&provider, "completion");
    let parent = lsp_tool_result("completion", &body, "parent-def");
    assert_eq!(parent["ok"], true, "{parent}");
    lsp_calls_reply(
        request,
        "completion",
        json!([call(
            "spawn-isolated",
            "task",
            json!({"context":"isolated","tasks":[{"agent":"scout","task":"isolated lookup","isolated":true}]})
        )]),
    );
    let mut child_done = false;
    for _ in 0..20 {
        let (request, body) = lsp_receive(&provider, "completion");
        if lsp_is_child(&body) {
            let outputs = lsp_output_ids("completion", &body);
            if !outputs.contains("child-def") {
                lsp_calls_reply(
                    request,
                    "completion",
                    json!([call(
                        "child-def",
                        "lsp",
                        json!({"action":"definition","file":"src/lib.lspfixture","line":3,"symbol":"value","timeout":10})
                    )]),
                );
            } else {
                let result = lsp_tool_result("completion", &body, "child-def");
                assert_eq!(result["ok"], true, "{result}");
                assert!(
                    result["content"]
                        .as_str()
                        .unwrap()
                        .contains("src/helper.lspfixture:1:8"),
                    "{result}"
                );
                child_done = true;
                lsp_final(request, "completion", "isolated child done");
            }
        } else if child_done && body.to_string().contains("isolated child done") {
            lsp_final(request, "completion", "isolated parent done");
            break;
        } else {
            lsp_final(request, "completion", "waiting for isolated child");
        }
    }
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    let events = lsp_wait_log(&log, "two isolated starts", |events| {
        lsp_started_pids(events).len() >= 2
    });
    let pids = lsp_started_pids(&events);
    assert_eq!(pids.len(), 2, "{events:?}");
    assert_ne!(pids[0], pids[1]);
    let roots: Vec<PathBuf> = events
        .iter()
        .filter(|event| lsp_started(event))
        .map(|event| fs::canonicalize(event["cwd"].as_str().unwrap()).unwrap())
        .collect();
    let source = fs::canonicalize(project.path()).unwrap();
    assert_eq!(roots.iter().filter(|root| **root == source).count(), 1);
    assert_eq!(roots.iter().filter(|root| **root != source).count(), 1);
    for pid in pids {
        lsp_assert_pid_dead(pid);
    }
    no_more(&provider);
}

#[test]
fn lsp_settings_invalid_timeout_and_unknown_fields_reject_before_request() {
    let provider = server();
    for setting in [
        "lsp: {requestTimeout: 4}",
        "lsp: {formatOnWrite: true}",
        "lsp: {requestTimeout: 301}",
        "lsp: {requestTimeout: 5.5}",
        "lsp: {shared: secret-marker}",
        "lsp: {enabled: secret-marker}",
    ] {
        let home = tempfile::tempdir().unwrap();
        let output = Cli::start_with(
            home.path(),
            Some("hello"),
            setting,
            &fixture_models(&provider),
        )
        .finish();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            !output.status.success(),
            "invalid LSP setting accepted: {setting}"
        );
        assert!(output.stdout.is_empty());
        assert!(!stderr.contains("secret-marker"), "{stderr}");
    }
    no_more(&provider);
}
