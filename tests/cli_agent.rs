use axum::{body::Body, response::Response};
use pp_test_support::{CapturedRequest, SseWriter, TestServer};
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Child, Command, Output, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

#[path = "cli_agent/auth.rs"]
mod auth;
#[path = "cli_agent/shell_cli.rs"]
mod shell_cli;
#[path = "cli_agent/stats.rs"]
mod stats;

#[path = "cli_agent/approval.rs"]
mod approval;
#[path = "cli_agent/ask.rs"]
mod ask;
#[path = "cli_agent/lsp.rs"]
mod lsp;
#[path = "cli_agent/mcp.rs"]
mod mcp;
#[path = "cli_agent/mcp_remote.rs"]
mod mcp_remote;
#[path = "cli_agent/subagents.rs"]
mod subagents;

// 全部 `/test-*` 命令只在 debug assertions 构建中注册，因此这些用例随同一
// profile 边界编译，release 测试构建不再要求命令存在。
#[cfg(debug_assertions)]
#[path = "cli_agent/mock_test.rs"]
mod mock_test;

fn config_path(home: &std::path::Path, name: &str) -> std::path::PathBuf {
    let dir = home.join(".pp/agent");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

// 夹具允许把 MCP 与其他设置一起传入；实际落盘始终使用独立 JSON。
fn write_settings(home: &std::path::Path, text: impl AsRef<str>) -> std::io::Result<()> {
    let text = text.as_ref();
    if let Ok(mut value) = serde_json::from_str::<Value>(text)
        && let Some(servers) = value
            .as_object_mut()
            .and_then(|map| map.remove("mcpServers"))
    {
        std::fs::write(
            config_path(home, "mcp.json"),
            json!({"mcpServers":servers}).to_string(),
        )?;
        return std::fs::write(config_path(home, "settings.yml"), value.to_string());
    }
    std::fs::write(config_path(home, "settings.yml"), text)
}

// A user Markdown command under the isolated HOME's commands directory.
fn install_command(home: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
    let dir = config_path(home, "commands");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    path
}

struct Cli(Option<Child>);
impl Cli {
    fn start_with(
        cwd: &std::path::Path,
        prompt: Option<&str>,
        settings: &str,
        models: &str,
    ) -> Self {
        write_settings(cwd, settings).unwrap();
        std::fs::write(config_path(cwd, "models.yml"), models).unwrap();
        Self::start_with_existing_files(cwd, prompt)
    }
    fn start_with_existing_files(cwd: &std::path::Path, prompt: Option<&str>) -> Self {
        let mut args = Vec::new();
        if let Some(prompt) = prompt {
            args.extend(["-p", prompt]);
        }
        Self::start_env(cwd, cwd, &args, &[])
    }
    // 独立 HOME 与工作目录，或向行模式子进程注入代理等环境变量的入口。
    fn start_env(
        cwd: &std::path::Path,
        home: &std::path::Path,
        args: &[&str],
        envs: &[(&str, &str)],
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pp"));
        command
            .current_dir(cwd)
            .env("HOME", home)
            .args(args)
            .envs(envs.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self(Some(command.spawn().unwrap()))
    }
    fn start(server: &TestServer, cwd: &std::path::Path, prompt: Option<&str>) -> Self {
        Self::start_with(cwd, prompt, fixture_settings(), &fixture_models(server))
    }
    fn spawn(cwd: &std::path::Path, args: &[&str], envs: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pp"));
        command
            .current_dir(cwd)
            .env("HOME", cwd)
            .args(args)
            .envs(envs.iter().copied())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Self(Some(command.spawn().unwrap()))
    }
    fn input(&mut self, text: &str) {
        self.0
            .as_mut()
            .unwrap()
            .stdin
            .as_mut()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }
    fn pid(&self) -> u32 {
        self.0.as_ref().unwrap().id()
    }
    fn finish(self) -> Output {
        self.finish_within(Duration::from_secs(5))
    }
    // Mock 场景包含多次真实 Agent 运行与工具调用，宽限只用于这些显式入口。
    fn finish_within(mut self, timeout: Duration) -> Output {
        self.0.as_mut().unwrap().stdin.take();
        let deadline = Instant::now() + timeout;
        loop {
            if self.0.as_mut().unwrap().try_wait().unwrap().is_some() {
                return self.0.take().unwrap().wait_with_output().unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "CLI did not exit within {timeout:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Cli {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn fixture_settings() -> &'static str {
    "# 此测试进程的应用设置\nrate_limit_retry_budget_seconds: 0\n"
}

const FIXTURE_FAST_ROLE: &str = "model_roles:\n  fast: {model: fixture/primary, effort: none}\n";

fn task_fixture_settings() -> String {
    format!("{}{FIXTURE_FAST_ROLE}", fixture_settings())
}

fn fixture_models(server: &TestServer) -> String {
    format!(
        "# 此测试进程的回环服务\nproviders:\n  fixture:\n    protocol: completion\n    api_key: fixture-key\n    base_url: http://{}/v1\n    models:\n      primary:\n        model: fixture-model\n",
        server.addr()
    )
}

fn server() -> Arc<TestServer> {
    Arc::new(TestServer::start().unwrap())
}
fn receive(server: &TestServer) -> (CapturedRequest, Value) {
    receive_at(server, "/v1/chat/completions")
}
fn receive_raw(server: &TestServer) -> (CapturedRequest, Value) {
    receive_raw_within(server, Duration::from_secs(10))
}
fn receive_raw_within(server: &TestServer, timeout: Duration) -> (CapturedRequest, Value) {
    // 默认并行档会同时运行多个真实二进制与 PTY；冷启动可超过 5s，但请求仍应确定到达。
    let request = server
        .recv_timeout(timeout)
        .unwrap()
        .expect("missing CLI request");
    let body = request.json().unwrap();
    (request, body)
}
fn is_task_title(body: &Value) -> bool {
    body["instructions"]
        .as_str()
        .or_else(|| body["messages"][0]["content"].as_str())
        .is_some_and(|text| {
            text.starts_with("Generate a short task title for a terminal statusline.")
        })
}
fn receive_at(server: &TestServer, path: &str) -> (CapturedRequest, Value) {
    let (request, body) = receive_raw(server);
    assert_eq!(request.target(), path);
    (request, body)
}

fn chat_chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id":"chatcmpl-fixture", "object":"chat.completion.chunk",
        "created":1, "model":"fixture-model",
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
    })
}

fn reply(request: CapturedRequest, mut message: Value) {
    let finish_reason = if message.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
        for (index, call) in calls.iter_mut().enumerate() {
            call["index"] = json!(index);
        }
    }
    let mut stream = request.start_sse().unwrap();
    stream.event(&chat_chunk(message, None)).unwrap();
    stream
        .event(&chat_chunk(json!({}), Some(finish_reason)))
        .unwrap();
    stream.frame("[DONE]").unwrap();
    stream.finish();
}
fn final_reply(request: CapturedRequest, text: &str) {
    reply(request, json!({"role":"assistant","content":text}));
}
fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})
}
fn calls_reply(request: CapturedRequest, calls: Value) {
    reply(
        request,
        json!({"role":"assistant","content":null,"tool_calls":calls}),
    );
}
fn result(message: &Value, id: &str) -> Value {
    assert_eq!(message["role"], "tool");
    assert_eq!(message["tool_call_id"], id);
    serde_json::from_str(message["content"].as_str().unwrap()).unwrap()
}
fn no_more(server: &TestServer) {
    assert!(
        server
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none(),
        "unexpected extra model request"
    );
}

fn authorization(request: &CapturedRequest) -> String {
    request
        .headers()
        .get("authorization")
        .expect("认证请求必须带 Authorization")
        .to_str()
        .unwrap()
        .to_owned()
}

fn response_models(server: &TestServer, protocol: Option<&str>) -> String {
    let mut provider = json!({
        "api_key": "fixture-key",
        "base_url": format!("http://{}/v1", server.addr()),
        "models": {"primary": { "model": "fixture-model"}}
    });
    if let Some(protocol) = protocol {
        provider["protocol"] = json!(protocol);
    }
    json!({"providers": {"fixture": provider}}).to_string()
}

fn response_body(output: Value) -> Value {
    // Required fields follow async-openai 0.42.2 types/responses/response.rs.
    json!({
        "id": "resp-fixture",
        "object": "response",
        "created_at": 1,
        "model": "fixture-model",
        "status": "completed",
        "output": output
    })
}

fn response_reply(request: CapturedRequest, output: Value) {
    let mut stream = request.start_sse().unwrap();
    let mut sequence = 0;
    for (output_index, item) in output.as_array().unwrap().iter().enumerate() {
        if let Some(parts) = item["content"].as_array() {
            for (content_index, part) in parts.iter().enumerate() {
                let (event_type, text) = match part["type"].as_str() {
                    Some("output_text") => ("response.output_text.delta", part["text"].as_str()),
                    Some("refusal") => ("response.refusal.delta", part["refusal"].as_str()),
                    _ => continue,
                };
                stream
                    .event(&json!({
                        "type":event_type, "sequence_number":sequence,
                        "item_id":item["id"], "output_index":output_index,
                        "content_index":content_index, "delta":text.unwrap(),
                        "logprobs":[]
                    }))
                    .unwrap();
                sequence += 1;
                stream
                    .event(&json!({
                        "type":"response.content_part.done", "sequence_number":sequence,
                        "item_id":item["id"], "output_index":output_index,
                        "content_index":content_index, "part":part
                    }))
                    .unwrap();
                sequence += 1;
            }
        }
        stream
            .event(&json!({
                "type":"response.output_item.done", "sequence_number":sequence,
                "output_index":output_index, "item":item
            }))
            .unwrap();
        sequence += 1;
    }
    stream
        .event(&json!({
            "type":"response.completed", "sequence_number":sequence,
            "response":response_body(output)
        }))
        .unwrap();
    stream.finish();
}

fn response_message(id: &str, content: Value) -> Value {
    json!({
        "type": "message",
        "id": id,
        "role": "assistant",
        "phase": "final_answer",
        "status": "completed",
        "content": content
    })
}

fn response_text(text: &str) -> Value {
    json!({"type": "output_text", "text": text, "annotations": [], "logprobs": null})
}

fn response_final(request: CapturedRequest, text: &str) {
    response_reply(
        request,
        json!([response_message("msg-final", json!([response_text(text)]))]),
    );
}

fn response_call(id: &str, name: &str, args: Value) -> Value {
    json!({
        "type": "function_call",
        "id": format!("fc-{id}"),
        "call_id": id,
        "name": name,
        "arguments": args.to_string(),
        "status": "completed"
    })
}

fn response_result(item: &Value, id: &str) -> Value {
    assert_eq!(item["type"], "function_call_output");
    assert_eq!(item["call_id"], id);
    serde_json::from_str(item["output"].as_str().unwrap()).unwrap()
}

fn response_user(item: &Value, expected: &str) {
    assert_eq!(item["role"], "user");
    match &item["content"] {
        Value::String(text) => assert_eq!(text, expected),
        Value::Array(parts) => {
            assert_eq!(parts.len(), 1);
            assert_eq!(parts[0]["type"], "input_text");
            assert_eq!(parts[0]["text"], expected);
        }
        content => panic!("unexpected user content: {content}"),
    }
}

fn slash_provider(server: &TestServer, id: &str, protocol: &str, models: Value) -> Value {
    json!({
        "protocol": protocol,
        "api_key": format!("{id}-key"),
        "base_url": format!("http://{}/v1", server.addr()),
        "models": models
    })
}

fn slash_message_text(item: &Value) -> String {
    match &item["content"] {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .map(|part| match part["type"].as_str().unwrap() {
                "input_text" | "output_text" => part["text"].as_str().unwrap(),
                "refusal" => part["refusal"].as_str().unwrap(),
                other => panic!("unexpected text content type: {other}"),
            })
            .collect(),
        content => panic!("unexpected message content: {content}"),
    }
}

fn slash_assert_message(item: &Value, role: &str, text: &str) {
    assert_eq!(item["role"], role);
    assert_eq!(slash_message_text(item), text);
}

fn slash_assert_portable(input: &[Value]) {
    for item in input {
        assert_ne!(item["type"], "reasoning");
        for field in [
            "id",
            "status",
            "namespace",
            "caller",
            "async",
            "phase",
            "encrypted_content",
        ] {
            assert!(
                item.get(field).is_none_or(Value::is_null),
                "nonportable {field}: {item}"
            );
        }
    }
}

fn slash_response_batch() -> Value {
    let mut commentary = response_message(
        "msg-commentary",
        json!([response_text("response commentary")]),
    );
    commentary["phase"] = json!("commentary");
    json!([
        commentary,
        response_call("response-write", "write", json!({"path":"response.txt","content":"beta"})),
        {
            "type": "reasoning",
            "id": "reasoning-private-id",
            "summary": [
                {"type":"summary_text","text":"summary one"},
                {"type":"summary_text","text":"summary two"}
            ],
            "content": [{"type":"reasoning_text","text":"reasoning plain"}],
            "encrypted_content": "opaque-switch-marker",
            "status": "completed"
        },
        response_message("msg-mixed", json!([
            response_text("before"),
            {"type":"refusal","refusal":"refused"},
            response_text("after")
        ])),
        response_call("response-read", "read", json!({"path":"response.txt"}))
    ])
}

fn slash_assert_response_calls(items: &[Value], calls: &Value) {
    assert_eq!(items.len(), calls.as_array().unwrap().len());
    for (item, call) in items.iter().zip(calls.as_array().unwrap()) {
        assert_eq!(item["type"], "function_call");
        assert_eq!(item["call_id"], call["id"]);
        assert_eq!(item["name"], call["function"]["name"]);
        assert_eq!(item["arguments"], call["function"]["arguments"]);
    }
}

fn slash_assert_merged_response_batch(message: &Value) {
    slash_assert_message(
        message,
        "assistant",
        "response commentary\nsummary one\nsummary two\nreasoning plain\nbeforerefusedafter",
    );
    assert_eq!(
        message["tool_calls"],
        json!([
            call(
                "response-write",
                "write",
                json!({"path":"response.txt","content":"beta"})
            ),
            call("response-read", "read", json!({"path":"response.txt"}))
        ])
    );
}

const PROCESS_DEADLINE: Duration = Duration::from_secs(10);

// 等待 bash 写出 PID marker 文件：进入执行阶段的确定性栅栏。
// CLI 与 TUI 测试共用，后台与取消路径都以它为进程级屏障。
fn cancel_wait_pid_files(project: &std::path::Path, names: &[&str]) {
    let deadline = Instant::now() + PROCESS_DEADLINE;
    loop {
        if names.iter().all(|name| project.join(name).is_file()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "bash PID marker 未出现：{names:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn cancel_wait_pid_dead(path: &std::path::Path) {
    let pid: i32 = std::fs::read_to_string(path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    let deadline = Instant::now() + PROCESS_DEADLINE;
    while rustix::process::test_kill_process(pid).is_ok() {
        assert!(Instant::now() < deadline, "进程 {pid:?} 仍在运行");
        thread::sleep(Duration::from_millis(10));
    }
}

// bash scratch 目录建在任务 cwd 下；结束或取消收尾必须清理干净。
fn cancel_assert_no_scratch(project: &std::path::Path) {
    let deadline = Instant::now() + PROCESS_DEADLINE;
    loop {
        let leftover: Vec<_> = std::fs::read_dir(project)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".pp-tmp-"))
            .collect();
        if leftover.is_empty() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "bash scratch 目录未清理：{leftover:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

// 会话日志按每行一个共享值表记录写盘；这些 helper 是测试侧唯一的解码与再编码
// 入口，断言必须看逻辑记录，不能直接解析磁盘行。
fn session_text(path: &std::path::Path) -> String {
    let mut reader = pp_core::session_jsonl::Reader::new(std::io::BufReader::new(
        std::fs::File::open(path).unwrap(),
    ));
    let mut line = Vec::new();
    let mut result = Vec::new();
    while reader.read_record(&mut line).unwrap() != 0 {
        result.extend_from_slice(&line);
    }
    String::from_utf8(result).unwrap()
}

// 未以换行结尾的尾部记录是唯一允许忽略的损坏，读取时直接排除。
fn records(path: &std::path::Path) -> Vec<Value> {
    let text = session_text(path);
    let terminated = match text.rfind('\n') {
        Some(index) if !text.ends_with('\n') => &text[..index + 1],
        _ => text.as_str(),
    };
    terminated
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("invalid record line: {error}"))
        })
        .collect()
}

// 重写整份日志：首行头部原样保留，其余逻辑行用新的共享值表重新编码。
fn write_session_text(path: &std::path::Path, text: &str) {
    let mut storage = pp_core::session_jsonl::Storage::default();
    let mut output = Vec::new();
    for (index, line) in text.split_inclusive('\n').enumerate() {
        if index == 0 {
            output.extend_from_slice(line.as_bytes());
        } else if let Ok(value) = serde_json::from_str::<Value>(line) {
            output.extend(storage.encode(&value).unwrap());
            if line.ends_with('\n') {
                output.push(b'\n');
            }
        } else {
            output.extend_from_slice(line.as_bytes());
        }
    }
    std::fs::write(path, output).unwrap();
}

// 工作区外夹具：受管临时根（`/tmp`）内的目录对保存层仍是可写目标，逃逸断言必须
// 落在两个受管根之外；`/var/tmp` 在 macOS 与 Linux 上均可写且不在受管根内。
fn outside_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new().tempdir_in("/var/tmp").unwrap()
}

#[path = "cli_agent/context.rs"]
mod context;
#[path = "cli_agent/project_instructions.rs"]
mod project_instructions;
#[path = "cli_agent/streaming.rs"]
mod streaming;
#[path = "cli_agent/system_prompt.rs"]
mod system_prompt;
#[path = "cli_agent/think.rs"]
mod think;
#[cfg(unix)]
#[path = "cli_agent/tui.rs"]
mod tui;

#[path = "cli_agent/skills.rs"]
mod skills;

#[path = "cli_agent/session.rs"]
mod session;

#[path = "cli_agent/resume_cli.rs"]
mod resume_cli;

#[path = "cli_agent/logging.rs"]
mod logging;

#[path = "cli_agent/todo.rs"]
mod todo;

#[path = "cli_agent/command_cases.rs"]
mod command_cases;

#[path = "cli_agent/model_switch_cases.rs"]
mod model_switch_cases;

#[path = "cli_agent/model_arg_cases.rs"]
mod model_arg_cases;

#[path = "cli_agent/response_cases.rs"]
mod response_cases;

#[path = "cli_agent/tool_cases.rs"]
mod tool_cases;

#[path = "cli_agent/retry_cases.rs"]
mod retry_cases;

#[path = "cli_agent/config_model_cases.rs"]
mod config_model_cases;

#[path = "cli_agent/config_error_cases.rs"]
mod config_error_cases;

#[path = "cli_agent/sqlite.rs"]
mod sqlite;

#[path = "cli_agent/fallback_cases.rs"]
mod fallback_cases;
