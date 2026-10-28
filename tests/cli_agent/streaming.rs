use super::*;
use std::{
    io::Read,
    sync::mpsc::{self, Receiver},
};

#[derive(Clone, Copy, Debug)]
enum Protocol {
    Chat,
    Responses,
}

impl Protocol {
    fn start(self, server: &Server, cwd: &std::path::Path, prompt: Option<&str>) -> LiveCli {
        self.start_with_settings(server, cwd, prompt, fixture_settings())
    }

    fn start_with_settings(
        self,
        server: &Server,
        cwd: &std::path::Path,
        prompt: Option<&str>,
        settings: &str,
    ) -> LiveCli {
        let models = match self {
            Self::Chat => fixture_models(server),
            Self::Responses => response_models(server, Some("response")),
        };
        LiveCli::new(Cli::start_with(cwd, prompt, settings, &models))
    }

    fn receive(self, server: &Server) -> (Request, Value) {
        let path = match self {
            Self::Chat => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
        };
        let (request, body) = receive_at(server, path);
        assert_eq!(body["stream"], true);
        (request, body)
    }

    // The edit tool exactly as sent to the model: Chat exposes it under
    // `tools[].function`, Responses directly in `tools[]`.
    fn edit_tool(self, body: &Value) -> Value {
        let tools = body["tools"].as_array().unwrap();
        let tool = tools
            .iter()
            .find(|tool| match self {
                Self::Chat => tool["function"]["name"] == "edit",
                Self::Responses => tool["name"] == "edit",
            })
            .unwrap_or_else(|| panic!("missing edit tool: {body}"));
        match self {
            Self::Chat => tool["function"].clone(),
            Self::Responses => tool.clone(),
        }
    }

    fn text(self, stream: &mut SseReply, text: &str) {
        match self {
            Self::Chat => {
                stream.event(chat_chunk(json!({"role":"assistant","content":text}), None))
            }
            Self::Responses => stream.event(json!({
                "type":"response.output_text.delta", "sequence_number":1,
                "item_id":"msg-stream", "output_index":0, "content_index":0,
                "delta":text
            })),
        }
    }

    fn call(self, id: &str, name: &str, args: Value) -> Value {
        match self {
            Self::Chat => call(id, name, args),
            Self::Responses => response_call(id, name, args),
        }
    }

    fn calls_reply(self, request: Request, calls: Value) {
        match self {
            Self::Chat => calls_reply(request, calls),
            Self::Responses => response_reply(request, calls),
        }
    }

    fn final_reply(self, request: Request, text: &str) {
        match self {
            Self::Chat => final_reply(request, text),
            Self::Responses => response_final(request, text),
        }
    }

    fn complete(self, stream: &mut SseReply, output: Value, has_calls: bool) {
        match self {
            Self::Chat => {
                stream.event(chat_chunk(
                    json!({}),
                    Some(if has_calls { "tool_calls" } else { "stop" }),
                ));
                stream.frame("[DONE]");
            }
            Self::Responses => stream.event(json!({
                "type":"response.completed", "sequence_number":100,
                "response":response_body(output)
            })),
        }
    }

    fn history(self, body: &Value) -> &[Value] {
        match self {
            Self::Chat => body["messages"].as_array().unwrap(),
            Self::Responses => body["input"].as_array().unwrap(),
        }
    }

    fn tool_result(self, body: &Value, id: &str) -> Value {
        let item = self
            .history(body)
            .iter()
            .find(|item| match self {
                Self::Chat => item["role"] == "tool" && item["tool_call_id"] == id,
                Self::Responses => item["type"] == "function_call_output" && item["call_id"] == id,
            })
            .expect("missing tool result");
        match self {
            Self::Chat => result(item, id),
            Self::Responses => response_result(item, id),
        }
    }

    fn assert_only_users(self, body: &Value, expected: &[&str]) {
        let history = self.history(body);
        let history = match self {
            Self::Chat => {
                assert_eq!(history[0]["role"], "system");
                &history[1..]
            }
            Self::Responses => history,
        };
        assert_eq!(
            history.len(),
            expected.len(),
            "partial batch polluted history: {body}"
        );
        for (item, expected) in history.iter().zip(expected) {
            match self {
                Self::Chat => {
                    assert_eq!(item["role"], "user");
                    assert_eq!(item["content"], *expected);
                }
                Self::Responses => response_user(item, expected),
            }
        }
    }
}

const PROTOCOLS: [Protocol; 2] = [Protocol::Chat, Protocol::Responses];

fn read_value(protocol: Protocol, body: &Value, id: &str) -> Value {
    let item = protocol
        .history(body)
        .iter()
        .find(|item| {
            item["tool_call_id"] == id
                || item["call_id"] == id && item["type"] == "function_call_output"
        })
        .unwrap();
    let content = match protocol {
        Protocol::Chat => &item["content"],
        Protocol::Responses => &item["output"],
    };
    let text = content
        .as_str()
        .or_else(|| content[0]["text"].as_str())
        .unwrap();
    serde_json::from_str(text).unwrap()
}

fn read_assert_images(protocol: Protocol, body: &Value, expected: usize) {
    let history = protocol.history(body);
    let mut images = 0;
    for (index, item) in history.iter().enumerate() {
        match protocol {
            Protocol::Chat if item["role"] == "user" && item["content"].is_array() => {
                let parts = item["content"].as_array().unwrap();
                assert!(
                    parts[0]["text"]
                        .as_str()
                        .unwrap()
                        .starts_with("pp.read_image:")
                );
                assert!(history[..index].iter().any(|r| r["role"] == "tool"));
                for image in &parts[1..] {
                    assert!(
                        image["image_url"]["url"]
                            .as_str()
                            .unwrap()
                            .starts_with("data:image/png;base64,")
                    );
                    images += 1;
                }
            }
            Protocol::Responses
                if item["type"] == "function_call_output" && item["output"].is_array() =>
            {
                assert_eq!(item["output"][0]["type"], "input_text");
                for image in item["output"].as_array().unwrap().iter().skip(1) {
                    assert_eq!(image["type"], "input_image");
                    assert!(
                        image["image_url"]
                            .as_str()
                            .unwrap()
                            .starts_with("data:image/png;base64,")
                    );
                    images += 1;
                }
            }
            _ => {}
        }
    }
    assert_eq!(images, expected);
}

#[derive(Clone, Copy)]
pub(super) enum Pipe {
    Stdout,
    Stderr,
}

// Both pipes are drained from spawn onward: a test observing stdout must never
// accidentally make the child block on a full stderr pipe (or vice versa).
pub(super) struct LiveCli {
    cli: Cli,
    rx: Receiver<(Pipe, Vec<u8>)>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    readers: Vec<thread::JoinHandle<()>>,
}

impl LiveCli {
    pub(super) fn new(mut cli: Cli) -> Self {
        let child = cli.0.as_mut().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        fn reader(
            mut pipe: impl Read + Send + 'static,
            kind: Pipe,
            tx: mpsc::Sender<(Pipe, Vec<u8>)>,
        ) -> thread::JoinHandle<()> {
            thread::spawn(move || {
                let mut bytes = [0; 4096];
                loop {
                    match pipe.read(&mut bytes) {
                        Ok(0) => break,
                        Ok(count) => {
                            if tx.send((kind, bytes[..count].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => panic!("reading CLI pipe: {error}"),
                    }
                }
            })
        }
        let readers = vec![
            reader(stdout, Pipe::Stdout, tx.clone()),
            reader(stderr, Pipe::Stderr, tx),
        ];
        Self {
            cli,
            rx,
            stdout: Vec::new(),
            stderr: Vec::new(),
            readers,
        }
    }

    pub(super) fn input(&mut self, text: &str) {
        self.cli.input(text);
    }

    fn record(&mut self, (pipe, bytes): (Pipe, Vec<u8>)) {
        match pipe {
            Pipe::Stdout => self.stdout.extend(bytes),
            Pipe::Stderr => self.stderr.extend(bytes),
        }
    }

    pub(super) fn text(&self, pipe: Pipe) -> String {
        String::from_utf8_lossy(match pipe {
            Pipe::Stdout => &self.stdout,
            Pipe::Stderr => &self.stderr,
        })
        .into_owned()
    }

    pub(super) fn wait_for(&mut self, pipe: Pipe, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.text(pipe).contains(expected) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = self.rx.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!(
                    "missing live output {expected:?}: {error}; stdout={:?}; stderr={:?}",
                    self.text(Pipe::Stdout),
                    self.text(Pipe::Stderr)
                )
            });
            self.record(event);
        }
    }

    fn assert_running(&mut self) {
        assert!(
            self.cli.0.as_mut().unwrap().try_wait().unwrap().is_none(),
            "CLI exited before the gate was released"
        );
    }

    pub(super) fn finish(self) -> Output {
        let Self {
            cli,
            rx,
            mut stdout,
            mut stderr,
            readers,
        } = self;
        // Cli owns panic cleanup and a bounded wait; its pipes were taken above.
        let mut output = cli.finish();
        for (pipe, bytes) in rx {
            match pipe {
                Pipe::Stdout => stdout.extend(bytes),
                Pipe::Stderr => stderr.extend(bytes),
            }
        }
        for reader in readers {
            reader.join().unwrap();
        }
        output.stdout = stdout;
        output.stderr = stderr;
        output
    }
}

fn text_output(text: &str) -> Value {
    json!([response_message("msg-stream", json!([response_text(text)]))])
}

fn assert_clean_pipes(output: &Output) {
    assert!(!output.stdout.contains(&0x1b), "ANSI on stdout");
    assert!(!output.stderr.contains(&0x1b), "ANSI on stderr");
    for secret in [
        "private-provider-body",
        "fixture-key",
        "private-provider-code",
        "private-provider-reason",
        "private-provider-param",
        "https://private-provider.invalid",
    ] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(secret));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(secret));
    }
}

// Structural checks on the schema actually sent over the wire: the edit tool
// must expose exactly one mode's parameters. Descriptions are never pinned.
fn assert_edit_schema(tool: &Value, replace: bool) {
    assert_eq!(tool["name"], "edit");
    let parameters = &tool["parameters"];
    assert_eq!(parameters["type"], "object");
    if !replace {
        assert_eq!(parameters["required"], json!(["input"]));
        assert_eq!(parameters["additionalProperties"], false);
        assert_eq!(parameters["properties"].as_object().unwrap().len(), 1);
        assert_eq!(parameters["properties"]["input"]["type"], "string");
        return;
    }

    assert_eq!(parameters["required"], json!(["path", "edits"]));
    assert_eq!(parameters["additionalProperties"], false);
    let mut properties: Vec<&str> = parameters["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    properties.sort_unstable();
    assert_eq!(properties, ["edits", "path"], "{parameters}");
    let edits = &parameters["properties"]["edits"];
    assert_eq!(edits["type"], "array");
    assert_eq!(edits["minItems"], 1);
    let schema = parameters.to_string();
    let items = &edits["items"];
    assert_eq!(items["type"], "object", "{schema}");
    assert_eq!(items["required"], json!(["oldText", "newText"]));
    assert_eq!(items["additionalProperties"], false);
    let mut item_properties: Vec<&str> = items["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    item_properties.sort_unstable();
    assert_eq!(item_properties, ["newText", "oldText"], "{schema}");
    assert_eq!(items["properties"]["oldText"]["type"], "string");
    assert_eq!(items["properties"]["oldText"]["minLength"], 1);
    assert_eq!(items["properties"]["newText"]["type"], "string");
    assert!(
        !schema.contains("anchor"),
        "hashline field leaked: {schema}"
    );
    assert!(
        !schema.contains("\"op\""),
        "hashline field leaked: {schema}"
    );
}

fn fragmented_calls(protocol: Protocol, stream: &mut SseReply, invalid: bool) -> Value {
    let args = [
        json!({"path":"first-file","content":"first"}),
        json!({"path":"second-file","content":"second"}),
    ];
    let names = ["write", if invalid { "  " } else { "write" }];
    match protocol {
        Protocol::Chat => {
            stream.event(chat_chunk(json!({"role":"assistant"}), None));
            // Index 1 arrives first, and names/arguments are interleaved. The
            // resulting history and execution order must still be index order.
            for index in [1, 0] {
                let arguments = args[index].to_string();
                stream.event(chat_chunk(json!({"tool_calls":[{
                    "index":index,"id":format!("fragment-{index}"),"type":"function",
                    "function":{"name":&names[index][..2],"arguments":&arguments[..arguments.len()/2]}
                }]}), None));
            }
            for index in [0, 1] {
                let arguments = args[index].to_string();
                stream.event(chat_chunk(json!({"tool_calls":[{
                    "index":index,"function":{"name":&names[index][2..],"arguments":&arguments[arguments.len()/2..]}
                }]}), None));
            }
        }
        Protocol::Responses => {
            for index in [0, 1] {
                let mut item = protocol.call(
                    &format!("fragment-{index}"),
                    names[index],
                    args[index].clone(),
                );
                item["status"] = json!("in_progress");
                item["arguments"] = json!("");
                // Ark 的起始调用可能省略参数；另一项仍覆盖标准空字符串。
                if index == 0 {
                    item.as_object_mut().unwrap().remove("arguments");
                }
                stream.event(
                    json!({"type":"response.output_item.added","sequence_number":index,
                    "output_index":index,"item":item}),
                );
            }
            for index in [1, 0] {
                let arguments = args[index].to_string();
                for part in [
                    &arguments[..arguments.len() / 2],
                    &arguments[arguments.len() / 2..],
                ] {
                    stream.event(json!({"type":"response.function_call_arguments.delta","sequence_number":10+index,
                        "item_id":format!("fc-fragment-{index}"),"output_index":index,"delta":part}));
                }
                stream.event(json!({"type":"response.function_call_arguments.done","sequence_number":20+index,
                    "item_id":format!("fc-fragment-{index}"),"output_index":index,
                    "arguments":arguments,"name":names[index]}));
                stream.event(json!({"type":"response.output_item.done","sequence_number":30+index,
                    "output_index":index,"item":protocol.call(&format!("fragment-{index}"),names[index],args[index].clone())}));
            }
        }
    }
    json!([
        protocol.call("fragment-0", names[0], args[0].clone()),
        protocol.call("fragment-1", names[1], args[1].clone())
    ])
}

// A text delta after all argument fragments is the observable processing fence:
// no sleep can prove that the model parser has actually consumed a fragment.
fn calls_fence(protocol: Protocol, stream: &mut SseReply) {
    match protocol {
        Protocol::Chat => protocol.text(stream, "batch-gate"),
        Protocol::Responses => stream.event(json!({
            "type":"response.output_text.delta","sequence_number":50,
            "item_id":"msg-stream","output_index":2,"content_index":0,"delta":"batch-gate"
        })),
    }
}

#[derive(Clone, Copy, Debug)]
enum Broken {
    Eof,
    Malformed,
    Error(&'static str),
    Failed(&'static str),
    Incomplete(&'static str),
}

fn break_stream(protocol: Protocol, stream: &mut SseReply, broken: Broken) {
    match (protocol, broken) {
        (_, Broken::Eof) => {}
        (_, Broken::Malformed) => stream.frame("{private-provider-body fixture-key invalid-json"),
        (Protocol::Chat, Broken::Error(code)) => stream.event(json!({"error":{
            "message":"private-provider-body fixture-key","type":"server_error","code":code
        }})),
        (Protocol::Chat, Broken::Failed(reason) | Broken::Incomplete(reason)) => {
            stream.event(chat_chunk(json!({}), Some(reason)));
        }
        (Protocol::Responses, Broken::Error(code)) => stream.event(json!({
            "type":"error","sequence_number":90,"code":code,
            "message":"private-provider-body fixture-key https://private-provider.invalid",
            "param":"private-provider-param"
        })),
        (Protocol::Responses, Broken::Failed(code)) => {
            let mut response = response_body(json!([]));
            response["status"] = json!("failed");
            response["error"] = json!({
                "code":code,
                "message":"private-provider-body fixture-key https://private-provider.invalid",
                "param":"private-provider-param"
            });
            stream.event(json!({
                "type":"response.failed","sequence_number":90,"response":response
            }));
        }
        (Protocol::Responses, Broken::Incomplete(reason)) => {
            let mut response = response_body(json!([]));
            response["status"] = json!("incomplete");
            response["incomplete_details"] = json!({"reason":reason});
            stream.event(json!({
                "type":"response.incomplete","sequence_number":90,"response":response
            }));
        }
    }
}

fn pending_write(protocol: Protocol, stream: &mut SseReply) {
    match protocol {
        Protocol::Chat => stream.event(chat_chunk(json!({"role":"assistant","tool_calls":[{
            "index":0,"id":"pending","type":"function","function":{
                "name":"write","arguments":json!({"path":"must-not-exist","content":"not committed"}).to_string()
            }
        }]}), None)),
        Protocol::Responses => {
            let mut item = response_call("pending", "write", json!({"path":"must-not-exist","content":"not committed"}));
            item["status"] = json!("in_progress");
            stream.event(json!({"type":"response.output_item.added","sequence_number":10,"output_index":1,"item":item}));
        }
    }
}

// 文件版本头直接复制真实 read 结果，测试不自行计算 TAG。
fn hashline_header(content: &str) -> &str {
    let header = content.lines().next().unwrap();
    assert!(
        header.starts_with('[') && header.ends_with(']'),
        "{content}"
    );
    header
}

fn hashline_anchors(content: &str) -> Vec<(String, String)> {
    content
        .lines()
        .skip(1)
        .map(|row| {
            let (n, text) = row.split_once(':').unwrap();
            (n.into(), text.into())
        })
        .collect()
}

// 两种协议均验证进程内搜索和工具结果回填。

// PID marker files and the scratch directories under the task cwd are the
// deterministic process-level barriers for background tasks; the model wire
// carries the snapshots the CLI reports.
fn background_pid(path: &std::path::Path) -> rustix::process::Pid {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .and_then(rustix::process::Pid::from_raw)
        {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "missing or incomplete background PID marker {path:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

// The real CLI drives the real stats server as a background task: the agent
// starts it with background bash (surviving the foreground deadline), finds
// the printed URL through bash_task status, probes it over HTTP from a later
// foreground tool call, stops it, and keeps executing tools afterwards.

#[derive(Clone, Copy, Debug)]
enum RunBoundary {
    FinalAnswer,
    ModelError,
}

// Every way a user task can end without an explicit stop — final answer or
// model error — must stop the background tasks it started. The next user
// message in the same interactive CLI cannot address the old task id, does
// not revive the service, and can start a fresh background task with a
// different id.

#[path = "streaming/background_cases.rs"]
mod background_cases;
#[path = "streaming/edit_cases.rs"]
mod edit_cases;
#[path = "streaming/lifecycle_cases.rs"]
mod lifecycle_cases;
#[path = "streaming/read_cases.rs"]
mod read_cases;
