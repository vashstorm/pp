use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};
use tiny_http::{Request, Response, Server};

struct Cli(Option<Child>);
impl Cli {
    fn start_with(
        cwd: &std::path::Path,
        prompt: Option<&str>,
        settings: &str,
        models: &str,
    ) -> Self {
        std::fs::write(cwd.join("settings.jsonc"), settings).unwrap();
        std::fs::write(cwd.join("models.jsonc"), models).unwrap();
        Self::start_with_existing_files(cwd, prompt)
    }
    fn start_with_existing_files(cwd: &std::path::Path, prompt: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pp"));
        command
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(prompt) = prompt {
            command.args(["-p", prompt]);
        }
        Self(Some(command.spawn().unwrap()))
    }
    fn start(server: &Server, cwd: &std::path::Path, prompt: Option<&str>) -> Self {
        Self::start_with(cwd, prompt, fixture_settings(), &fixture_models(server))
    }
    fn spawn(cwd: &std::path::Path, args: &[&str], envs: &[(&str, &str)]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_pp"));
        command
            .current_dir(cwd)
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
    fn finish(mut self) -> Output {
        self.0.as_mut().unwrap().stdin.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.0.as_mut().unwrap().try_wait().unwrap().is_some() {
                return self.0.take().unwrap().wait_with_output().unwrap();
            }
            assert!(
                Instant::now() < deadline,
                "CLI did not exit within five seconds"
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
    "{\n  // App behavior for this process only.\n  \"default_model\": \"fixture/primary\",\n}\n"
}

fn fixture_models(server: &Server) -> String {
    format!(
        "{{\n// Loopback provider for this process only.\n\"providers\":[{{\"id\":\"fixture\",\"protocol\":\"completion\",\"api_key\":\"fixture-key\",\"base_url\":\"http://{}/v1\",\"models\":[{{\"id\":\"primary\",\"model\":\"fixture-model\"}}]}}],\n}}\n",
        server.server_addr()
    )
}

fn server() -> Server {
    Server::http("127.0.0.1:0").unwrap()
}
fn receive(server: &Server) -> (Request, Value) {
    receive_at(server, "/v1/chat/completions")
}
fn receive_at(server: &Server, path: &str) -> (Request, Value) {
    let mut request = server
        .recv_timeout(Duration::from_secs(5))
        .unwrap()
        .expect("missing CLI request");
    assert_eq!(request.url(), path);
    let body = serde_json::from_reader(request.as_reader()).unwrap();
    (request, body)
}
struct SseReply {
    writer: Box<dyn Write + Send>,
}

impl SseReply {
    fn new(request: Request) -> Self {
        let mut writer = request.into_writer();
        writer.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
        writer.flush().unwrap();
        Self { writer }
    }

    fn frame(&mut self, data: &str) {
        let frame = format!("data: {data}\n\n");
        write!(self.writer, "{:x}\r\n{}\r\n", frame.len(), frame).unwrap();
        self.writer.flush().unwrap();
    }

    fn event(&mut self, event: Value) {
        self.frame(&event.to_string());
    }

    fn finish(mut self) {
        self.writer.write_all(b"0\r\n\r\n").unwrap();
        self.writer.flush().unwrap();
    }
}

fn chat_chunk(delta: Value, finish: Option<&str>) -> Value {
    json!({
        "id":"chatcmpl-fixture", "object":"chat.completion.chunk",
        "created":1, "model":"fixture-model",
        "choices":[{"index":0,"delta":delta,"finish_reason":finish}]
    })
}

fn reply(request: Request, mut message: Value) {
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
    let mut stream = SseReply::new(request);
    stream.event(chat_chunk(message, None));
    stream.event(chat_chunk(json!({}), Some(finish_reason)));
    stream.frame("[DONE]");
    stream.finish();
}
fn final_reply(request: Request, text: &str) {
    reply(request, json!({"role":"assistant","content":text}));
}
fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})
}
fn calls_reply(request: Request, calls: Value) {
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
fn no_more(server: &Server) {
    assert!(
        server
            .recv_timeout(Duration::from_millis(100))
            .unwrap()
            .is_none(),
        "unexpected extra model request"
    );
}
fn authorization(request: &Request) -> String {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv("Authorization"))
        .map(|header| header.value.as_str().to_owned())
        .unwrap()
}

fn response_models(server: &Server, protocol: Option<&str>) -> String {
    let mut provider = json!({
        "id": "fixture",
        "api_key": "fixture-key",
        "base_url": format!("http://{}/v1", server.server_addr()),
        "models": [{"id": "primary", "model": "fixture-model"}]
    });
    if let Some(protocol) = protocol {
        provider["protocol"] = json!(protocol);
    }
    json!({"providers": [provider]}).to_string()
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

fn response_reply(request: Request, output: Value) {
    let mut stream = SseReply::new(request);
    let mut sequence = 0;
    for (output_index, item) in output.as_array().unwrap().iter().enumerate() {
        if let Some(parts) = item["content"].as_array() {
            for (content_index, part) in parts.iter().enumerate() {
                let (event_type, text) = match part["type"].as_str() {
                    Some("output_text") => ("response.output_text.delta", part["text"].as_str()),
                    Some("refusal") => ("response.refusal.delta", part["refusal"].as_str()),
                    _ => continue,
                };
                stream.event(json!({
                    "type":event_type, "sequence_number":sequence,
                    "item_id":item["id"], "output_index":output_index,
                    "content_index":content_index, "delta":text.unwrap(),
                    "logprobs":[]
                }));
                sequence += 1;
                stream.event(json!({
                    "type":"response.content_part.done", "sequence_number":sequence,
                    "item_id":item["id"], "output_index":output_index,
                    "content_index":content_index, "part":part
                }));
                sequence += 1;
            }
        }
        stream.event(json!({
            "type":"response.output_item.done", "sequence_number":sequence,
            "output_index":output_index, "item":item
        }));
        sequence += 1;
    }
    stream.event(json!({
        "type":"response.completed", "sequence_number":sequence,
        "response":response_body(output)
    }));
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

fn response_final(request: Request, text: &str) {
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

fn slash_provider(server: &Server, id: &str, protocol: &str, models: Value) -> Value {
    json!({
        "id": id,
        "protocol": protocol,
        "api_key": format!("{id}-key"),
        "base_url": format!("http://{}/v1", server.server_addr()),
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

#[test]
fn slash_local_commands_and_unavailable_help_never_request_models() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": [
        slash_provider(&a, "a", "completion", json!([
            {"id": "primary", "model": "vendor-a"}
        ])),
        slash_provider(&b, "gateway", "response", json!([
            {"id": "team/secondary", "model": "vendor-b"}
        ]))
    ]})
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        r#"{"default_model":"a/primary"}"#,
        &models,
    );
    cli.input("/help\n/model\n/bogus\n/model missing/nope\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    for command in ["/help", "/model", "/new", "/quit"] {
        assert!(stderr.contains(command), "{stderr}");
    }
    for value in [
        "a/primary",
        "vendor-a",
        "completion",
        "gateway/team/secondary",
        "vendor-b",
        "response",
        "unknown command; use /help",
        "model not found; use /model to list configured models",
    ] {
        assert!(stderr.contains(value), "{stderr}");
    }
    let listing = stderr.find("a/primary").unwrap();
    assert!(listing < stderr.find("gateway/team/secondary").unwrap());
    assert!(
        stderr
            .lines()
            .any(|line| line.contains('*') && line.contains("a/primary"))
    );
    assert!(!stderr.contains("a-key"));
    assert!(!stderr.contains("gateway-key"));
    no_more(&a);
    no_more(&b);

    let unavailable = tempfile::tempdir().unwrap();
    let mut cli = Cli::start_with(unavailable.path(), None, "{}", "{}");
    cli.input("/help\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    for command in ["/help", "/model", "/new", "/quit"] {
        assert!(stderr.contains(command), "{stderr}");
    }
    no_more(&a);
    no_more(&b);
}

#[test]
fn model_switch_completion_alias_routing_preserves_tools_and_reset_selection() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": [
        slash_provider(&a, "a", "completion", json!([
            {"id": "primary", "model": "vendor-a"}
        ])),
        slash_provider(&b, "gateway", "completion", json!([
            {"id": "team/secondary", "model": "vendor-b"}
        ]))
    ]})
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        r#"{"default_model":"a/primary"}"#,
        &models,
    );
    cli.input("write then read alpha\n");
    let (request, body) = receive(&a);
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a");
    let system = body["messages"][0].clone();
    let calls = json!([
        call(
            "write-alpha",
            "write",
            json!({"path":"alpha.txt","content":"alpha"})
        ),
        call("read-alpha", "read", json!({"path":"alpha.txt"}))
    ]);
    calls_reply(request, calls.clone());
    let (request, body) = receive(&a);
    assert_eq!(body["messages"][2]["tool_calls"], calls);
    assert_eq!(
        result(&body["messages"][3], "write-alpha")["bytes_written"],
        5
    );
    assert_eq!(result(&body["messages"][4], "read-alpha")["ok"], true);
    let history = body["messages"].as_array().unwrap().clone();
    final_reply(request, "alpha saved");

    // A sentinel file makes replaying an old write observable.
    std::fs::write(dir.path().join("alpha.txt"), "do not replay").unwrap();
    cli.input("/model gateway/team/secondary\nrecall alpha\n");
    let (request, body) = receive(&b);
    assert_eq!(authorization(&request), "Bearer gateway-key");
    assert_eq!(body["model"], "vendor-b");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..history.len()], history.as_slice());
    slash_assert_message(&messages[5], "assistant", "alpha saved");
    slash_assert_message(&messages[6], "user", "recall alpha");
    assert_eq!(messages.len(), 7);
    final_reply(request, "alpha recalled");

    cli.input("/model gateway/team/secondary\nrecall again\n");
    let (request, body) = receive(&b);
    assert_eq!(body["model"], "vendor-b");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..history.len()], history.as_slice());
    slash_assert_message(&messages[7], "assistant", "alpha recalled");
    slash_assert_message(&messages[8], "user", "recall again");
    assert_eq!(messages.len(), 9);
    final_reply(request, "alpha again");

    cli.input("/new\nfresh task\n");
    let (request, body) = receive(&b);
    assert_eq!(authorization(&request), "Bearer gateway-key");
    assert_eq!(body["model"], "vendor-b");
    assert_eq!(body["messages"][0], system);
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    slash_assert_message(&body["messages"][1], "user", "fresh task");
    final_reply(request, "fresh on b");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"alpha saved\nalpha recalled\nalpha again\nfresh on b\n"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr
            .matches("Current model: gateway/team/secondary (vendor-b)")
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("alpha.txt")).unwrap(),
        "do not replay"
    );
    no_more(&a);
    no_more(&b);
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

#[test]
fn model_switch_completion_response_completion_preserves_full_tool_batches() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": [
        slash_provider(&a, "a", "completion", json!([{"id":"primary","model":"vendor-a"}])),
        slash_provider(&b, "b", "response", json!([{"id":"secondary","model":"vendor-b"}]))
    ]})
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        r#"{"default_model":"a/primary"}"#,
        &models,
    );
    cli.input("completion first task\n");
    let (request, body) = receive(&a);
    let system = body["messages"][0].clone();
    let calls = json!([
        call(
            "completion-write",
            "write",
            json!({"path":"completion.txt","content":"alpha"})
        ),
        call("completion-read", "read", json!({"path":"completion.txt"}))
    ]);
    let assistant = json!({
        "role":"assistant", "content":"completion commentary", "tool_calls":calls
    });
    reply(request, assistant.clone());
    let (request, body) = receive(&a);
    assert_eq!(body["messages"][2], assistant);
    assert_eq!(
        result(&body["messages"][3], "completion-write")["bytes_written"],
        5
    );
    assert_eq!(result(&body["messages"][4], "completion-read")["ok"], true);
    let completion_history = body["messages"].as_array().unwrap().clone();
    final_reply(request, "completion final");
    std::fs::write(dir.path().join("completion.txt"), "completion sentinel").unwrap();

    cli.input("/model b/secondary\nresponse second task\n");
    let (request, body) = receive_at(&b, "/v1/responses");
    assert_eq!(authorization(&request), "Bearer b-key");
    assert_eq!(body["model"], "vendor-b");
    assert_eq!(body["instructions"], system["content"]);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 8);
    slash_assert_portable(input);
    response_user(&input[0], "completion first task");
    slash_assert_message(&input[1], "assistant", "completion commentary");
    slash_assert_response_calls(&input[2..4], &calls);
    for (index, old_index, id) in [(4, 3, "completion-write"), (5, 4, "completion-read")] {
        assert_eq!(
            input[index]["output"],
            completion_history[old_index]["content"]
        );
        assert_eq!(
            response_result(&input[index], id),
            result(&completion_history[old_index], id)
        );
    }
    slash_assert_message(&input[6], "assistant", "completion final");
    response_user(&input[7], "response second task");
    let second_initial = input.clone();
    let batch = slash_response_batch();
    response_reply(request, batch.clone());
    let (request, body) = receive_at(&b, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(&input[..8], second_initial.as_slice());
    assert_eq!(&input[8..13], batch.as_array().unwrap().as_slice());
    assert_eq!(
        response_result(&input[13], "response-write")["bytes_written"],
        4
    );
    assert_eq!(response_result(&input[14], "response-read")["ok"], true);
    let response_results = [input[13].clone(), input[14].clone()];
    response_final(request, "response final");
    std::fs::write(dir.path().join("response.txt"), "response sentinel").unwrap();

    cli.input("/model a/primary\ncompletion third task\n");
    let (request, body) = receive(&a);
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..5], completion_history.as_slice());
    assert_eq!(messages.len(), 12);
    slash_assert_message(&messages[5], "assistant", "completion final");
    slash_assert_message(&messages[6], "user", "response second task");
    slash_assert_merged_response_batch(&messages[7]);
    for (index, old, id) in [
        (8, &response_results[0], "response-write"),
        (9, &response_results[1], "response-read"),
    ] {
        assert_eq!(messages[index]["content"], old["output"]);
        assert_eq!(result(&messages[index], id), response_result(old, id));
    }
    slash_assert_message(&messages[10], "assistant", "response final");
    slash_assert_message(&messages[11], "user", "completion third task");
    assert!(!body.to_string().contains("opaque-switch-marker"));
    assert!(!body.to_string().contains("reasoning-private-id"));
    let third_history = messages.clone();
    let calls = json!([
        call(
            "third-write",
            "write",
            json!({"path":"third.txt","content":"gamma"})
        ),
        call("third-read", "read", json!({"path":"third.txt"}))
    ]);
    calls_reply(request, calls.clone());
    let (request, body) = receive(&a);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(&messages[..12], third_history.as_slice());
    assert_eq!(messages[12]["tool_calls"], calls);
    assert_eq!(result(&messages[13], "third-write")["bytes_written"], 5);
    assert_eq!(result(&messages[14], "third-read")["ok"], true);
    final_reply(request, "third final");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"completion commentary\ncompletion final\nresponse commentarybeforeafter\nresponse final\nthird final\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("completion.txt")).unwrap(),
        "completion sentinel"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("response.txt")).unwrap(),
        "response sentinel"
    );
    no_more(&a);
    no_more(&b);
}

#[test]
fn model_switch_responses_first_round_trip_keeps_text_reasoning_and_tools() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": [
        slash_provider(&a, "a", "response", json!([{"id":"primary","model":"vendor-a"}])),
        slash_provider(&b, "b", "completion", json!([{"id":"secondary","model":"vendor-b"}]))
    ]})
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        r#"{"default_model":"a/primary"}"#,
        &models,
    );
    cli.input("responses first task\n");
    let (request, body) = receive_at(&a, "/v1/responses");
    let instructions = body["instructions"].clone();
    let batch = slash_response_batch();
    response_reply(request, batch.clone());
    let (request, body) = receive_at(&a, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(&input[1..6], batch.as_array().unwrap().as_slice());
    assert_eq!(
        response_result(&input[6], "response-write")["bytes_written"],
        4
    );
    assert_eq!(response_result(&input[7], "response-read")["ok"], true);
    let outputs = [input[6].clone(), input[7].clone()];
    response_final(request, "responses first final");
    std::fs::write(dir.path().join("response.txt"), "response sentinel").unwrap();

    cli.input("/model b/secondary\ncompletion second task\n");
    let (request, body) = receive(&b);
    assert_eq!(authorization(&request), "Bearer b-key");
    assert_eq!(body["model"], "vendor-b");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 7);
    slash_assert_message(&messages[0], "system", instructions.as_str().unwrap());
    slash_assert_message(&messages[1], "user", "responses first task");
    slash_assert_merged_response_batch(&messages[2]);
    for (index, old, id) in [
        (3, &outputs[0], "response-write"),
        (4, &outputs[1], "response-read"),
    ] {
        assert_eq!(messages[index]["content"], old["output"]);
        assert_eq!(result(&messages[index], id), response_result(old, id));
    }
    slash_assert_message(&messages[5], "assistant", "responses first final");
    slash_assert_message(&messages[6], "user", "completion second task");
    assert!(!body.to_string().contains("opaque-switch-marker"));
    let calls = json!([
        call(
            "completion-write",
            "write",
            json!({"path":"completion.txt","content":"alpha"})
        ),
        call("completion-read", "read", json!({"path":"completion.txt"}))
    ]);
    reply(
        request,
        json!({"role":"assistant","content":"completion commentary","tool_calls":calls}),
    );
    let (request, body) = receive(&b);
    assert_eq!(body["messages"][7]["tool_calls"], calls);
    assert_eq!(
        result(&body["messages"][8], "completion-write")["bytes_written"],
        5
    );
    assert_eq!(result(&body["messages"][9], "completion-read")["ok"], true);
    let completion_outputs = [body["messages"][8].clone(), body["messages"][9].clone()];
    final_reply(request, "completion second final");
    std::fs::write(dir.path().join("completion.txt"), "completion sentinel").unwrap();

    cli.input("/model a/primary\nresponses third task\n");
    let (request, body) = receive_at(&a, "/v1/responses");
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a");
    assert_eq!(body["instructions"], instructions);
    let input = body["input"].as_array().unwrap();
    slash_assert_portable(input);
    assert_eq!(input.len(), 15);
    response_user(&input[0], "responses first task");
    slash_assert_message(
        &input[1],
        "assistant",
        "response commentary\nsummary one\nsummary two\nreasoning plain\nbeforerefusedafter",
    );
    let original_calls = json!([
        call(
            "response-write",
            "write",
            json!({"path":"response.txt","content":"beta"})
        ),
        call("response-read", "read", json!({"path":"response.txt"}))
    ]);
    slash_assert_response_calls(&input[2..4], &original_calls);
    assert_eq!(input[4]["output"], outputs[0]["output"]);
    assert_eq!(input[5]["output"], outputs[1]["output"]);
    assert_eq!(response_result(&input[4], "response-write")["ok"], true);
    assert_eq!(response_result(&input[5], "response-read")["ok"], true);
    slash_assert_message(&input[6], "assistant", "responses first final");
    response_user(&input[7], "completion second task");
    slash_assert_message(&input[8], "assistant", "completion commentary");
    slash_assert_response_calls(&input[9..11], &calls);
    for (index, old, id) in [
        (11, &completion_outputs[0], "completion-write"),
        (12, &completion_outputs[1], "completion-read"),
    ] {
        assert_eq!(input[index]["output"], old["content"]);
        assert_eq!(response_result(&input[index], id), result(old, id));
    }
    slash_assert_message(&input[13], "assistant", "completion second final");
    response_user(&input[14], "responses third task");
    assert!(!body.to_string().contains("opaque-switch-marker"));
    let third_initial = input.clone();
    let calls = json!([
        response_call(
            "third-write",
            "write",
            json!({"path":"third.txt","content":"gamma"})
        ),
        response_call("third-read", "read", json!({"path":"third.txt"}))
    ]);
    response_reply(request, calls.clone());
    let (request, body) = receive_at(&a, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(&input[..15], third_initial.as_slice());
    assert_eq!(&input[15..17], calls.as_array().unwrap().as_slice());
    assert_eq!(
        response_result(&input[17], "third-write")["bytes_written"],
        5
    );
    assert_eq!(response_result(&input[18], "third-read")["ok"], true);
    response_final(request, "responses third final");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"response commentarybeforeafter\nresponses first final\ncompletion commentary\ncompletion second final\nresponses third final\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("response.txt")).unwrap(),
        "response sentinel"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("completion.txt")).unwrap(),
        "completion sentinel"
    );
    no_more(&a);
    no_more(&b);
}

#[test]
fn model_switch_responses_native_replay_and_cross_provider_portability() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({"providers": [
        slash_provider(&a, "a", "response", json!([
            {"id":"primary","model":"vendor-a"},
            {"id":"second","model":"vendor-a-second"}
        ])),
        slash_provider(&b, "b", "response", json!([{"id":"secondary","model":"vendor-b"}]))
    ]})
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        r#"{"default_model":"a/primary"}"#,
        &models,
    );
    cli.input("native response task\n");
    let (request, body) = receive_at(&a, "/v1/responses");
    let instructions = body["instructions"].clone();
    let batch = slash_response_batch();
    response_reply(request, batch.clone());
    let (request, body) = receive_at(&a, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(&input[1..6], batch.as_array().unwrap().as_slice());
    assert_eq!(
        response_result(&input[6], "response-write")["bytes_written"],
        4
    );
    assert_eq!(response_result(&input[7], "response-read")["ok"], true);
    let native_history = input.clone();
    let final_message =
        response_message("msg-native-final", json!([response_text("native final")]));
    response_reply(request, json!([final_message.clone()]));
    std::fs::write(dir.path().join("response.txt"), "native sentinel").unwrap();

    cli.input("/model a/second\nsame provider task\n");
    let (request, body) = receive_at(&a, "/v1/responses");
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a-second");
    assert_eq!(body["instructions"], instructions);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 10);
    assert_eq!(&input[..8], native_history.as_slice());
    assert_eq!(input[8], final_message);
    response_user(&input[9], "same provider task");
    // This equality includes provider IDs, status, phase and opaque reasoning.
    assert_eq!(&input[1..6], batch.as_array().unwrap().as_slice());
    assert_eq!(input[3]["encrypted_content"], "opaque-switch-marker");
    response_final(request, "same provider final");

    cli.input("/model b/secondary\ncross provider task\n");
    let (request, body) = receive_at(&b, "/v1/responses");
    assert_eq!(authorization(&request), "Bearer b-key");
    assert_eq!(body["model"], "vendor-b");
    assert_eq!(body["instructions"], instructions);
    assert_eq!(body["store"], false);
    assert!(body.get("previous_response_id").is_none());
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 12);
    slash_assert_portable(input);
    response_user(&input[0], "native response task");
    slash_assert_message(&input[1], "assistant", "response commentary");
    assert_eq!(input[2]["type"], "function_call");
    assert_eq!(input[2]["call_id"], "response-write");
    assert_eq!(input[2]["name"], "write");
    assert_eq!(input[2]["arguments"], batch[1]["arguments"]);
    slash_assert_message(
        &input[3],
        "assistant",
        "summary one\nsummary two\nreasoning plain",
    );
    slash_assert_message(&input[4], "assistant", "beforerefusedafter");
    assert_eq!(input[5]["type"], "function_call");
    assert_eq!(input[5]["call_id"], "response-read");
    assert_eq!(input[5]["name"], "read");
    assert_eq!(input[5]["arguments"], batch[4]["arguments"]);
    for (index, id) in [(6, "response-write"), (7, "response-read")] {
        assert_eq!(input[index]["output"], native_history[index]["output"]);
        assert_eq!(
            response_result(&input[index], id),
            response_result(&native_history[index], id)
        );
    }
    slash_assert_message(&input[8], "assistant", "native final");
    response_user(&input[9], "same provider task");
    slash_assert_message(&input[10], "assistant", "same provider final");
    response_user(&input[11], "cross provider task");
    for marker in [
        "opaque-switch-marker",
        "reasoning-private-id",
        "msg-commentary",
        "msg-mixed",
        "msg-native-final",
        "fc-response-write",
        "fc-response-read",
    ] {
        assert!(
            !body["input"].to_string().contains(marker),
            "replayed {marker}"
        );
    }
    response_final(request, "portable final");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"response commentarybeforeafter\nnative final\nsame provider final\nportable final\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("response.txt")).unwrap(),
        "native sentinel"
    );
    no_more(&a);
    no_more(&b);
}

#[test]
fn model_switch_failure_is_atomic_and_catalog_is_a_read_only_startup_snapshot() {
    let a = server();
    let b = server();
    let bad = server();
    let dir = tempfile::tempdir().unwrap();
    let mut invalid_header = slash_provider(
        &bad,
        "bad",
        "response",
        json!([{"id":"broken","model":"vendor-broken"}]),
    );
    invalid_header["api_key"] = json!("switch-secret-marker\ninvalid-header");
    let models = json!({"providers": [
        slash_provider(&a, "a", "completion", json!([{"id":"primary","model":"vendor-a"}])),
        slash_provider(&b, "gateway", "completion", json!([
            {"id":"team/secondary","model":"vendor-b"}
        ])),
        invalid_header
    ]})
    .to_string();
    let settings = r#"{"default_model":"a/primary"}"#;
    let mut cli = Cli::start_with(dir.path(), None, settings, &models);
    cli.input("establish history\n");
    let (request, body) = receive(&a);
    assert_eq!(authorization(&request), "Bearer a-key");
    let system = body["messages"][0].clone();
    // The first request proves startup loading finished before files change.
    assert_eq!(
        std::fs::read(dir.path().join("models.jsonc")).unwrap(),
        models.as_bytes()
    );
    assert_eq!(
        std::fs::read(dir.path().join("settings.jsonc")).unwrap(),
        settings.as_bytes()
    );
    let replacement_settings = r#"{"default_model":"replacement/new"}"#;
    let replacement_models = json!({"providers": [
        slash_provider(&bad, "replacement", "response", json!([
            {"id":"new","model":"not-the-startup-model"}
        ]))
    ]})
    .to_string();
    std::fs::write(dir.path().join("settings.jsonc"), replacement_settings).unwrap();
    std::fs::write(dir.path().join("models.jsonc"), &replacement_models).unwrap();
    let calls = json!([
        call(
            "atomic-write",
            "write",
            json!({"path":"atomic.txt","content":"remembered"})
        ),
        call("atomic-read", "read", json!({"path":"atomic.txt"}))
    ]);
    calls_reply(request, calls.clone());
    let (request, body) = receive(&a);
    assert_eq!(body["messages"][2]["tool_calls"], calls);
    assert_eq!(
        result(&body["messages"][3], "atomic-write")["bytes_written"],
        10
    );
    assert_eq!(result(&body["messages"][4], "atomic-read")["ok"], true);
    let history = body["messages"].as_array().unwrap().clone();
    final_reply(request, "history established");
    std::fs::write(dir.path().join("atomic.txt"), "atomic sentinel").unwrap();

    cli.input("/model bad/broken\n/model missing/nope\nstill use a\n");
    let (request, body) = receive(&a);
    assert_eq!(authorization(&request), "Bearer a-key");
    assert_eq!(body["model"], "vendor-a");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 7);
    assert_eq!(&messages[..5], history.as_slice());
    slash_assert_message(&messages[5], "assistant", "history established");
    slash_assert_message(&messages[6], "user", "still use a");
    let before_success = messages.clone();
    final_reply(request, "old model intact");

    cli.input("/model\n/model gateway/team/secondary\nuse startup b\n");
    let (request, body) = receive(&b);
    assert_eq!(authorization(&request), "Bearer gateway-key");
    assert_eq!(body["model"], "vendor-b");
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 9);
    assert_eq!(messages[0], system);
    assert_eq!(&messages[..7], before_success.as_slice());
    slash_assert_message(&messages[7], "assistant", "old model intact");
    slash_assert_message(&messages[8], "user", "use startup b");
    final_reply(request, "startup b intact");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"history established\nold model intact\nstartup b intact\n"
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("model not found; use /model to list configured models"),
        "{stderr}"
    );
    assert!(stderr.to_lowercase().contains("header"), "{stderr}");
    assert!(
        stderr
            .lines()
            .any(|line| line.contains('*') && line.contains("a/primary"))
    );
    assert!(stderr.contains("gateway/team/secondary"), "{stderr}");
    assert!(stderr.contains("vendor-b"), "{stderr}");
    assert!(!stderr.contains("replacement/new"), "{stderr}");
    assert!(!stderr.contains("not-the-startup-model"), "{stderr}");
    for secret in [
        "switch-secret-marker",
        "invalid-header",
        "a-key",
        "gateway-key",
    ] {
        assert!(!stderr.contains(secret), "leaked {secret}: {stderr}");
    }
    assert_eq!(
        std::fs::read(dir.path().join("models.jsonc")).unwrap(),
        replacement_models.as_bytes()
    );
    assert_eq!(
        std::fs::read(dir.path().join("settings.jsonc")).unwrap(),
        replacement_settings.as_bytes()
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("atomic.txt")).unwrap(),
        "atomic sentinel"
    );
    no_more(&a);
    no_more(&b);
    no_more(&bad);
}

#[test]
fn slash_single_prompt_is_an_ordinary_model_task() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::start(&server, dir.path(), Some("/model"));
    let (request, body) = receive(&server);
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    slash_assert_message(&body["messages"][1], "user", "/model");
    final_reply(request, "ordinary slash prompt answer");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"ordinary slash prompt answer\n");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Current model:"));
    no_more(&server);
}

#[test]
fn responses_default_and_explicit_protocol_route_and_render_final_content() {
    for (protocol, output, expected) in [
        (
            None,
            json!([
                response_message(
                    "msg-one",
                    json!([response_text("first "), response_text("part")])
                ),
                response_message("msg-two", json!([response_text("; second part")]))
            ]),
            "first part; second part\n",
        ),
        (
            Some("response"),
            json!([response_message(
                "msg-refusal",
                json!([{"type": "refusal", "refusal": "Cannot perform that action."}])
            )]),
            "Cannot perform that action.\n",
        ),
        (
            Some("response"),
            json!([response_message(
                "msg-mixed",
                json!([
                    {"type": "refusal", "refusal": "not the answer"},
                    response_text("available answer")
                ])
            )]),
            "available answer\n",
        ),
    ] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("answer"),
            fixture_settings(),
            &response_models(&server, protocol),
        );
        let (request, body) = receive_at(&server, "/v1/responses");
        assert_eq!(authorization(&request), "Bearer fixture-key");
        assert_eq!(body["model"], "fixture-model");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert!(body.get("previous_response_id").is_none());
        assert!(
            body["include"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item == "reasoning.encrypted_content")
        );
        response_user(&body["input"][0], "answer");
        response_reply(request, output);
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, expected.as_bytes());
        no_more(&server);
    }
}

#[test]
fn responses_tool_batches_preserve_output_history_and_reset_instructions() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        fixture_settings(),
        &response_models(&server, None),
    );
    cli.input("create and verify a file\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    let instructions = body["instructions"].clone();
    let initial = body["input"].as_array().unwrap().clone();
    assert_eq!(initial.len(), 1);
    response_user(&initial[0], "create and verify a file");
    let reasoning = json!({
        "type": "reasoning",
        "id": "rs-plan",
        "summary": [{"type": "summary_text", "text": "Check the file after writing."}],
        "content": [{"type": "reasoning_text", "text": "Plan the dependent operations."}],
        "encrypted_content": "opaque-encrypted-reasoning",
        "status": "completed"
    });
    let mut commentary =
        response_message("msg-progress", json!([response_text("Checking the file.")]));
    commentary["phase"] = json!("commentary");
    let mut batch = json!([
        reasoning,
        commentary,
        response_call(
            "w",
            "write",
            json!({"path":"demo.txt","content":"hello from responses\n"})
        ),
        response_call("r", "read", json!({"path":"demo.txt"})),
        response_call("b", "bash", json!({"command":"wc -l < demo.txt"}))
    ]);
    // Function-call status is optional in the SDK schema; explicit unfinished statuses are not.
    batch[3].as_object_mut().unwrap().remove("status");
    response_reply(request, batch.clone());
    let (request, body) = receive_at(&server, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(body["instructions"], instructions);
    assert_eq!(body["store"], false);
    assert!(body.get("previous_response_id").is_none());
    assert_eq!(&input[..initial.len()], initial.as_slice());
    assert_eq!(&input[1..6], batch.as_array().unwrap().as_slice());
    assert_eq!(response_result(&input[6], "w")["bytes_written"], 21);
    let read = response_result(&input[7], "r");
    assert_eq!(read["ok"], true);
    let bash = response_result(&input[8], "b");
    assert_eq!(bash["ok"], true);
    assert_eq!(bash["exit_code"], 0);
    assert_eq!(bash["stdout"].as_str().unwrap().trim(), "1");
    assert_eq!(
        std::fs::read(dir.path().join("demo.txt")).unwrap(),
        b"hello from responses\n"
    );
    let history = input.clone();
    response_reply(
        request,
        json!([
            response_call("unknown", "missing", json!({})),
            response_call("absent", "read", json!({"path":"repaired.txt"}))
        ]),
    );
    let (request, body) = receive_at(&server, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(&input[..history.len()], history.as_slice());
    for (index, id) in [(11, "unknown"), (12, "absent")] {
        let error = response_result(&input[index], id);
        assert_eq!(error["ok"], false);
    }
    response_reply(
        request,
        json!([
            response_call(
                "repair",
                "write",
                json!({"path":"repaired.txt","content":"fixed"})
            ),
            response_call("verify", "read", json!({"path":"repaired.txt"}))
        ]),
    );
    let (request, body) = receive_at(&server, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(response_result(&input[15], "repair")["ok"], true);
    assert_eq!(response_result(&input[16], "verify")["ok"], true);
    let history = input.clone();
    let final_message = response_message(
        "msg-verified",
        json!([response_text("verified: demo.txt; repaired: repaired.txt")]),
    );
    response_reply(request, json!([final_message.clone()]));
    cli.input("which files did you create?\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    let input = body["input"].as_array().unwrap();
    assert_eq!(&input[..history.len()], history.as_slice());
    assert_eq!(input[history.len()], final_message);
    response_user(input.last().unwrap(), "which files did you create?");
    assert_eq!(input.len(), history.len() + 2);
    response_final(request, "demo.txt and repaired.txt");
    cli.input("/new\nstart fresh\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_eq!(body["instructions"], instructions);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    response_user(&input[0], "start fresh");
    assert!(body.get("previous_response_id").is_none());
    response_final(request, "fresh answer");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        output.stdout,
        b"Checking the file.\nverified: demo.txt; repaired: repaired.txt\ndemo.txt and repaired.txt\nfresh answer\n"
    );
    no_more(&server);
}

#[test]
fn responses_invalid_batches_never_write_and_repl_recovers() {
    let valid_write = response_call(
        "write-first",
        "write",
        json!({"path":"must-not-exist","content":"bad"}),
    );
    let valid_read = response_call("read-second", "read", json!({"path":"must-not-exist"}));
    let mut cases = Vec::new();
    for (field, value) in [
        ("call_id", json!("write-first")),
        ("call_id", json!("")),
        ("call_id", json!("  ")),
        ("name", json!("")),
        ("name", json!("  ")),
        ("status", json!("in_progress")),
        ("status", json!("incomplete")),
    ] {
        let mut invalid = valid_read.clone();
        invalid[field] = value;
        cases.push(response_body(json!([valid_write.clone(), invalid])));
    }
    let mut missing = valid_read.clone();
    missing.as_object_mut().unwrap().remove("call_id");
    cases.push(response_body(json!([valid_write.clone(), missing])));
    for status in ["failed", "incomplete", "in_progress", "queued", "cancelled"] {
        let mut invalid = response_body(json!([valid_write.clone()]));
        invalid["status"] = json!(status);
        if status == "failed" {
            invalid["error"] =
                json!({"code":"server_error","message":"provider exposed fixture-key"});
        } else if status == "incomplete" {
            invalid["incomplete_details"] = json!({"reason":"max_output_tokens"});
        }
        cases.push(invalid);
    }
    let mut response_error = response_body(json!([valid_write.clone()]));
    response_error["error"] = json!({"code":"server_error","message":"fixture-key"});
    cases.push(response_error);
    for unsupported in [
        json!({
            "type":"function_call_output",
            "id":"fco-unexpected",
            "call_id":"other",
            "output":"provider must not execute our tools",
            "status":"completed"
        }),
        json!({
            "type":"file_search_call",
            "id":"fs-unsupported",
            "queries":["anything"],
            "status":"completed"
        }),
    ] {
        cases.push(response_body(json!([valid_write.clone(), unsupported])));
    }
    cases.push(response_body(json!([response_message(
        "msg-empty",
        json!([])
    )])));
    let mut incomplete_message = response_message(
        "msg-incomplete",
        json!([response_text("not a completed message")]),
    );
    incomplete_message["status"] = json!("incomplete");
    cases.push(response_body(json!([valid_write, incomplete_message])));
    cases.push(response_body(json!([])));
    cases.push(response_body(json!([{
        "type":"reasoning",
        "id":"rs-only",
        "summary":[],
        "encrypted_content":"no-final-message",
        "status":"completed"
    }])));
    for (index, invalid) in cases.into_iter().enumerate() {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = Cli::start_with(
            dir.path(),
            None,
            fixture_settings(),
            &response_models(&server, Some("response")),
        );
        cli.input("do not execute an invalid batch\n");
        let (request, _) = receive_at(&server, "/v1/responses");
        let mut stream = SseReply::new(request);
        stream.event(json!({"type":"response.completed","sequence_number":0,"response":invalid}));
        stream.finish();
        cli.input("recover\n");
        let (request, body) = receive_at(&server, "/v1/responses");
        assert!(
            !dir.path().join("must-not-exist").exists(),
            "invalid case {index} executed a partial batch"
        );
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2, "invalid case {index} polluted history");
        response_user(&input[0], "do not execute an invalid batch");
        response_user(&input[1], "recover");
        response_final(request, "recovered");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "case {index}");
        assert!(
            !String::from_utf8_lossy(&output.stderr).contains("fixture-key"),
            "case {index} leaked credentials"
        );
        assert_eq!(output.stdout, b"recovered\n", "case {index}");
        assert!(!dir.path().join("must-not-exist").exists(), "case {index}");
        no_more(&server);
    }
}

#[test]
fn dependent_batch_and_cross_turn_memory() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, dir.path(), None);
    cli.input("create and verify a file\n");
    let (request, _) = receive(&server);
    let calls = json!([
        call(
            "w",
            "write",
            json!({"path":"demo.txt","content":"hello from rust agent\n"})
        ),
        call("r", "read", json!({"path":"demo.txt"})),
        call("b", "bash", json!({"command":"wc -l < demo.txt"}))
    ]);
    calls_reply(request, calls.clone());
    let (request, body) = receive(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["tool_calls"], calls);
    assert_eq!(result(&messages[3], "w")["bytes_written"], 22);
    let read = result(&messages[4], "r");
    assert_eq!(read["ok"], true);
    let bash = result(&messages[5], "b");
    assert_eq!(bash["ok"], true);
    assert_eq!(bash["exit_code"], 0);
    assert_eq!(bash["stdout"].as_str().unwrap().trim(), "1");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("demo.txt")).unwrap(),
        "hello from rust agent\n"
    );
    final_reply(request, "verified: hello from rust agent; lines=1");
    cli.input("which file did you just create?\n");
    let (request, body) = receive(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(
        messages[6]["content"],
        "verified: hello from rust agent; lines=1"
    );
    assert_eq!(messages[7]["role"], "user");
    assert_eq!(messages[7]["content"], "which file did you just create?");
    final_reply(request, "demo.txt");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "verified: hello from rust agent; lines=1\ndemo.txt\n"
    );
    no_more(&server);
}

#[test]
fn tool_errors_allow_correction_and_pair_all_ids() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let cli = Cli::start(&server, dir.path(), Some("fix the failed operations"));
    let (request, _) = receive(&server);
    calls_reply(
        request,
        json!([
            call("unknown", "missing", json!({})),
            call("read", "read", json!({"path":"absent.txt"}))
        ]),
    );
    let (request, body) = receive(&server);
    for (index, id) in [(3, "unknown"), (4, "read")] {
        let error = result(&body["messages"][index], id);
        assert_eq!(error["ok"], false);
        assert!(
            error["error"]
                .as_str()
                .unwrap()
                .contains(if id == "unknown" {
                    "unknown tool"
                } else {
                    "absent.txt"
                })
        );
    }
    calls_reply(
        request,
        json!([call(
            "fix",
            "write",
            json!({"path":"absent.txt","content":"recovered"})
        )]),
    );
    let (request, body) = receive(&server);
    assert_eq!(result(&body["messages"][6], "fix")["ok"], true);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("absent.txt")).unwrap(),
        "recovered"
    );
    final_reply(request, "recovered");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"recovered\n");
}

#[test]
fn invalid_batch_never_executes_even_its_first_valid_write() {
    for duplicate in [false, true] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start(&server, dir.path(), Some("do not write a broken response"));
        let (request, _) = receive(&server);
        let first = call(
            "same",
            "write",
            json!({"path":"must-not-exist","content":"bad"}),
        );
        let mut second = call("same", "read", json!({"path":"must-not-exist"}));
        if !duplicate {
            second.as_object_mut().unwrap().remove("id");
        }
        calls_reply(request, json!([first, second]));
        let output = cli.finish();
        assert!(!output.status.success());
        if duplicate {
            assert!(String::from_utf8_lossy(&output.stderr).contains("duplicate"));
        }
        assert!(!dir.path().join("must-not-exist").exists());
        no_more(&server);
    }
}

#[test]
fn configured_request_limit_executes_final_batch_without_extra_request() {
    for responses in [false, true] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = if responses {
            response_models(&server, Some("response"))
        } else {
            fixture_models(&server)
        };
        let cli = Cli::start_with(
            dir.path(),
            Some("loop"),
            "{\n\"default_model\": \"fixture/primary\",\n\"max_model_requests\": 2,\n}\n",
            &models,
        );
        for index in 0..2 {
            let (request, body) = if responses {
                receive_at(&server, "/v1/responses")
            } else {
                receive(&server)
            };
            if index > 0 {
                let tool_result = if responses {
                    response_result(body["input"].as_array().unwrap().last().unwrap(), "tick")
                } else {
                    result(body["messages"].as_array().unwrap().last().unwrap(), "tick")
                };
                assert_eq!(tool_result["exit_code"], 0);
            }
            let args = json!({"command":"printf x >> count.txt"});
            if responses {
                response_reply(request, json!([response_call("tick", "bash", args)]));
            } else {
                calls_reply(request, json!([call("tick", "bash", args)]));
            }
        }
        let output = cli.finish();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("2 model request limit reached"), "{error}");
        assert_eq!(std::fs::read(dir.path().join("count.txt")).unwrap(), b"xx");
        no_more(&server);
    }
}

#[test]
fn repl_recovers_from_protocol_error_and_new_discards_history() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, dir.path(), None);
    cli.input("first\n");
    let (request, _) = receive(&server);
    reply(request, json!({"role":"assistant","content":null}));
    cli.input("second\n");
    let (request, body) = receive(&server);
    assert_eq!(body["messages"][1]["content"], "first");
    assert_eq!(body["messages"][2]["content"], "second");
    final_reply(request, "second answer");
    cli.input("/new\nthird\n");
    let (request, body) = receive(&server);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["content"], "third");
    final_reply(request, "fresh answer");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"second answer\nfresh answer\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("neither text nor tool calls"));
}

#[test]
fn provider_model_selection_routes_to_second_provider_second_model() {
    let unused = server();
    let selected = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({
        "providers": [
            {
                "id": "openai",
                "protocol": "completion",
                "api_key": "first-key",
                "base_url": format!("http://{}/v1", unused.server_addr()),
                "models": [
                    {"id": "primary", "model": "first-primary"},
                    {"id": "secondary", "model": "first-secondary"}
                ]
            },
            {
                "id": "gateway",
                "protocol": "completion",
                "api_key": "second-key",
                "base_url": format!("http://{}/v1", selected.server_addr()),
                "models": [
                    {"id": "primary", "model": "second-primary"},
                    {"id": "team/secondary", "model": "second-secondary"}
                ]
            }
        ]
    })
    .to_string();
    let mut cli = Cli::start_with(
        dir.path(),
        None,
        "{\"default_model\": \"gateway/team/secondary\"}",
        &models,
    );
    cli.input("create the proof file\n");
    let (request, body) = receive(&selected);
    assert_eq!(body["model"], "second-secondary");
    assert_eq!(authorization(&request), "Bearer second-key");
    calls_reply(
        request,
        json!([call(
            "proof",
            "write",
            json!({"path":"proof.txt","content":"from second provider\n"})
        )]),
    );
    let (request, body) = receive(&selected);
    assert_eq!(body["model"], "second-secondary");
    assert_eq!(authorization(&request), "Bearer second-key");
    assert_eq!(result(&body["messages"][3], "proof")["ok"], true);
    final_reply(request, "done");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"done\n");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("proof.txt")).unwrap(),
        "from second provider\n"
    );
    no_more(&unused);
    no_more(&selected);
}

#[test]
fn configured_request_timeout_stops_unresponsive_provider() {
    for responses in [false, true] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = if responses {
            response_models(&server, Some("response"))
        } else {
            fixture_models(&server)
        };
        let cli = Cli::start_with(
            dir.path(),
            Some("stall"),
            "{\n\"default_model\": \"fixture/primary\",\n\"request_timeout_seconds\": 1,\n}\n",
            &models,
        );
        // Receive the request and hold it without ever responding.
        let (_request, _) = if responses {
            receive_at(&server, "/v1/responses")
        } else {
            receive(&server)
        };
        let output = cli.finish();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("timed out"), "{error}");
        no_more(&server);
    }
}

#[test]
fn provider_errors_are_redacted_and_never_retried_on_either_protocol() {
    for responses in [false, true] {
        for status in [200, 400, 429, 500] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let models = if responses {
                response_models(&server, Some("response"))
            } else {
                fixture_models(&server)
            };
            let cli = Cli::start_with(
                dir.path(),
                Some("do not retry a failed request"),
                fixture_settings(),
                &models,
            );
            let (request, _) = if responses {
                receive_at(&server, "/v1/responses")
            } else {
                receive(&server)
            };
            let body = if status == 200 {
                "malformed JSON containing fixture-key".to_owned()
            } else {
                json!({
                    "error": {
                        "message": "upstream exposed fixture-key",
                        "type": "fixture-key",
                        "param": "fixture-key",
                        "code": "fixture-key"
                    }
                })
                .to_string()
            };
            request
                .respond(Response::from_string(body).with_status_code(status))
                .unwrap();
            let output = cli.finish();
            assert!(
                !output.status.success(),
                "responses={responses}, status={status}"
            );
            assert!(output.stdout.is_empty());
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(!error.is_empty());
            assert!(!error.contains("fixture-key"), "{error}");
            no_more(&server);
        }
    }
}

#[test]
fn file_configs_override_provider_environment() {
    let server = server();
    let work = tempfile::tempdir().unwrap();
    std::fs::write(
        work.path().join("settings.jsonc"),
        "{ /* app settings with a comment */ \"default_model\": \"chosen/primary\", }",
    )
    .unwrap();
    std::fs::write(work.path().join("models.jsonc"), format!(
        "{{ /* loopback provider */ \"providers\":[{{ \"id\": \"chosen\", \"protocol\": \"completion\", \"api_key\": \"file-key\", \"base_url\": \"http://{}/v1\", \"models\":[{{ \"id\": \"primary\", \"model\": \"file-model\", }}], }}], }}",
        server.server_addr()
    ))
    .unwrap();
    let cli = Cli::spawn(
        work.path(),
        &["-p", "create file"],
        &[
            ("OPENAI_API_KEY", "wrong-environment-key"),
            ("OPENAI_MODEL", "wrong-environment-model"),
            ("OPENAI_BASE_URL", "not-a-url"),
        ],
    );
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "file-model");
    assert_eq!(authorization(&request), "Bearer file-key");
    calls_reply(
        request,
        json!([call(
            "w",
            "write",
            json!({"path":"created.txt","content":"file-config-used"})
        )]),
    );
    let (request, body) = receive(&server);
    assert_eq!(result(&body["messages"][3], "w")["ok"], true);
    assert_eq!(
        std::fs::read_to_string(work.path().join("created.txt")).unwrap(),
        "file-config-used"
    );
    final_reply(request, "created");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"created\n");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("file-key"));
}

#[test]
fn missing_settings_uses_first_provider_first_model_and_defaults() {
    let unused = server();
    let server = server();
    let models = json!({
        "providers": [
            {
                "id": "first",
                "protocol": "completion",
                "api_key": "first-key",
                "base_url": format!("http://{}/v1", server.server_addr()),
                "models": [
                    {"id": "primary", "model": "first-primary"},
                    {"id": "secondary", "model": "first-secondary"}
                ]
            },
            {
                "id": "second",
                "protocol": "completion",
                "api_key": "second-key",
                "base_url": format!("http://{}/v1", unused.server_addr()),
                "models": [{"id": "primary", "model": "second-primary"}]
            }
        ]
    })
    .to_string();
    let dir = tempfile::tempdir().unwrap();
    // No settings.jsonc: defaults select the first provider's first model.
    std::fs::write(dir.path().join("models.jsonc"), models).unwrap();
    let cli = Cli::start_with_existing_files(dir.path(), Some("hello"));
    // finish() waits for exit, so the loopback reply must come first.
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "first-primary");
    assert_eq!(authorization(&request), "Bearer first-key");
    final_reply(request, "defaults-ok");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"defaults-ok\n");
    no_more(&server);
    no_more(&unused);
}

#[test]
fn unmatched_default_model_falls_back_to_first_provider_first_model() {
    let unused = server();
    let server = server();
    let models = json!({
        "providers": [
            {
                "id": "first",
                "protocol": "completion",
                "api_key": "first-key",
                "base_url": format!("http://{}/v1", server.server_addr()),
                "models": [{"id": "primary", "model": "first-primary"}]
            },
            {
                "id": "second",
                "protocol": "completion",
                "api_key": "second-key",
                "base_url": format!("http://{}/v1", unused.server_addr()),
                "models": [{"id": "primary", "model": "second-primary"}]
            }
        ]
    })
    .to_string();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("models.jsonc"), models).unwrap();
    std::fs::write(
        dir.path().join("settings.jsonc"),
        r#"{"default_model": "missing/nope"}"#,
    )
    .unwrap();
    let cli = Cli::start_with_existing_files(dir.path(), Some("hello"));
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "first-primary");
    assert_eq!(authorization(&request), "Bearer first-key");
    final_reply(request, "fallback-ok");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"fallback-ok\n");
    no_more(&server);
    no_more(&unused);
}

#[test]
fn unselected_anthropic_refuses_both_cli_modes_without_requests_or_tools() {
    let anthropic = server();
    let openai = server();
    let dir = tempfile::tempdir().unwrap();
    let models = json!({
        "providers": [
            {
                "id": "anthropic",
                "protocol": "anthropic",
                "api_key": "secret-marker-anthropic",
                "base_url": format!("http://{}/v1", anthropic.server_addr()),
                "models": [{"id": "primary", "model": "anthropic-model"}]
            },
            {
                "id": "openai",
                "protocol": "completion",
                "api_key": "secret-marker-openai",
                "base_url": format!("http://{}/v1", openai.server_addr()),
                "models": [{"id": "primary", "model": "openai-model"}]
            }
        ]
    })
    .to_string();
    for interactive in [false, true] {
        let mut cli = Cli::start_with(
            dir.path(),
            if interactive {
                None
            } else {
                Some("create must-not-exist file")
            },
            r#"{"default_model":"openai/primary"}"#,
            &models,
        );
        if interactive {
            cli.input("/new\ncreate must-not-exist file\n/quit\n");
        }
        let output = cli.finish();
        let error = String::from_utf8(output.stderr).unwrap();
        assert_eq!(output.status.success(), interactive, "{error}");
        assert!(output.stdout.is_empty());
        assert!(error.contains("models.jsonc"), "{error}");
        assert!(!error.contains("secret-marker"), "{error}");
        assert!(!dir.path().join("must-not-exist").exists());
        no_more(&anthropic);
        no_more(&openai);
    }
}

#[test]
fn selected_anthropic_refuses_both_cli_modes_without_requests_or_tools() {
    let anthropic = server();
    let openai = server();
    let models = json!({
        "providers": [
            {
                "id": "anthropic",
                "protocol": "anthropic",
                "api_key": "secret-marker-anthropic",
                "base_url": format!("http://{}/v1", anthropic.server_addr()),
                "models": [{"id": "primary", "model": "anthropic-model"}]
            },
            {
                "id": "openai",
                "protocol": "completion",
                "api_key": "secret-marker-openai",
                "base_url": format!("http://{}/v1", openai.server_addr()),
                "models": [{"id": "primary", "model": "openai-model"}]
            }
        ]
    })
    .to_string();
    // Missing settings must not skip the unsupported first provider.
    for settings in [None, Some(r#"{"default_model":"anthropic/primary"}"#)] {
        for interactive in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("models.jsonc"), &models).unwrap();
            if let Some(settings) = settings {
                std::fs::write(dir.path().join("settings.jsonc"), settings).unwrap();
            }
            let mut cli = Cli::start_with_existing_files(
                dir.path(),
                if interactive {
                    None
                } else {
                    Some("create must-not-exist file")
                },
            );
            if interactive {
                cli.input("/new\ncreate must-not-exist file\n/quit\n");
            }
            let output = cli.finish();
            let error = String::from_utf8(output.stderr).unwrap();
            assert_eq!(output.status.success(), interactive, "{error}");
            assert!(output.stdout.is_empty());
            assert!(error.contains("models.jsonc"), "{error}");
            assert!(!error.contains("secret-marker"), "{error}");
            assert!(!dir.path().join("must-not-exist").exists());
            no_more(&anthropic);
            no_more(&openai);
        }
    }
}

#[test]
fn interactive_config_errors_keep_session_available_without_model_requests() {
    let server = server();
    let valid = json!({
        "providers": [{
            "id": "fixture",
            "protocol": "completion",
            "api_key": "secret-marker",
            "base_url": format!("http://{}/v1", server.server_addr()),
            "models": [{"id": "primary", "model": "fixture-model"}]
        }]
    });
    let valid_models = valid.to_string();
    let mut cases = vec![
        (Some(fixture_settings().to_owned()), None, "models.jsonc"),
        (
            Some("{ broken secret-marker settings".to_owned()),
            Some(valid_models.clone()),
            "settings.jsonc",
        ),
        (
            Some(fixture_settings().to_owned()),
            Some(r#"{"providers":["secret-marker""#.to_owned()),
            "models.jsonc",
        ),
    ];
    for settings in [
        json!({"default_model": "fixture/primary", "unknown": "secret-marker"}),
        json!({"default_model": "fixture/primary", "max_model_requests": 0}),
        json!({"default_model": "fixture/primary", "request_timeout_seconds": 0}),
        json!({"default_model": ["secret-marker"]}),
        json!({"default_model": "secret-marker"}),
        json!({"default_model": "   "}),
        json!({"default_model": "/secret-marker"}),
        json!({"default_model": "secret-marker/ "}),
    ] {
        cases.push((
            Some(settings.to_string()),
            Some(valid_models.clone()),
            "settings.jsonc",
        ));
    }
    let mut invalid_models = vec![
        json!({"providers": []}),
        // The old flat format is deliberately rejected, not a valid fallback.
        json!({"models": [{"id": "fixture", "api_key": "secret-marker", "model": "m"}]}),
    ];
    for (field, value) in [
        ("id", json!(["secret-marker"])),
        ("id", json!(" ")),
        ("id", json!("secret-marker/provider")),
        ("api_key", json!(" ")),
        ("protocol", json!("secret-marker")),
        ("protocol", json!("openai")),
        ("protocol", json!("Completion")),
        ("protocol", json!(" response ")),
        ("protocol", Value::Null),
        ("base_url", Value::Null),
        ("base_url", json!("secret-marker")),
        ("models", json!([])),
    ] {
        let mut invalid = valid.clone();
        invalid["providers"][0][field] = value;
        invalid_models.push(invalid);
    }
    let mut duplicate_provider = valid.clone();
    duplicate_provider["providers"]
        .as_array_mut()
        .unwrap()
        .push(valid["providers"][0].clone());
    invalid_models.push(duplicate_provider);
    let mut duplicate_model = valid.clone();
    duplicate_model["providers"][0]["models"]
        .as_array_mut()
        .unwrap()
        .push(valid["providers"][0]["models"][0].clone());
    invalid_models.push(duplicate_model);
    for (field, value) in [
        ("id", json!(" ")),
        ("model", json!(" ")),
        ("api_key", json!("secret-marker")),
        ("base_url", json!("http://secret-marker.invalid")),
        ("protocol", json!("completion")),
    ] {
        let mut invalid = valid.clone();
        invalid["providers"][0]["models"][0][field] = value;
        invalid_models.push(invalid);
    }
    for invalid_provider in [
        json!({
            "id": "unused",
            "api_key": "secret-marker",
            "base_url": "secret-marker",
            "models": [{"id": "primary", "model": "unused-model"}]
        }),
        json!({
            "id": "unused",
            "api_key": "secret-marker",
            "models": []
        }),
        json!({
            "id": "unused",
            "protocol": "anthropic",
            "api_key": "secret-marker",
            "models": [{"id": "primary", "model": " "}]
        }),
    ] {
        let mut invalid = valid.clone();
        invalid["providers"]
            .as_array_mut()
            .unwrap()
            .push(invalid_provider);
        invalid_models.push(invalid);
    }
    let mut invalid_unselected_model = valid.clone();
    invalid_unselected_model["providers"][0]["models"]
        .as_array_mut()
        .unwrap()
        .push(json!({"id": "unused", "model": " "}));
    invalid_models.push(invalid_unselected_model);
    for models in invalid_models {
        cases.push((
            Some(fixture_settings().to_owned()),
            Some(models.to_string()),
            "models.jsonc",
        ));
    }
    // A non-blank selected key reaches the existing client header boundary.
    let mut invalid_header = valid.clone();
    invalid_header["providers"][0]["api_key"] = json!("secret-marker\n");
    cases.push((
        Some(fixture_settings().to_owned()),
        Some(invalid_header.to_string()),
        "invalid API key header",
    ));
    for (settings, models, failing) in cases {
        let dir = tempfile::tempdir().unwrap();
        if let Some(settings) = settings {
            std::fs::write(dir.path().join("settings.jsonc"), settings).unwrap();
        }
        if let Some(models) = models {
            std::fs::write(dir.path().join("models.jsonc"), models).unwrap();
        }
        let mut cli = Cli::start_with_existing_files(dir.path(), None);
        cli.input("\n/new\ncreate must-not-exist file\n/quit\n");
        let output = cli.finish();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{failing}: {error}");
        assert!(output.stdout.is_empty());
        assert!(error.contains(failing), "{error}");
        assert!(!error.contains("secret-marker"), "{error}");
        assert!(!dir.path().join("must-not-exist").exists());
    }
    no_more(&server);
}

#[test]
fn interactive_unreadable_configs_do_not_prevent_quit() {
    for failing in ["settings.jsonc", "models.jsonc"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.jsonc"), fixture_settings()).unwrap();
        if failing == "settings.jsonc" {
            std::fs::remove_file(dir.path().join(failing)).unwrap();
            std::fs::create_dir(dir.path().join(failing)).unwrap();
        } else {
            std::fs::write(dir.path().join(failing), [0xff, 0xfe]).unwrap();
        }
        let mut cli = Cli::start_with_existing_files(dir.path(), None);
        cli.input("/quit\n");
        let output = cli.finish();
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{error}");
        assert!(output.stdout.is_empty());
        assert!(error.contains(failing), "{error}");
    }
}

#[test]
fn missing_or_malformed_config_fails_without_environment_fallback_or_secret_leak() {
    let valid_settings = fixture_settings();
    let valid_models = "{\n\"providers\":[{\"id\":\"fixture\",\"protocol\":\"completion\",\"api_key\":\"secret-marker\",\"models\":[{\"id\":\"primary\",\"model\":\"fixture-model\"}]}],\n}\n";
    let cases = [
        (Some(valid_settings), None, "models.jsonc"),
        (
            Some("{ broken secret-marker settings"),
            Some(valid_models),
            "settings.jsonc",
        ),
        (
            Some(valid_settings),
            Some(
                "{\"providers\": [{\"id\": [\"secret-marker\"], \"api_key\": \"secret-marker\", \"models\": [{\"id\":\"primary\", \"model\":\"m\"}]}]}",
            ),
            "models.jsonc",
        ),
    ];
    for (settings, models, failing) in cases {
        let dir = tempfile::tempdir().unwrap();
        // A valid legacy single-file config must not rescue startup.
        std::fs::write(
            dir.path().join("pp.jsonc"),
            "{ \"api_key\": \"legacy-key\", \"model\": \"legacy-model\" }",
        )
        .unwrap();
        if let Some(settings) = settings {
            std::fs::write(dir.path().join("settings.jsonc"), settings).unwrap();
        }
        if let Some(models) = models {
            std::fs::write(dir.path().join("models.jsonc"), models).unwrap();
        }
        let cli = Cli::spawn(
            dir.path(),
            &["-p", "hello"],
            &[
                ("OPENAI_API_KEY", "secret-marker"),
                ("OPENAI_MODEL", "environment-model"),
            ],
        );
        let output = cli.finish();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(failing), "{error}");
        assert!(!error.contains("secret-marker"), "{error}");
        assert!(!error.contains("legacy-key"), "{error}");
    }
}

#[cfg(unix)]
mod tui {
    use super::*;
    use portable_pty::{CommandBuilder, MasterPty, PtySize, SlavePty, native_pty_system};
    use std::{
        io::Read,
        path::Path,
        sync::mpsc::{self, Receiver},
    };

    const DEADLINE: Duration = Duration::from_secs(5);

    fn input_is_ready(screen: &vt100::Screen) -> bool {
        screen
            .contents()
            .lines()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|lines| lines[1].starts_with('❯') && lines[0].chars().all(|ch| ch == '─'))
    }

    struct Pty {
        child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
        master: Box<dyn MasterPty + Send>,
        // Keep the slave open until termios has been checked after the child exits.
        slave: Option<Box<dyn SlavePty + Send>>,
        writer: Option<Box<dyn Write + Send>>,
        output: Receiver<Vec<u8>>,
        reader: Option<thread::JoinHandle<()>>,
        parser: vt100::Parser,
        raw: Vec<u8>,
        original_flags: [u128; 4],
    }

    fn terminal_flags(master: &dyn MasterPty) -> [u128; 4] {
        let termios = master.get_termios().expect("PTY must expose termios");
        [
            u128::from(termios.input_flags.bits()),
            u128::from(termios.output_flags.bits()),
            u128::from(termios.control_flags.bits()),
            u128::from(termios.local_flags.bits()),
        ]
    }

    impl Pty {
        fn start(server: &Server, cwd: &Path) -> Self {
            std::fs::write(cwd.join("settings.jsonc"), fixture_settings()).unwrap();
            std::fs::write(cwd.join("models.jsonc"), fixture_models(server)).unwrap();
            Self::start_with_existing_files(cwd)
        }

        fn start_with_existing_files(cwd: &Path) -> Self {
            let pair = native_pty_system()
                .openpty(PtySize {
                    rows: 30,
                    cols: 100,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            let original_flags = terminal_flags(pair.master.as_ref());
            let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_pp"));
            command.cwd(cwd);
            command.env("TERM", "xterm-256color");
            // Exercise ANSI colors even when the test runner opts out of colors.
            command.env_remove("NO_COLOR");
            let writer = pair.master.take_writer().unwrap();
            let mut reader = pair.master.try_clone_reader().unwrap();
            let child = pair.slave.spawn_command(command).unwrap();
            let (sender, output) = mpsc::channel();
            let reader = thread::spawn(move || {
                let mut buffer = [0; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            if sender.send(buffer[..count].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            });
            let mut pty = Self {
                child: Some(child),
                master: pair.master,
                slave: Some(pair.slave),
                writer: Some(writer),
                output,
                reader: Some(reader),
                parser: vt100::Parser::new(30, 100, 0),
                raw: Vec::new(),
                original_flags,
            };
            pty.wait_for(
                "alternate screen, bracketed paste, and SGR mouse capture",
                |screen| {
                    screen.alternate_screen()
                        && screen.bracketed_paste()
                        && screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None
                        && screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr
                },
            );
            assert_ne!(
                terminal_flags(pty.master.as_ref()),
                pty.original_flags,
                "TUI did not enter raw mode\n{}",
                pty.diagnostics()
            );
            pty
        }

        fn input(&mut self, text: &str) {
            let writer = self.writer.as_mut().unwrap();
            writer.write_all(text.as_bytes()).unwrap();
            writer.flush().unwrap();
        }

        fn resize(&mut self, rows: u16, cols: u16) {
            self.drain();
            let previous_output = self.raw.len();
            self.master
                .resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap();
            self.parser.screen_mut().set_size(rows, cols);
            let deadline = Instant::now() + DEADLINE;
            loop {
                self.drain();
                if self.raw.len() > previous_output {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "TUI did not redraw after resizing to {cols}x{rows}\n{}",
                    self.diagnostics()
                );
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn draft(&mut self, expected: &str) {
            self.wait_for("preserved input draft", |screen| {
                screen
                    .contents()
                    .lines()
                    .skip_while(|line| !line.starts_with('❯'))
                    .take_while(|line| !line.starts_with('─'))
                    .any(|line| line.trim_start_matches('❯').trim() == expected)
            });
        }

        fn ready_model(&mut self, expected: &str) {
            self.wait_for("空闲输入框和所选模型", |screen| {
                let text = screen.contents();
                input_is_ready(screen)
                    && text
                        .lines()
                        .last()
                        .is_some_and(|line| line.contains(expected))
            });
        }

        fn ready(&mut self) {
            self.wait_for("空闲输入框", input_is_ready);
        }

        fn drain(&mut self) {
            while let Ok(bytes) = self.output.try_recv() {
                self.parser.process(&bytes);
                self.raw.extend_from_slice(&bytes);
            }
        }

        fn diagnostics(&self) -> String {
            format!(
                "visible screen:\n{}\nraw terminal bytes (escaped): {:?}",
                self.parser.screen().contents(),
                String::from_utf8_lossy(&self.raw)
            )
        }

        fn wait_for(&mut self, description: &str, condition: impl Fn(&vt100::Screen) -> bool) {
            let deadline = Instant::now() + DEADLINE;
            loop {
                self.drain();
                if condition(self.parser.screen()) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {description}\n{}",
                    self.diagnostics()
                );
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn text(&mut self, text: &str) {
            self.wait_for(text, |screen| screen.contents().contains(text));
        }

        fn request(&mut self, server: &Server) -> (Request, Value) {
            // Reuse the HTTP fixture, adding the PTY's diagnostics on its deadline.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| receive(server))) {
                Ok(request) => request,
                Err(_) => {
                    self.drain();
                    panic!("model request failed\n{}", self.diagnostics());
                }
            }
        }

        fn finish(mut self) {
            let deadline = Instant::now() + DEADLINE;
            let status = loop {
                self.drain();
                if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                    break status;
                }
                assert!(
                    Instant::now() < deadline,
                    "TUI did not exit within five seconds\n{}",
                    self.diagnostics()
                );
                thread::sleep(Duration::from_millis(10));
            };
            self.child.as_mut().unwrap().wait().unwrap();
            self.child.take();
            assert_eq!(status.exit_code(), 0, "{status}\n{}", self.diagnostics());
            assert!(status.success(), "{status}\n{}", self.diagnostics());
            self.wait_for("terminal mode restoration", |screen| {
                !screen.alternate_screen()
                    && !screen.bracketed_paste()
                    && !screen.hide_cursor()
                    && screen.mouse_protocol_mode() == vt100::MouseProtocolMode::None
                    && screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Default
            });
            assert_eq!(
                terminal_flags(self.master.as_ref()),
                self.original_flags,
                "terminal flags were not restored\n{}",
                self.diagnostics()
            );
        }
    }

    impl Drop for Pty {
        fn drop(&mut self) {
            if let Some(child) = &mut self.child {
                let _ = child.kill();
                let _ = child.wait();
            }
            self.writer.take();
            self.slave.take();
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
        }
    }

    fn streaming_delta(stream: &mut SseReply, protocol: &str, text: &str) {
        if protocol == "completion" {
            stream.event(chat_chunk(json!({"role":"assistant","content":text}), None));
        } else {
            stream.event(json!({
                "type":"response.output_text.delta", "sequence_number":0,
                "item_id":"msg-live", "output_index":0, "content_index":0,
                "delta":text, "logprobs":[]
            }));
        }
    }

    fn streaming_complete(stream: &mut SseReply, protocol: &str, text: &str) {
        if protocol == "completion" {
            stream.event(chat_chunk(json!({}), Some("stop")));
            stream.frame("[DONE]");
        } else {
            stream.event(json!({
                "type":"response.completed", "sequence_number":1,
                "response":response_body(json!([
                    response_message("msg-live", json!([response_text(text)]))
                ]))
            }));
        }
    }

    fn label_color(screen: &vt100::Screen, label: &str) -> vt100::Color {
        for row in 0..30 {
            let line: String = (0..100)
                .filter_map(|col| screen.cell(row, col))
                .map(|cell| cell.contents())
                .collect();
            if line.contains(label) {
                return screen.cell(row, 2).unwrap().fgcolor();
            }
        }
        panic!("missing label {label}: {}", screen.contents());
    }

    #[test]
    fn tui_exit_keeps_conversation_on_normal_screen() {
        for exit in ["\x03", "/quit\r"] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let mut pty = Pty::start(&server, dir.path());
            pty.ready();
            pty.input("保留提问\r");
            let (request, _) = pty.request(&server);
            final_reply(request, "retained-answer 中😀\n    indented code");
            pty.text("retained-answer");
            pty.ready();
            if exit == "\x03" {
                pty.input("unsubmitted-draft");
                pty.draft("unsubmitted-draft");
            }
            pty.input(exit);
            pty.wait_for("退出后普通屏幕保留完整对话", |screen| {
                let text = screen.contents();
                !screen.alternate_screen()
                    && text.contains("│ You\n│ 保留提问")
                    && text.contains("│ Assistant\n│ retained-answer 中😀\n│     indented code")
                    && text.contains("fixture-model")
                    && text.contains('❯')
            });
            let text = pty.parser.screen().contents();
            if exit == "\x03" {
                assert!(text.contains("unsubmitted-draft"), "{text}");
            }
            assert_eq!(
                label_color(pty.parser.screen(), "You"),
                vt100::Color::Idx(4)
            );
            assert!(text.lines().any(|line| line.starts_with('─')), "{text}");
            assert_eq!(text.matches("retained-answer").count(), 1, "{text}");
            pty.finish();
            no_more(&server);
        }
    }

    #[test]
    fn tui_exit_keeps_scrolled_history_and_footer_styles() {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut pty = Pty::start(&server, dir.path());
        pty.ready();
        pty.input("long-history\r");
        let (request, _) = pty.request(&server);
        let answer = (0..90)
            .map(|index| format!("    retained-{index:03} 中😀"))
            .collect::<Vec<_>>()
            .join("\n");
        final_reply(request, &answer);
        pty.text("retained-089");
        pty.ready();
        pty.input("draft-中\x1b[5~");
        pty.draft("draft-中");
        pty.wait_for("退出前已向上翻页", |screen| {
            !screen.contents().contains("retained-089")
        });
        pty.input("\x03");
        pty.wait_for("退出后保留输入框和状态栏", |screen| {
            !screen.alternate_screen()
                && screen.contents().contains("❯ draft-中")
                && screen.contents().contains("fixture-model")
        });
        let marker = b"\x1b[?1049l";
        let start = pty
            .raw
            .windows(marker.len())
            .rposition(|bytes| bytes == marker)
            .unwrap();
        let mut parser = vt100::Parser::new(150, 100, 0);
        parser.process(&pty.raw[start + marker.len()..]);
        let screen = parser.screen();
        let text = screen.contents();
        for line in answer.lines() {
            assert_eq!(text.matches(line).count(), 1, "{line}\n{text}");
        }
        assert!(text.contains("│ You\n│ long-history"), "{text}");
        assert!(text.contains("❯ draft-中"), "{text}");
        let border_row = text.lines().position(|line| line.starts_with('─')).unwrap() as u16;
        assert_eq!(
            screen.cell(border_row, 0).unwrap().fgcolor(),
            vt100::Color::Rgb(127, 166, 106)
        );
        assert_eq!(label_color(screen, "You"), vt100::Color::Idx(4));
        let footer_row = text
            .lines()
            .position(|line| line.contains("fixture-model"))
            .unwrap() as u16;
        assert_eq!(
            screen.cell(footer_row, 2).unwrap().fgcolor(),
            vt100::Color::Rgb(190, 133, 169)
        );
        parser.process(b"shell> ");
        assert!(parser.screen().contents().contains("\nshell> "));
        assert_eq!(parser.screen().fgcolor(), vt100::Color::Default);
        assert!(!parser.screen().hide_cursor());
        pty.finish();
        no_more(&server);
    }

    #[test]
    fn tui_ctrl_c_keeps_partial_output_without_waiting_for_response() {
        for protocol in ["completion", "response"] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("settings.jsonc"), fixture_settings()).unwrap();
            std::fs::write(
                dir.path().join("models.jsonc"),
                response_models(&server, Some(protocol)),
            )
            .unwrap();
            let mut pty = Pty::start_with_existing_files(dir.path());
            pty.ready();
            pty.input("pending-task\r");
            let path = if protocol == "completion" {
                "/v1/chat/completions"
            } else {
                "/v1/responses"
            };
            let (request, _) = receive_at(&server, path);
            let mut stream = SseReply::new(request);
            streaming_delta(&mut stream, protocol, "partial-retained 中😀");
            pty.text("partial-retained");
            pty.input("\x03");
            pty.wait_for("退出后保留未完成的流式输出", |screen| {
                !screen.alternate_screen()
                    && screen
                        .contents()
                        .contains("│ Assistant\n│ partial-retained 中😀")
            });
            pty.finish();
            stream.finish();
            no_more(&server);
        }
    }

    #[test]
    fn tui_think_level_updates_status_and_rejects_changes_while_busy() {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut pty = Pty::start(&server, dir.path());
        pty.ready();
        pty.input("/effort ");
        pty.text("Highest level the model supports");
        pty.draft("/effort");
        assert!(server.try_recv().unwrap().is_none());
        pty.input("\u{1b}[B\u{1b}[B\u{1b}[B\u{1b}[B\r");
        pty.wait_for("思考级别更新到状态栏", |screen| {
            input_is_ready(screen)
                && screen
                    .contents()
                    .lines()
                    .last()
                    .is_some_and(|line| line.contains("xhigh"))
        });
        pty.input("first\r");
        let (request, body) = pty.request(&server);
        assert_eq!(body["reasoning_effort"], "xhigh");
        pty.input("/effort low\r");
        pty.draft("/effort low");
        pty.wait_for("忙碌时保留原级别", |screen| {
            screen.contents().contains("cannot send now")
                && screen
                    .contents()
                    .lines()
                    .last()
                    .is_some_and(|line| line.contains("xhigh"))
        });
        final_reply(request, "first answer");
        pty.ready();
        pty.draft("/effort low");
        pty.input("\r");
        pty.wait_for("完成后切换到 low", |screen| {
            input_is_ready(screen)
                && screen
                    .contents()
                    .lines()
                    .last()
                    .is_some_and(|line| line.contains("low"))
        });
        pty.input("second\r");
        let (request, body) = pty.request(&server);
        assert_eq!(body["reasoning_effort"], "low");
        final_reply(request, "second answer");
        pty.ready();
        pty.input("\u{3}");
        pty.finish();
    }

    #[test]
    fn tui_statusline_refreshes_git_without_losing_draft() {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "-q", "--initial-branch=fixture"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(
            dir.path().join(".gitignore"),
            "settings.jsonc\nmodels.jsonc\n",
        )
        .unwrap();
        assert!(
            Command::new("git")
                .args(["add", ".gitignore"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let mut pty = Pty::start(&server, dir.path());
        pty.wait_for(
            "ordered model, thinking, directory and Git footer",
            |screen| {
                screen.contents().lines().last().is_some_and(|line| {
                    line.starts_with(" \u{ec19} fixture-model · \u{f0aa3} default · \u{f07c} ")
                        && line.ends_with("\u{f126} fixture *1")
                })
            },
        );
        pty.input("draft-中");
        pty.draft("draft-中");
        std::fs::write(dir.path().join("new-file"), "draft").unwrap();
        pty.wait_for("background Git refresh", |screen| {
            screen
                .contents()
                .lines()
                .last()
                .is_some_and(|line| line.ends_with("\u{f126} fixture *1 ?1"))
        });
        pty.draft("draft-中");
        pty.resize(4, 20);
        pty.resize(30, 100);
        pty.draft("draft-中");
        pty.text("\u{f126} fixture *1 ?1");
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }

    #[test]
    fn tui_mouse_wheel_scrolls_history_and_resumes_following() {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("settings.jsonc"), fixture_settings()).unwrap();
        std::fs::write(
            dir.path().join("models.jsonc"),
            response_models(&server, Some("completion")),
        )
        .unwrap();
        let mut pty = Pty::start_with_existing_files(dir.path());
        pty.ready();
        pty.input("wheel task\r");
        let (request, _) = receive_at(&server, "/v1/chat/completions");
        let mut stream = SseReply::new(request);
        let mut full = (0..60)
            .map(|i| format!("line-{i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        streaming_delta(&mut stream, "completion", &full);
        pty.text("line-59");
        pty.input("draft-中");
        pty.draft("draft-中");
        pty.input("\x1b[<64;10;5M\x1b[<64;10;5M");
        pty.wait_for("wheel-scrolled history", |screen| {
            let text = screen.contents();
            text.contains("line-53") && !text.contains("line-59")
        });
        let old_view: Vec<String> = pty
            .parser
            .screen()
            .contents()
            .lines()
            .take(25)
            .map(str::to_owned)
            .collect();
        streaming_delta(&mut stream, "completion", "\nnew-a\nnew-b");
        full.push_str("\nnew-a\nnew-b");
        streaming_complete(&mut stream, "completion", &full);
        stream.finish();
        pty.ready();
        assert_eq!(
            pty.parser
                .screen()
                .contents()
                .lines()
                .take(25)
                .map(str::to_owned)
                .collect::<Vec<_>>(),
            old_view
        );
        pty.draft("draft-中");
        pty.input("\x1b[<65;10;5M\x1b[<65;10;5M\x1b[<65;10;5M\x1b[<65;10;5M");
        pty.text("new-b");
        pty.draft("draft-中");
        pty.input("\x7f\x7f\x7f\x7f\x7f\x7f\x7f");
        pty.input("follow task\r");
        let (request, _) = receive_at(&server, "/v1/chat/completions");
        let mut stream = SseReply::new(request);
        streaming_delta(&mut stream, "completion", "follow-visible");
        pty.text("follow-visible");
        streaming_complete(&mut stream, "completion", "follow-visible");
        stream.finish();
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }

    #[test]
    fn streaming_tui_both_protocols_live_colors_scroll_resize_and_bash() {
        for protocol in ["completion", "response"] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("settings.jsonc"), fixture_settings()).unwrap();
            std::fs::write(
                dir.path().join("models.jsonc"),
                response_models(&server, Some(protocol)),
            )
            .unwrap();
            let mut pty = Pty::start_with_existing_files(dir.path());
            pty.ready();
            pty.input("live task\r");
            let path = if protocol == "completion" {
                "/v1/chat/completions"
            } else {
                "/v1/responses"
            };
            let (request, body) = receive_at(&server, path);
            assert_eq!(body["stream"], true);
            let mut stream = SseReply::new(request);
            streaming_delta(&mut stream, protocol, "first-中");
            pty.text("first-中");
            pty.drain();
            assert!(pty.parser.screen().contents().contains("running"));
            assert!(!pty.parser.screen().contents().contains("-last"));
            assert_ne!(
                label_color(pty.parser.screen(), "You"),
                label_color(pty.parser.screen(), "Assistant")
            );
            let mut full = "first-中".to_owned();
            let lines: String = (0..60).map(|i| format!("\nline-{i:02}")).collect();
            full.push_str(&lines);
            streaming_delta(&mut stream, protocol, &lines);
            pty.text("line-59");
            pty.input("draft-中");
            pty.draft("draft-中");
            pty.input("\x1b[5~");
            pty.wait_for("paged older output", |s| {
                s.contents().contains("line-30") && !s.contents().contains("line-59")
            });
            let old_view: Vec<String> = pty
                .parser
                .screen()
                .contents()
                .lines()
                .take(25)
                .map(str::to_owned)
                .collect();
            streaming_delta(&mut stream, protocol, "-last");
            full.push_str("-last");
            streaming_complete(&mut stream, protocol, &full);
            stream.finish();
            pty.ready();
            assert_eq!(
                pty.parser
                    .screen()
                    .contents()
                    .lines()
                    .take(25)
                    .map(str::to_owned)
                    .collect::<Vec<_>>(),
                old_view
            );
            pty.draft("draft-中");
            pty.input("\x1b[6~");
            pty.text("line-59-last");
            pty.resize(4, 20);
            pty.resize(30, 100);
            pty.draft("draft-中");
            pty.text("line-59-last");
            pty.input("\r");
            let (request, _) = receive_at(&server, path);
            let command = "printf 'out-first'; printf 'err-first' >&2; for ((i=0;i<1000;i++)); do [[ -e release ]] && break; sleep 0.01; done; printf 'out-last'";
            if protocol == "completion" {
                calls_reply(
                    request,
                    json!([call("b", "bash", json!({"command":command}))]),
                );
            } else {
                response_reply(
                    request,
                    json!([response_call("b", "bash", json!({"command":command}))]),
                );
            }
            pty.wait_for("both live pipe bodies", |s| {
                let text = s.contents();
                let body = |expected: &str| {
                    text.lines()
                        .any(|line| line.trim_matches([' ', '│']) == expected)
                };
                body("out-first") && body("err-first")
            });
            pty.drain();
            assert!(pty.parser.screen().contents().contains("running"));
            assert!(!pty.parser.screen().contents().contains("out-firstout-last"));
            let bash_color = label_color(pty.parser.screen(), "bash");
            let user_color = label_color(pty.parser.screen(), "You");
            assert_ne!(bash_color, user_color);
            assert_ne!(bash_color, vt100::Color::Default);
            no_more(&server);
            std::fs::write(dir.path().join("release"), "").unwrap();
            let (request, body) = receive_at(&server, path);
            let tool = if protocol == "completion" {
                result(body["messages"].as_array().unwrap().last().unwrap(), "b")
            } else {
                response_result(body["input"].as_array().unwrap().last().unwrap(), "b")
            };
            assert_eq!(tool["stdout"], "out-firstout-last");
            assert_eq!(tool["stderr"], "err-first");
            assert_eq!(tool["exit_code"], 0);
            if protocol == "completion" {
                final_reply(request, "bash-complete");
            } else {
                response_final(request, "bash-complete");
            }
            pty.text("bash-complete");
            pty.ready();
            pty.input("interrupt running\r");
            let (_request, _) = receive_at(&server, path);
            pty.input("\x03");
            pty.finish();
            no_more(&server);
        }
    }

    #[test]
    fn streaming_tui_partial_failures_preserve_text_and_recover_without_leaks() {
        for protocol in ["completion", "response"] {
            for ending in ["eof", "json", "failed"] {
                let server = server();
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("settings.jsonc"), fixture_settings()).unwrap();
                std::fs::write(
                    dir.path().join("models.jsonc"),
                    response_models(&server, Some(protocol)),
                )
                .unwrap();
                let mut pty = Pty::start_with_existing_files(dir.path());
                pty.ready();
                pty.input("fail partial\r");
                let path = if protocol == "completion" {
                    "/v1/chat/completions"
                } else {
                    "/v1/responses"
                };
                let (request, _) = receive_at(&server, path);
                let mut stream = SseReply::new(request);
                streaming_delta(&mut stream, protocol, "partial-visible");
                pty.text("partial-visible");
                match ending {
                    "json" => stream.frame("{private-provider-body fixture-key"),
                    "failed" if protocol == "completion" => stream.event(json!({
                        "error":{"message":"private-provider-body fixture-key","type":"server_error"}
                    })),
                    "failed" => stream.event(json!({
                        "type":"error","sequence_number":1,"code":"server_error",
                        "message":"private-provider-body fixture-key","param":null
                    })),
                    _ => {}
                }
                stream.finish();
                pty.wait_for("partial plus failure and ready", |s| {
                    let text = s.contents();
                    text.contains("partial-visible") && text.contains("Error") && input_is_ready(s)
                });
                assert!(
                    !pty.parser
                        .screen()
                        .contents()
                        .contains("private-provider-body")
                );
                assert!(!String::from_utf8_lossy(&pty.raw).contains("fixture-key"));
                assert!(!String::from_utf8_lossy(&pty.raw).contains("private-provider-body"));
                pty.input("recover\r");
                let (request, body) = receive_at(&server, path);
                if protocol == "completion" {
                    assert_eq!(body["messages"].as_array().unwrap().len(), 3);
                    assert_eq!(body["messages"][1]["content"], "fail partial");
                    assert_eq!(body["messages"][2]["content"], "recover");
                    final_reply(request, "recovered");
                } else {
                    assert_eq!(body["input"].as_array().unwrap().len(), 2);
                    response_user(&body["input"][0], "fail partial");
                    response_user(&body["input"][1], "recover");
                    response_final(request, "recovered");
                }
                pty.text("recovered");
                pty.ready();
                pty.input("\x03");
                pty.finish();
                no_more(&server);
            }
        }
    }

    #[test]
    fn tui_slash_model_picker_preserves_history() {
        let a = server();
        let b = server();
        let dir = tempfile::tempdir().unwrap();
        let models = json!({"providers": [
            slash_provider(&a, "a", "completion", json!([
                {"id":"primary","model":"vendor-a"}
            ])),
            slash_provider(&b, "gateway", "completion", json!([
                {"id":"team/secondary","model":"vendor-b"}
            ]))
        ]})
        .to_string();
        std::fs::write(
            dir.path().join("settings.jsonc"),
            r#"{"default_model":"a/primary"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("models.jsonc"), models).unwrap();
        let mut pty = Pty::start_with_existing_files(dir.path());
        pty.ready_model("vendor-a");
        pty.input("remember picker alpha\r");
        let (request, body) = pty.request(&a);
        assert_eq!(body["model"], "vendor-a");
        let system = body["messages"][0].clone();
        final_reply(request, "picker alpha retained");
        pty.text("picker alpha retained");
        pty.ready_model("vendor-a");

        pty.input("/");
        pty.wait_for("visible slash command menu", |screen| {
            let text = screen.contents();
            ["help", "model", "new", "effort", "quit"]
                .iter()
                .enumerate()
                .all(|(index, command)| {
                    text.lines()
                        .nth(24 + index)
                        .is_some_and(|line| line.contains(command))
                })
                && screen.hide_cursor()
        });
        pty.input("\x1b[B");
        pty.resize(4, 20);
        pty.resize(30, 100);
        pty.text("Exit pp");
        pty.input("\t");
        pty.draft("/model");
        no_more(&a);
        no_more(&b);
        pty.input(" ");
        pty.wait_for("空格展开输入框下方模型菜单", |screen| {
            let text = screen.contents();
            text.contains("a/primary")
                && text.contains("gateway/team/secondary")
                && text.contains("vendor-b")
                && text.contains("completion")
                && text.find("❯ /model").unwrap() < text.find("a/primary").unwrap()
                && text.contains("(current)")
                && screen.hide_cursor()
        });
        // 确认前检查首尾导航、翻页以及窗口缩放。
        pty.input("\x1b[H\x1b[5~\x1b[A\x1b[F\x1b[6~\x1b[B");
        pty.resize(4, 20);
        pty.resize(30, 100);
        pty.text("gateway/team/secondary");
        pty.input("\r");
        pty.text("Current model: gateway/team/secondary (vendor-b)");
        pty.ready_model("vendor-b");
        pty.text("picker alpha retained");
        no_more(&a);
        no_more(&b);

        pty.input("recall picker alpha\r");
        let (request, body) = pty.request(&b);
        assert_eq!(authorization(&request), "Bearer gateway-key");
        assert_eq!(body["model"], "vendor-b");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0], system);
        slash_assert_message(&messages[1], "user", "remember picker alpha");
        slash_assert_message(&messages[2], "assistant", "picker alpha retained");
        slash_assert_message(&messages[3], "user", "recall picker alpha");
        let history = messages.clone();
        final_reply(request, "picker beta retained");
        pty.text("picker beta retained");
        pty.ready_model("vendor-b");

        // 默认选中当前模型，直接确认不能误选第一项。
        pty.input("/model\r");
        pty.text("vendor-a");
        pty.input("\r");
        pty.wait_for("current model confirmation closes picker", |screen| {
            let text = screen.contents();
            input_is_ready(screen)
                && text.contains("gateway/team/secondary (vendor-b)")
                && !text.contains("vendor-a")
        });
        pty.draft("");
        no_more(&a);
        no_more(&b);

        pty.input("/model\r");
        pty.text("vendor-a");
        pty.input("\x1b[A\x1b");
        pty.draft("/model");
        pty.ready_model("vendor-b");
        no_more(&a);
        no_more(&b);
        pty.input("\x7f\x7f\x7f\x7f\x7f\x7f\x7f");
        pty.input("/model a/primary\r");
        pty.text("Current model: a/primary (vendor-a)");
        pty.ready_model("vendor-a");
        pty.input("direct model recall\r");
        let (request, body) = pty.request(&a);
        assert_eq!(authorization(&request), "Bearer a-key");
        assert_eq!(body["model"], "vendor-a");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(&messages[..history.len()], history.as_slice());
        slash_assert_message(&messages[4], "assistant", "picker beta retained");
        slash_assert_message(&messages[5], "user", "direct model recall");
        assert_eq!(messages.len(), 6);
        final_reply(request, "direct history retained");
        pty.text("direct history retained");
        pty.ready_model("vendor-a");

        pty.input("/bogus\r");
        pty.text("unknown command; use /help");
        pty.draft("/bogus");
        no_more(&a);
        no_more(&b);
        pty.input("\x7f\x7f\x7f\x7f\x7f\x7f");
        pty.input("\x1b[200~/not-a-command\nordinary pasted task\x1b[201~");
        pty.draft("/not-a-command");
        pty.draft("ordinary pasted task");
        no_more(&a);
        no_more(&b);
        pty.input("\r");
        let (request, body) = pty.request(&a);
        let messages = body["messages"].as_array().unwrap();
        slash_assert_message(
            messages.last().unwrap(),
            "user",
            "/not-a-command\nordinary pasted task",
        );
        assert_eq!(messages.len(), 8);
        final_reply(request, "multiline slash task accepted");
        pty.text("multiline slash task accepted");
        pty.ready_model("vendor-a");

        pty.input("/model\r");
        pty.text("gateway/team/secondary");
        // 菜单不能拦截 Ctrl-C 或妨碍终端恢复。
        pty.input("\x03");
        pty.finish();
        no_more(&a);
        no_more(&b);
    }

    #[test]
    fn tui_model_switch_busy_and_failure() {
        let a = server();
        let b = server();
        let bad = server();
        let dir = tempfile::tempdir().unwrap();
        let mut broken = slash_provider(
            &bad,
            "bad",
            "response",
            json!([{"id":"broken","model":"vendor-broken"}]),
        );
        broken["api_key"] = json!("tui-secret-marker\ninvalid-header");
        let models = json!({"providers": [
            slash_provider(&a, "a", "completion", json!([
                {"id":"primary","model":"vendor-a"}
            ])),
            slash_provider(&b, "gateway", "completion", json!([
                {"id":"team/secondary","model":"vendor-b"}
            ])),
            broken
        ]})
        .to_string();
        std::fs::write(
            dir.path().join("settings.jsonc"),
            r#"{"default_model":"a/primary"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("models.jsonc"), models).unwrap();
        let mut pty = Pty::start_with_existing_files(dir.path());
        pty.ready_model("vendor-a");
        pty.input("hold busy history\r");
        let (pending, first) = pty.request(&a);
        pty.text("running");
        pty.input("/model gateway/team/secondary\r");
        pty.text("cannot send now");
        pty.draft("/model gateway/team/secondary");
        no_more(&a);
        no_more(&b);
        no_more(&bad);
        final_reply(pending, "busy history retained");
        pty.text("busy history retained");
        pty.ready_model("vendor-a");
        pty.draft("/model gateway/team/secondary");
        no_more(&a);
        no_more(&b);

        // Only a fresh Enter after the held request finishes may switch models.
        pty.input("\r");
        pty.text("Current model: gateway/team/secondary (vendor-b)");
        pty.ready_model("vendor-b");
        no_more(&a);
        no_more(&b);
        pty.input("after busy switch\r");
        let (request, body) = pty.request(&b);
        assert_eq!(authorization(&request), "Bearer gateway-key");
        assert_eq!(body["model"], "vendor-b");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(&messages[..2], first["messages"].as_array().unwrap());
        slash_assert_message(&messages[2], "assistant", "busy history retained");
        slash_assert_message(&messages[3], "user", "after busy switch");
        assert_eq!(messages.len(), 4);
        let history = messages.clone();
        final_reply(request, "busy switch retained");
        pty.text("busy switch retained");
        pty.ready_model("vendor-b");

        pty.input("/model bad/broken\r");
        pty.wait_for("failed switch is a local error", |screen| {
            let text = screen.contents();
            text.contains("Error") && text.to_lowercase().contains("header")
        });
        pty.ready_model("vendor-b");
        no_more(&a);
        no_more(&b);
        no_more(&bad);
        pty.input("after failed switch\r");
        let (request, body) = pty.request(&b);
        assert_eq!(authorization(&request), "Bearer gateway-key");
        assert_eq!(body["model"], "vendor-b");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(&messages[..history.len()], history.as_slice());
        slash_assert_message(&messages[4], "assistant", "busy switch retained");
        slash_assert_message(&messages[5], "user", "after failed switch");
        assert_eq!(messages.len(), 6);
        final_reply(request, "failed switch history retained");
        pty.text("failed switch history retained");
        pty.ready_model("vendor-b");
        for secret in [
            "tui-secret-marker",
            "invalid-header",
            "a-key",
            "gateway-key",
        ] {
            assert!(
                !String::from_utf8_lossy(&pty.raw).contains(secret),
                "TUI leaked {secret}"
            );
        }

        pty.input("exit with model request pending\r");
        let (pending, body) = pty.request(&b);
        assert_eq!(body["model"], "vendor-b");
        slash_assert_message(
            body["messages"].as_array().unwrap().last().unwrap(),
            "user",
            "exit with model request pending",
        );
        pty.text("running");
        pty.input("/quit\r");
        pty.finish();
        drop(pending);
        no_more(&a);
        no_more(&b);
        no_more(&bad);
    }

    #[test]
    fn tui_input_history_and_control_keys_edit_the_submitted_buffer() {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut pty = Pty::start(&server, dir.path());
        pty.ready();
        pty.input("old-中\r");
        let (request, _) = pty.request(&server);
        final_reply(request, "history-first-ok");
        pty.text("history-first-ok");
        pty.ready();

        pty.input("alpha\x0a中🙂trash\x01\x01\x1b[C\x1b[C\x0b-kept\x05\x05\r");
        let (request, body) = pty.request(&server);
        assert_eq!(body["messages"][3]["content"], "alpha\n中🙂-kept");
        pty.input("draft-中\x1b[D\x1b[A");
        pty.draft("中🙂-kept");
        pty.input("\x1b[A");
        pty.draft("old-中");
        pty.input("\x1b[B");
        pty.draft("中🙂-kept");
        pty.input("\x1b[B!");
        pty.draft("draft-!中");
        no_more(&server);
        final_reply(request, "history-second-ok");
        pty.text("history-second-ok");
        pty.ready();

        pty.input("\x1b[A\x01\x0bcopied\r");
        let (request, body) = pty.request(&server);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.last().unwrap()["content"], "alpha\ncopied");
        assert_eq!(messages[3]["content"], "alpha\n中🙂-kept");
        final_reply(request, "history-edited-ok");
        pty.text("history-edited-ok");
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }

    #[test]
    fn tui_real_task_keeps_draft_and_preserves_multiline() {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut pty = Pty::start(&server, dir.path());
        pty.ready();
        pty.text("fixture-model");
        pty.input("line one\x0aline two\r");
        let (request, body) = pty.request(&server);
        assert_eq!(body["model"], "fixture-model");
        assert_eq!(body["messages"][1]["content"], "line one\nline two");

        pty.text("running");
        pty.input("next draft\r");
        pty.text("next draft");
        pty.text("cannot send now");
        no_more(&server);
        pty.input("\x1b[200~\r\npasted line\x1b[201~");
        pty.text("pasted line");
        no_more(&server);

        let calls = json!([
            call(
                "write-proof",
                "write",
                json!({"path":"proof.txt","content":"tui-ok\n"})
            ),
            call("read-proof", "read", json!({"path":"proof.txt"}))
        ]);
        calls_reply(request, calls.clone());
        let (request, body) = pty.request(&server);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[2]["tool_calls"], calls);
        let written = result(&messages[3], "write-proof");
        assert_eq!(written["ok"], true);
        assert_eq!(written["bytes_written"], 7);
        let read = result(&messages[4], "read-proof");
        assert_eq!(read["ok"], true);
        assert_eq!(
            std::fs::read(dir.path().join("proof.txt")).unwrap(),
            b"tui-ok\n"
        );
        let history = messages.clone();
        final_reply(request, "tui-ok");
        pty.wait_for("answer and preserved busy draft", |screen| {
            let text = screen.contents();
            text.contains("Assistant")
                && text.contains("tui-ok")
                && text.contains("next draft")
                && text.contains("pasted line")
                && input_is_ready(screen)
        });
        no_more(&server);

        pty.input("\r");
        let (request, body) = pty.request(&server);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(&messages[..history.len()], history.as_slice());
        assert_eq!(messages.len(), history.len() + 2);
        assert_eq!(messages[history.len()]["role"], "assistant");
        assert_eq!(messages[history.len()]["content"], "tui-ok");
        assert_eq!(messages.last().unwrap()["role"], "user");
        assert_eq!(
            messages.last().unwrap()["content"],
            "next draft\npasted line"
        );
        final_reply(request, "second-ok");
        pty.text("second-ok");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }

    #[test]
    fn tui_reset_and_request_error_recover() {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut pty = Pty::start(&server, dir.path());
        pty.ready();
        pty.input("first\r");
        let (request, _) = pty.request(&server);
        reply(request, json!({"role":"assistant","content":null}));
        pty.text("neither text nor tool calls");
        pty.text("Error");
        pty.ready();

        pty.input("second\r");
        let (request, body) = pty.request(&server);
        assert_eq!(body["messages"][1]["content"], "first");
        assert_eq!(body["messages"][2]["content"], "second");
        final_reply(request, "recovered-answer");
        pty.text("recovered-answer");
        pty.ready();

        pty.input("/new\r");
        pty.wait_for("acknowledged reset", |screen| {
            let text = screen.contents();
            text.contains("Session reset")
                && input_is_ready(screen)
                && !text.contains("recovered-answer")
                && !text.contains("neither text nor tool calls")
        });
        no_more(&server);
        pty.input("fresh\r");
        let (request, body) = pty.request(&server);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "fresh");
        final_reply(request, "fresh-answer");
        pty.text("fresh-answer");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&server);
    }

    #[test]
    fn tui_config_error_and_busy_quit_restore_terminal() {
        let server = server();
        let unavailable = tempfile::tempdir().unwrap();
        std::fs::write(
            unavailable.path().join("settings.jsonc"),
            fixture_settings(),
        )
        .unwrap();
        let mut pty = Pty::start_with_existing_files(unavailable.path());
        pty.text("unavailable");
        pty.text("models.jsonc");
        let task = "create must-not-exist";
        pty.input(task);
        pty.input("\r");
        pty.text(task);
        no_more(&server);
        assert!(!unavailable.path().join("must-not-exist").exists());
        pty.input(&"\x7f".repeat(task.chars().count()));
        pty.input("/new\r");
        pty.text("/new");
        pty.text("unavailable");
        no_more(&server);
        assert!(!unavailable.path().join("must-not-exist").exists());
        pty.input("\x7f\x7f\x7f\x7f/quit\r");
        pty.finish();
        no_more(&server);

        let busy = tempfile::tempdir().unwrap();
        let mut pty = Pty::start(&server, busy.path());
        pty.ready();
        pty.input("wait-for-reply\r");
        let (pending, body) = pty.request(&server);
        assert_eq!(body["messages"][1]["content"], "wait-for-reply");
        pty.text("running");
        // Hold the actual HTTP request open: Ctrl-C must not join its worker.
        pty.input("\x03");
        pty.finish();
        drop(pending);
        no_more(&server);
    }
}

#[test]
fn streaming_broken_stdout_still_records_executed_tool_results() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, dir.path(), Some("write with closed display"));
    drop(cli.0.as_mut().unwrap().stdout.take());
    let (request, _) = receive(&server);
    reply(
        request,
        json!({
            "role":"assistant", "content":"visible attempt",
            "tool_calls":[call("write", "write", json!({"path":"saved","content":"record me"}))]
        }),
    );
    let (request, body) = receive(&server);
    assert_eq!(result(&body["messages"][3], "write")["bytes_written"], 9);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("saved")).unwrap(),
        "record me"
    );
    final_reply(request, "finished history");
    let output = cli.finish();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("writing task output"));
    no_more(&server);
}

#[path = "cli_agent/streaming.rs"]
mod streaming;
#[path = "cli_agent/think.rs"]
mod think;
