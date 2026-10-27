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

#[test]
fn read_files_images_web_and_resume_send_real_content_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let web = super::server();
        let dir = tempfile::tempdir().unwrap();
        let image_path = dir.path().join("fixture.png");
        image::DynamicImage::new_rgb8(2, 3)
            .save(&image_path)
            .unwrap();
        let image_bytes = std::fs::read(&image_path).unwrap();
        std::fs::write(dir.path().join("source"), "one\r\ntwo\r\nthree\r\nfour\r\n").unwrap();
        let url = format!("http://{}/page", web.server_addr());
        let image_url = format!("http://{}/image", web.server_addr());
        let cli = protocol.start_with_settings(
            &server,
            dir.path(),
            Some("read all"),
            "read_max_lines: 2\n",
        );
        let (request, body) = protocol.receive(&server);
        let tools = body["tools"].as_array().unwrap();
        let tool = tools
            .iter()
            .map(|t| {
                if matches!(protocol, Protocol::Chat) {
                    &t["function"]
                } else {
                    t
                }
            })
            .find(|t| t["name"] == "read")
            .unwrap();
        assert_eq!(
            tool["parameters"]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            vec!["path"]
        );
        assert!(
            tool["description"]
                .as_str()
                .unwrap()
                .contains("Ranges are exact")
        );
        assert!(
            body.to_string()
                .contains("NEVER separate offset/limit parameters")
        );
        protocol.calls_reply(
            request,
            json!([
                protocol.call("text", "read", json!({"path":"source:1,3"})),
                protocol.call("image", "read", json!({"path":"fixture.png"})),
                protocol.call("web", "read", json!({"path":url})),
                protocol.call("remote-image", "read", json!({"path":image_url})),
                protocol.call(
                    "after-image",
                    "write",
                    json!({"path":"kept","content":"after image"})
                )
            ]),
        );
        let request = web.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(request.url(), "/page");
        assert!(
            !request
                .headers()
                .iter()
                .any(|h| h.field.equiv("Authorization"))
        );
        request
            .respond(
                Response::from_string("first\nsecond\nthird\nfourth\n")
                    .with_header(Header::from_bytes("Content-Type", "text/plain").unwrap()),
            )
            .unwrap();
        let request = web.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(request.url(), "/image");
        request
            .respond(
                Response::from_data(image_bytes.clone())
                    .with_header(Header::from_bytes("Content-Type", "image/png").unwrap()),
            )
            .unwrap();
        let (request, body) = protocol.receive(&server);
        read_assert_images(protocol, &body, 2);
        assert_eq!(
            read_value(protocol, &body, "text")["content"],
            "one\r\nthree\r\n"
        );
        assert_eq!(read_value(protocol, &body, "image")["width"], 2);
        assert_eq!(read_value(protocol, &body, "after-image")["ok"], true);
        if matches!(protocol, Protocol::Chat) {
            let history = protocol.history(&body);
            let last_tool = history.iter().rposition(|v| v["role"] == "tool").unwrap();
            let first_attachment = history
                .iter()
                .position(|v| v["role"] == "user" && v["content"].is_array())
                .unwrap();
            assert!(last_tool < first_attachment);
        }
        let next = read_value(protocol, &body, "web")["next_path"]
            .as_str()
            .unwrap()
            .to_owned();
        protocol.calls_reply(
            request,
            json!([
                protocol.call("web-next", "read", json!({"path":next})),
                protocol.call("old-args", "read", json!({"path":"source","offset":2}))
            ]),
        );
        let (request, body) = protocol.receive(&server);
        assert_eq!(
            read_value(protocol, &body, "web-next")["content"],
            "third\nfourth\n"
        );
        assert_eq!(read_value(protocol, &body, "old-args")["ok"], false);
        no_more(&web);
        protocol.final_reply(request, "read complete");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("base64"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("Dimensions: 2 x 3"));
        let sessions = dir.path().join(".pp/agent/sessions");
        let file = std::fs::read_dir(sessions)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let id = file.file_stem().unwrap().to_str().unwrap();
        std::fs::remove_file(image_path).unwrap();
        let resumed = LiveCli::new(Cli::spawn(
            dir.path(),
            &["-r", id, "-p", "verify restored images"],
            &[],
        ));
        let (request, body) = protocol.receive(&server);
        read_assert_images(protocol, &body, 2);
        protocol.final_reply(request, "restored");
        assert!(resumed.finish().status.success());
        no_more(&web);
    }
}

#[test]
fn read_image_model_switch_and_compaction_keep_visual_exchange() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        image::DynamicImage::new_rgb8(1, 1)
            .save(dir.path().join("fixture.png"))
            .unwrap();
        std::fs::write(dir.path().join("old"), "OLD_READ_PREFIX ".repeat(3500)).unwrap();
        let (source, target) = match protocol {
            Protocol::Chat => ("completion", "response"),
            Protocol::Responses => ("response", "completion"),
        };
        let provider = |protocol: &str| json!({"api_key":"fixture", "protocol":protocol,"base_url":format!("http://{}/v1",server.server_addr()),"models":{"one":{"model":"one","context_window":65536}}});
        let models = json!({"providers":{"a":provider(source),"b":provider(target)}}).to_string();
        let mut cli = LiveCli::new(Cli::start_with(
            dir.path(),
            None,
            "default_model: a/one\nauto_compact: false\n",
            &models,
        ));
        cli.input("old task\n");
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call("old-read", "read", json!({"path":"old"}))]),
        );
        let (request, _) = protocol.receive(&server);
        protocol.final_reply(request, "old done");
        cli.wait_for(Pipe::Stdout, "old done");
        cli.input("image task\n");
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call("image", "read", json!({"path":"fixture.png"}))]),
        );
        let (request, body) = protocol.receive(&server);
        read_assert_images(protocol, &body, 1);
        protocol.final_reply(request, "image done");
        cli.wait_for(Pipe::Stdout, "image done");
        cli.input("/compact\n");
        let (request, body) = protocol.receive(&server);
        assert!(body.to_string().contains("OLD_READ_PREFIX"));
        protocol.final_reply(request, "Earlier task read a text file.");
        cli.wait_for(Pipe::Stderr, "Context compacted:");
        cli.input("/model b/one\nverify switch\n");
        let other = match protocol {
            Protocol::Chat => Protocol::Responses,
            Protocol::Responses => Protocol::Chat,
        };
        let (request, body) = other.receive(&server);
        read_assert_images(other, &body, 1);
        assert!(!body.to_string().contains("OLD_READ_PREFIX"));
        other.final_reply(request, "switched");
        cli.wait_for(Pipe::Stdout, "switched");
        cli.input("/model a/one\nverify roundtrip\n");
        let (request, body) = protocol.receive(&server);
        read_assert_images(protocol, &body, 1);
        protocol.final_reply(request, "roundtrip");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("base64"));
    }
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

#[test]
fn thinking_is_streamed_when_enabled_without_polluting_stdout() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start_with_settings(
            &server,
            dir.path(),
            Some("inspect"),
            r#"{"think_level":"high"}"#,
        );
        let (request, body) = protocol.receive(&server);
        let mut stream = SseReply::new(request);
        let reasoning = json!({"type":"reasoning","id":"r","status":"completed","summary":[{"type":"summary_text","text":"inspect 中"}],"encrypted_content":"opaque-hidden"});
        match protocol {
            Protocol::Chat => stream.event(chat_chunk(
                json!({"role":"assistant","reasoning_content":"inspect 中"}),
                None,
            )),
            Protocol::Responses => {
                assert_eq!(body["reasoning"], json!({"effort":"high","summary":"auto"}));
                stream.event(json!({"type":"response.reasoning_summary_text.delta","sequence_number":1,"item_id":"r","output_index":0,"summary_index":0,"delta":"inspect 中"}));
            }
        }
        cli.wait_for(Pipe::Stderr, "inspect 中");
        cli.assert_running();
        assert!(cli.text(Pipe::Stdout).is_empty());
        match protocol {
            Protocol::Chat => {
                protocol.text(&mut stream, "answer");
                protocol.complete(&mut stream, text_output("answer"), false);
            }
            Protocol::Responses => {
                stream.event(json!({"type":"response.reasoning_summary_text.done","sequence_number":2,"item_id":"r","output_index":0,"summary_index":0,"text":"inspect 中"}));
                stream.event(json!({"type":"response.completed","sequence_number":3,"response":response_body(json!([reasoning,response_message("m",json!([response_text("answer")]))]))}));
            }
        }
        stream.finish();
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"answer\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(stderr.matches("inspect 中").count(), 1);
        assert!(stderr.contains("inspect 中\n"));
        assert!(!stderr.contains("opaque-hidden"));
        assert!(!stderr.contains('\x1b'));
    }
}

#[test]
fn streaming_first_chunk_is_visible_before_finish_and_repl_history_is_complete() {
    for protocol in PROTOCOLS {
        for repl in [false, true] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let mut cli = protocol.start(&server, dir.path(), (!repl).then_some("first task"));
            if repl {
                cli.input("first task\n");
            }
            let (request, _) = protocol.receive(&server);
            let mut stream = SseReply::new(request);
            protocol.text(&mut stream, "first-中");
            cli.wait_for(Pipe::Stdout, "first-中");
            cli.assert_running();
            assert!(!cli.text(Pipe::Stdout).contains("-last"));
            protocol.text(&mut stream, "-last");
            protocol.complete(&mut stream, text_output("first-中-last"), false);
            stream.finish();
            if repl {
                cli.input("second task\n");
                let (request, body) = protocol.receive(&server);
                let history = protocol.history(&body);
                match protocol {
                    Protocol::Chat => {
                        assert_eq!(history.len(), 4);
                        assert_eq!(history[2]["role"], "assistant");
                        assert_eq!(history[2]["content"], "first-中-last");
                        assert_eq!(history[3]["content"], "second task");
                    }
                    Protocol::Responses => {
                        assert_eq!(history.len(), 3);
                        assert_eq!(history[1]["content"][0]["text"], "first-中-last");
                        response_user(&history[2], "second task");
                    }
                }
                protocol.final_reply(request, "second-complete");
                cli.input("/quit\n");
            }
            let output = cli.finish();
            assert!(output.status.success(), "{protocol:?}: {:?}", output);
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                if repl {
                    "first-中-last\nsecond-complete\n"
                } else {
                    "first-中-last\n"
                }
            );
            assert_clean_pipes(&output);
            no_more(&server);
        }
    }
}

#[test]
fn streaming_bash_both_pipes_are_live_and_failed_tool_does_not_stop_batch() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start(&server, dir.path(), Some("run tools"));
        let (request, _) = protocol.receive(&server);
        let command = "printf 'out-first'; printf 'err-first' >&2; for ((i=0;i<1000;i++)); do [[ -e release ]] && break; sleep 0.01; done; printf 'out-last'";
        protocol.calls_reply(
            request,
            json!([
                protocol.call("live", "bash", json!({"command":command})),
                protocol.call(
                    "bad",
                    "bash",
                    json!({"command":"printf failed-out; printf failed-err >&2; exit 7"})
                ),
                protocol.call(
                    "after",
                    "write",
                    json!({"path":"after-failure","content":"still executed"})
                )
            ]),
        );
        cli.wait_for(Pipe::Stderr, "out-first");
        cli.wait_for(Pipe::Stderr, "err-first");
        cli.assert_running();
        assert!(cli.text(Pipe::Stderr).contains("[bash stdout]"));
        assert!(cli.text(Pipe::Stderr).contains("[bash stderr]"));
        assert!(cli.text(Pipe::Stderr).contains("tool bash: running"));
        assert!(!cli.text(Pipe::Stderr).contains("out-last"));
        assert!(!dir.path().join("after-failure").exists());
        no_more(&server);
        std::fs::write(dir.path().join("release"), "go").unwrap();
        let (request, body) = protocol.receive(&server);
        assert_eq!(
            protocol.tool_result(&body, "live"),
            json!({
                "ok":true, "stdout":"out-firstout-last", "stderr":"err-first",
                "exit_code":0, "timed_out":false, "cancelled":false, "stdout_truncated":false, "stderr_truncated":false
            })
        );
        let failed = protocol.tool_result(&body, "bad");
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["exit_code"], 7);
        assert_eq!(failed["stdout"], "failed-out");
        assert_eq!(failed["stderr"], "failed-err");
        assert_eq!(protocol.tool_result(&body, "after")["ok"], true);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("after-failure")).unwrap(),
            "still executed"
        );
        protocol.final_reply(request, "tools-complete");
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"tools-complete\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("tool bash: failed"));
        assert_eq!(stderr.matches("out-first").count(), 1);
        assert_eq!(stderr.matches("err-first").count(), 1);
        assert_eq!(stderr.matches("out-last").count(), 1);
        assert_clean_pipes(&output);
        no_more(&server);
    }
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

#[test]
fn streaming_fragmented_calls_wait_for_complete_batch_before_side_effects() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start(&server, dir.path(), Some("fragmented tools"));
        let (request, _) = protocol.receive(&server);
        let mut stream = SseReply::new(request);
        let calls = fragmented_calls(protocol, &mut stream, false);
        calls_fence(protocol, &mut stream);
        cli.wait_for(Pipe::Stdout, "batch-gate");
        assert!(!dir.path().join("first-file").exists());
        assert!(!dir.path().join("second-file").exists());
        assert!(!cli.text(Pipe::Stderr).contains("tool write: running"));
        cli.assert_running();
        let mut output = calls.as_array().unwrap().clone();
        output.push(response_message(
            "msg-stream",
            json!([response_text("batch-gate")]),
        ));
        protocol.complete(&mut stream, json!(output), true);
        stream.finish();
        let (request, body) = protocol.receive(&server);
        for (index, expected) in [(0, "first"), (1, "second")] {
            assert_eq!(
                protocol.tool_result(&body, &format!("fragment-{index}"))["ok"],
                true
            );
            assert_eq!(
                std::fs::read_to_string(dir.path().join(if index == 0 {
                    "first-file"
                } else {
                    "second-file"
                }))
                .unwrap(),
                expected
            );
        }
        let history = protocol.history(&body);
        match protocol {
            Protocol::Chat => {
                assert_eq!(history[2]["tool_calls"], calls);
                assert_eq!(history[2]["content"], "batch-gate");
                assert_eq!(history[3]["tool_call_id"], "fragment-0");
                assert_eq!(history[4]["tool_call_id"], "fragment-1");
            }
            Protocol::Responses => {
                assert_eq!(history[1], calls[0]);
                assert_eq!(history[2], calls[1]);
            }
        }
        protocol.final_reply(request, "fragment-complete");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"batch-gate\nfragment-complete\n");
        assert_clean_pipes(&output);
        no_more(&server);
    }
}

#[test]
fn streaming_invalid_later_call_rejects_entire_batch_and_repl_history() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = protocol.start(&server, dir.path(), None);
        cli.input("invalid batch\n");
        let (request, _) = protocol.receive(&server);
        let mut stream = SseReply::new(request);
        let mut calls = fragmented_calls(protocol, &mut stream, true)
            .as_array()
            .unwrap()
            .clone();
        calls_fence(protocol, &mut stream);
        cli.wait_for(Pipe::Stdout, "batch-gate");
        assert!(!dir.path().join("first-file").exists());
        calls.push(response_message(
            "msg-stream",
            json!([response_text("batch-gate")]),
        ));
        protocol.complete(&mut stream, json!(calls), true);
        stream.finish();
        cli.input("recover\n");
        let (request, body) = protocol.receive(&server);
        protocol.assert_only_users(&body, &["invalid batch", "recover"]);
        assert!(!dir.path().join("first-file").exists());
        assert!(!dir.path().join("second-file").exists());
        protocol.final_reply(request, "recovered");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"batch-gate\nrecovered\n");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("tool write: running"));
        assert_clean_pipes(&output);
        no_more(&server);
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

#[test]
fn streaming_failures_keep_partial_redact_secrets_and_never_commit_or_retry() {
    for protocol in PROTOCOLS {
        let failures: &[Broken] = match protocol {
            Protocol::Chat => &[
                Broken::Eof,
                Broken::Malformed,
                Broken::Error("private-provider-code"),
                Broken::Failed("content_filter"),
                Broken::Incomplete("length"),
            ],
            Protocol::Responses => &[
                Broken::Eof,
                Broken::Malformed,
                Broken::Error("invalid_request"),
                Broken::Error("invalid_request_error"),
                Broken::Error("server_error"),
                Broken::Error("rate_limit_exceeded"),
                Broken::Error("invalid_prompt"),
                Broken::Error("private-provider-code"),
                Broken::Failed("invalid_request"),
                Broken::Failed("invalid_request_error"),
                Broken::Failed("server_error"),
                Broken::Failed("rate_limit_exceeded"),
                Broken::Failed("invalid_prompt"),
                Broken::Failed("private-provider-code"),
                Broken::Incomplete("max_output_tokens"),
                Broken::Incomplete("content_filter"),
                Broken::Incomplete("private-provider-reason"),
            ],
        };
        for &broken in failures {
            for repl in [false, true] {
                let server = server();
                let dir = tempfile::tempdir().unwrap();
                let mut cli = protocol.start(&server, dir.path(), (!repl).then_some("broken turn"));
                if repl {
                    cli.input("broken turn\n");
                }
                let (request, _) = protocol.receive(&server);
                let mut stream = SseReply::new(request);
                protocol.text(&mut stream, "partial-visible");
                cli.wait_for(Pipe::Stdout, "partial-visible");
                cli.assert_running();
                pending_write(protocol, &mut stream);
                break_stream(protocol, &mut stream, broken);
                stream.finish();
                if repl {
                    cli.input("recover\n");
                    let (request, body) = protocol.receive(&server);
                    protocol.assert_only_users(&body, &["broken turn", "recover"]);
                    protocol.final_reply(request, "recovered");
                    cli.input("/quit\n");
                }
                let output = cli.finish();
                assert_eq!(
                    output.status.success(),
                    repl,
                    "{protocol:?} {broken:?}: {output:?}"
                );
                assert_eq!(
                    output.stdout,
                    if repl {
                        b"partial-visible\nrecovered\n".as_slice()
                    } else {
                        b"partial-visible\n".as_slice()
                    }
                );
                assert!(!output.stderr.is_empty(), "missing failure description");
                if matches!(protocol, Protocol::Responses) {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let diagnostic = match broken {
                        Broken::Error(code) => Some(("error", code)),
                        Broken::Failed(code) => Some(("response.failed", code)),
                        Broken::Incomplete(reason) => Some(("response.incomplete", reason)),
                        Broken::Eof | Broken::Malformed => None,
                    };
                    if let Some((event, detail)) = diagnostic {
                        assert!(stderr.contains(event), "{broken:?}: {stderr}");
                        match detail {
                            "private-provider-code" | "private-provider-reason" => {}
                            _ => assert!(stderr.contains(detail), "{broken:?}: {stderr}"),
                        }
                        if matches!(detail, "invalid_request" | "invalid_request_error") {
                            assert!(stderr.contains("/effort low"), "{broken:?}: {stderr}");
                        }
                    }
                }
                assert!(!dir.path().join("must-not-exist").exists());
                assert!(!String::from_utf8_lossy(&output.stderr).contains("tool write: running"));
                assert_clean_pipes(&output);
                no_more(&server);
            }
        }
    }
}

#[test]
fn default_reasoning_rejection_allows_manual_effort_recovery_without_committing_failure() {
    let protocol = Protocol::Responses;
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let settings = r#"{"think_level":"default"}"#;
    let mut cli = protocol.start_with_settings(&server, dir.path(), None, settings);
    cli.input("original task\n");
    let (request, body) = protocol.receive(&server);
    super::think::assert_level(&body, "response", "default");
    protocol.assert_only_users(&body, &["original task"]);
    let mut stream = SseReply::new(request);
    protocol.text(&mut stream, "failed-visible");
    cli.wait_for(Pipe::Stdout, "failed-visible");
    pending_write(protocol, &mut stream);
    break_stream(protocol, &mut stream, Broken::Failed("invalid_request"));
    stream.finish();

    // The failed turn stays in this REPL, but neither an automatic retry nor
    // the queued write may run while the user decides how to recover.
    cli.wait_for(Pipe::Stderr, "invalid_request");
    cli.wait_for(Pipe::Stderr, "/effort low");
    cli.assert_running();
    assert!(!dir.path().join("must-not-exist").exists());
    assert!(!cli.text(Pipe::Stderr).contains("tool write: running"));
    no_more(&server);

    cli.input("/effort low\nrecover task\n");
    let (request, body) = protocol.receive(&server);
    super::think::assert_level(&body, "response", "low");
    protocol.assert_only_users(&body, &["original task", "recover task"]);
    assert!(!dir.path().join("must-not-exist").exists());
    protocol.final_reply(request, "recovered");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"failed-visible\nrecovered\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("response.failed"));
    assert!(stderr.contains("invalid_request"));
    assert!(stderr.contains("reasoning.effort"));
    assert!(stderr.contains("/effort low"));
    assert!(!stderr.contains("tool write: running"));
    assert!(!stderr.contains(&server.server_addr().to_string()));
    assert!(!dir.path().join("must-not-exist").exists());
    assert_eq!(
        std::fs::read_to_string(config_path(dir.path(), "settings.yml")).unwrap(),
        settings
    );
    assert_clean_pipes(&output);
    no_more(&server);
}

#[test]
fn streaming_responses_annotations_default_only_when_missing() {
    for annotations in [
        None,
        Some(json!([])),
        Some(json!([{
            "type":"url_citation", "start_index":0, "end_index":6,
            "url":"https://example.com/source", "title":"source"
        }])),
    ] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = Protocol::Responses.start(&server, dir.path(), None);
        cli.input("first task\n");
        let (request, _) = Protocol::Responses.receive(&server);
        let mut part = json!({"type":"output_text", "text":"answer"});
        if let Some(annotations) = &annotations {
            part["annotations"] = annotations.clone();
        }
        let item = response_message("msg-stream", json!([part]));
        let mut stream = SseReply::new(request);
        stream.event(json!({
            "type":"response.output_item.added", "sequence_number":0,
            "output_index":0, "item":item
        }));
        stream.event(json!({
            "type":"response.content_part.added", "sequence_number":1,
            "item_id":"msg-stream", "output_index":0, "content_index":0,
            "part":part
        }));
        Protocol::Responses.text(&mut stream, "answer");
        stream.event(json!({
            "type":"response.content_part.done", "sequence_number":2,
            "item_id":"msg-stream", "output_index":0, "content_index":0,
            "part":part
        }));
        stream.event(json!({
            "type":"response.output_item.done", "sequence_number":3,
            "output_index":0, "item":item
        }));
        Protocol::Responses.complete(&mut stream, json!([item]), false);
        cli.wait_for(Pipe::Stdout, "answer\n");
        cli.input("second task\n");
        let (request, body) = Protocol::Responses.receive(&server);
        assert_eq!(
            body["input"][1]["content"][0]["annotations"],
            annotations.unwrap_or_else(|| json!([]))
        );
        stream.finish();
        Protocol::Responses.final_reply(request, "done");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"answer\ndone\n");
        assert_clean_pipes(&output);
        no_more(&server);
    }
}

#[test]
fn streaming_responses_function_arguments_invalid_or_missing_final_still_fail() {
    for stage in ["added", "done", "completed"] {
        for arguments in [None, Some(Value::Null), Some(json!({})), Some(json!(7))] {
            if stage == "added" && arguments.is_none() {
                continue;
            }
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let cli = Protocol::Responses.start(&server, dir.path(), Some("task"));
            let (request, _) = Protocol::Responses.receive(&server);
            let mut item = response_call(
                "private-call",
                "write",
                json!({"path":"must-not-exist", "content":"private-content"}),
            );
            match arguments {
                Some(arguments) => item["arguments"] = arguments,
                None => {
                    item.as_object_mut().unwrap().remove("arguments");
                }
            }
            let mut stream = SseReply::new(request);
            if stage == "completed" {
                // 同时触发正文注释兼容路径，仍不得补齐最终工具参数。
                Protocol::Responses.complete(
                    &mut stream,
                    json!([
                        item,
                        response_message(
                            "msg-stream",
                            json!([{"type":"output_text", "text":"private-text"}])
                        )
                    ]),
                    true,
                );
            } else {
                stream.event(json!({
                    "type":format!("response.output_item.{stage}"), "sequence_number":0,
                    "output_index":0, "item":item
                }));
            }
            stream.finish();
            let output = cli.finish();
            assert!(!output.status.success(), "{stage}: {output:?}");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains("decoding Responses response: invalid JSON or response structure")
            );
            assert!(!stderr.contains("private-"));
            assert!(!stderr.contains("tool write: running"));
            assert!(output.stdout.is_empty());
            assert!(!dir.path().join("must-not-exist").exists());
            no_more(&server);
        }
    }
}

#[test]
fn streaming_responses_annotations_invalid_values_still_fail() {
    for part in [
        json!({"type":"output_text", "text":"private-text", "annotations":null}),
        json!({"type":"output_text", "text":"private-text", "annotations":{}}),
        json!({"type":"output_text", "text":"private-text", "annotations":"private-value"}),
        json!({"type":"output_text", "text":"private-text", "annotations":[{}]}),
        json!({"type":"output_text"}),
    ] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Protocol::Responses.start(&server, dir.path(), Some("task"));
        let (request, _) = Protocol::Responses.receive(&server);
        let mut stream = SseReply::new(request);
        Protocol::Responses.complete(
            &mut stream,
            json!([response_message("msg-stream", json!([part]))]),
            false,
        );
        stream.finish();
        let output = cli.finish();
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("decoding Responses response: invalid JSON or response structure"));
        assert!(!stderr.contains("private-"));
        no_more(&server);
    }
}

#[test]
fn streaming_completed_only_text_and_missing_tail_are_printed_once() {
    for prefix in [None, Some("prefix-中")] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let mut cli = Protocol::Responses.start(&server, dir.path(), None);
        cli.input("first task\n");
        let (request, _) = Protocol::Responses.receive(&server);
        let mut stream = SseReply::new(request);
        if let Some(prefix) = prefix {
            Protocol::Responses.text(&mut stream, prefix);
            cli.wait_for(Pipe::Stdout, prefix);
        }
        Protocol::Responses.complete(&mut stream, text_output("prefix-中-tail"), false);
        // Intentionally leave this HTTP response open. completed, not EOF or
        // [DONE], must release the turn and allow the next REPL request.
        cli.wait_for(Pipe::Stdout, "prefix-中-tail\n");
        cli.input("second task\n");
        let (request, body) = Protocol::Responses.receive(&server);
        assert_eq!(body["input"][1]["content"][0]["text"], "prefix-中-tail");
        stream.finish();
        Protocol::Responses.final_reply(request, "second-complete");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(
            output.stdout,
            b"prefix-\xe4\xb8\xad-tail\nsecond-complete\n"
        );
        assert_clean_pipes(&output);
        no_more(&server);
    }
}

#[test]
fn streaming_refusal_is_printed_once_and_content_has_priority() {
    for protocol in PROTOCOLS {
        for content in [None, Some("preferred-content")] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let cli = protocol.start(&server, dir.path(), Some("refusal selection"));
            let (request, _) = protocol.receive(&server);
            let mut stream = SseReply::new(request);
            match protocol {
                Protocol::Chat => {
                    stream.event(chat_chunk(
                        json!({"role":"assistant","refusal":"provisional-refusal"}),
                        None,
                    ));
                    if let Some(content) = content {
                        protocol.text(&mut stream, content);
                    }
                }
                Protocol::Responses => {
                    stream.event(json!({"type":"response.refusal.delta","sequence_number":1,
                        "item_id":"msg-stream","output_index":0,"content_index":0,"delta":"provisional-refusal"}));
                    stream.event(
                        json!({"type":"response.content_part.done","sequence_number":2,
                        "item_id":"msg-stream","output_index":0,"content_index":0,
                        "part":{"type":"refusal","refusal":"provisional-refusal"}}),
                    );
                    if let Some(content) = content {
                        stream.event(json!({"type":"response.output_text.delta","sequence_number":3,
                            "item_id":"msg-stream","output_index":0,"content_index":1,"delta":content}));
                    }
                }
            }
            let mut parts = vec![json!({"type":"refusal","refusal":"provisional-refusal"})];
            if let Some(content) = content {
                parts.push(response_text(content));
            }
            protocol.complete(
                &mut stream,
                json!([response_message("msg-stream", json!(parts))]),
                false,
            );
            stream.finish();
            let output = cli.finish();
            assert!(output.status.success(), "{protocol:?}: {output:?}");
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                format!("{}\n", content.unwrap_or("provisional-refusal"))
            );
            assert_clean_pipes(&output);
            no_more(&server);
        }
    }
}

#[test]
fn streaming_completed_only_refusal_is_printed_once() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let cli = Protocol::Responses.start(&server, dir.path(), Some("refuse"));
    let (request, _) = Protocol::Responses.receive(&server);
    let mut stream = SseReply::new(request);
    Protocol::Responses.complete(
        &mut stream,
        json!([response_message(
            "msg-stream",
            json!([
                {"type":"refusal","refusal":"completed-refusal"}
            ])
        )]),
        false,
    );
    stream.finish();
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"completed-refusal\n");
    assert_clean_pipes(&output);
    no_more(&server);
}

#[test]
fn streaming_completed_prefix_conflict_fails_without_tools_or_history() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Protocol::Responses.start(&server, dir.path(), None);
    cli.input("conflicting completion\n");
    let (request, _) = Protocol::Responses.receive(&server);
    let mut stream = SseReply::new(request);
    Protocol::Responses.text(&mut stream, "visible-prefix");
    cli.wait_for(Pipe::Stdout, "visible-prefix");
    Protocol::Responses.complete(
        &mut stream,
        json!([
            response_message(
                "msg-stream",
                json!([response_text("different private-provider-body fixture-key")])
            ),
            response_call(
                "never",
                "write",
                json!({"path":"must-not-exist","content":"wrong"})
            )
        ]),
        true,
    );
    stream.finish();
    cli.input("recover\n");
    let (request, body) = Protocol::Responses.receive(&server);
    Protocol::Responses.assert_only_users(&body, &["conflicting completion", "recover"]);
    assert!(!dir.path().join("must-not-exist").exists());
    Protocol::Responses.final_reply(request, "recovered");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"visible-prefix\nrecovered\n");
    assert!(!String::from_utf8_lossy(&output.stderr).contains("tool write: running"));
    assert_clean_pipes(&output);
    no_more(&server);
}

#[test]
fn streaming_empty_bash_has_no_invented_output_or_assistant_turn() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = protocol.start(&server, dir.path(), Some("empty tool"));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call("empty", "bash", json!({"command":"true"}))]),
        );
        let (request, body) = protocol.receive(&server);
        let tool = protocol.tool_result(&body, "empty");
        assert_eq!(tool["stdout"], "");
        assert_eq!(tool["stderr"], "");
        assert_eq!(tool["exit_code"], 0);
        protocol.final_reply(request, "empty-complete");
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"empty-complete\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("[bash stdout]"));
        assert!(!stderr.contains("[bash stderr]"));
        assert!(stderr.contains("tool bash: ok"));
        assert_clean_pipes(&output);
        no_more(&server);
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

#[test]
fn hashline_edit_roundtrip_and_stale_snapshot_recovery() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let demo = dir.path().join("demo.txt");
        std::fs::write(&demo, "one\ntwo\nthree\n").unwrap();
        let cli = protocol.start_with_settings(
            &server,
            dir.path(),
            Some("edit demo.txt"),
            r#"{"edit":"hashline"}"#,
        );
        let (request, body) = protocol.receive(&server);
        assert_edit_schema(&protocol.edit_tool(&body), false);
        protocol.calls_reply(
            request,
            json!([protocol.call("read-first", "read", json!({"path":"demo.txt"}))]),
        );
        let (request, body) = protocol.receive(&server);
        let read = protocol.tool_result(&body, "read-first");
        let content = read["content"].as_str().unwrap();
        assert_eq!(
            hashline_anchors(content)
                .iter()
                .map(|(_, s)| s.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
        let header = hashline_header(content).to_owned();
        // 故意打乱操作顺序，所有行号仍属于原始快照。
        protocol.calls_reply(request,json!([protocol.call("edit-multi","edit",json!({"input":format!("{header}\nPUT 2.=2:\n+TWO\nPUT <1:\n+zero\nPUT >3:\n+four")}))]));
        let (request, body) = protocol.receive(&server);
        let result = protocol.tool_result(&body, "edit-multi");
        assert_eq!(
            result,
            json!({"ok":true,"edits_applied":3,"bytes_written":24}),
            "{protocol:?}: {result}"
        );
        assert_eq!(
            std::fs::read(&demo).unwrap(),
            b"zero\none\nTWO\nthree\nfour\n"
        );
        // 成功后旧快照失效，重新读取才能继续。
        protocol.calls_reply(
            request,
            json!([
                protocol.call(
                    "edit-without-read",
                    "edit",
                    json!({"input":format!("{header}\nCUT 1.=1")})
                ),
                protocol.call("read-second", "read", json!({"path":"demo.txt"}))
            ]),
        );
        let (request, body) = protocol.receive(&server);
        assert_eq!(
            protocol.tool_result(&body, "edit-without-read")["ok"],
            false
        );
        let read = protocol.tool_result(&body, "read-second");
        let header = hashline_header(read["content"].as_str().unwrap()).to_owned();
        std::fs::write(&demo, "zero\none\nexternal\nthree\nfour\n").unwrap();
        protocol.calls_reply(
            request,
            json!([
                protocol.call(
                    "edit-stale",
                    "edit",
                    json!({"input":format!("{header}\nPUT 2.=2:\n+uno\nPUT 3.=3:\n+recovered")})
                ),
                protocol.call("read-third", "read", json!({"path":"demo.txt"}))
            ]),
        );
        let (request, body) = protocol.receive(&server);
        let failed = protocol.tool_result(&body, "edit-stale");
        assert_eq!(failed["ok"], false);
        assert!(
            failed["error"]
                .as_str()
                .unwrap()
                .contains("snapshot changed")
        );
        assert_eq!(
            std::fs::read(&demo).unwrap(),
            b"zero\none\nexternal\nthree\nfour\n"
        );
        let read = protocol.tool_result(&body, "read-third");
        let header = hashline_header(read["content"].as_str().unwrap());
        protocol.calls_reply(request,json!([protocol.call("edit-retry","edit",json!({"input":format!("*** Begin Patch\n{header}\nPUT 3.=3:\n+recovered\n*** End Patch")}))]));
        let (request, body) = protocol.receive(&server);
        assert_eq!(protocol.tool_result(&body, "edit-retry")["ok"], true);
        assert_eq!(
            std::fs::read(&demo).unwrap(),
            b"zero\none\nrecovered\nthree\nfour\n"
        );
        protocol.final_reply(request, "hashline-ok");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        assert_eq!(output.stdout, b"hashline-ok\n");
        assert_clean_pipes(&output);
    }
}

#[test]
fn replace_edit_roundtrip_and_atomic_failure() {
    for protocol in PROTOCOLS {
        for settings in [
            fixture_settings(), // no edit field: replace is the default
            r#"{"default_model":"fixture/primary","edit":"replace"}"#,
        ] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let demo = dir.path().join("demo.txt");
            let original = "keep “Ｋ”  \nsay “Hi”—x  \ntail\n";
            std::fs::write(&demo, original).unwrap();
            let cli =
                protocol.start_with_settings(&server, dir.path(), Some("edit demo.txt"), settings);

            // The schema actually sent exposes only replace parameters.
            let (request, body) = protocol.receive(&server);
            assert_edit_schema(&protocol.edit_tool(&body), true);

            // Plain read: the original text without anchors or prefixes.
            protocol.calls_reply(
                request,
                json!([protocol.call("read-first", "read", json!({"path":"demo.txt"}))]),
            );
            let (request, body) = protocol.receive(&server);
            let read = protocol.tool_result(&body, "read-first");
            assert_eq!(read["ok"], true, "{protocol:?}: {read}");
            assert_eq!(read["truncated"], false);
            assert_eq!(read["content"], original);

            // One batch with an exact and a loose match, deliberately out of
            // file order; both entries locate the original snapshot.
            protocol.calls_reply(
                request,
                json!([protocol.call(
                    "edit-batch",
                    "edit",
                    json!({
                        "path":"demo.txt",
                        "edits":[
                            {"oldText":"tail","newText":"TAIL"},
                            {"oldText":"say \"Hi\"-x","newText":"said"}
                        ]
                    })
                )]),
            );
            let (request, body) = protocol.receive(&server);
            let edited = protocol.tool_result(&body, "edit-batch");
            let expected = "keep “Ｋ”  \nsaid  \nTAIL\n";
            assert_eq!(edited["ok"], true, "{protocol:?}: {edited}");
            assert_eq!(edited["edits_applied"], 2);
            assert_eq!(edited["bytes_written"], expected.len());
            assert_eq!(std::fs::read(&demo).unwrap(), expected.as_bytes());

            // A valid entry plus a missing oldText rejects the whole batch.
            protocol.calls_reply(
                request,
                json!([protocol.call(
                    "edit-atomic",
                    "edit",
                    json!({
                        "path":"demo.txt",
                        "edits":[
                            {"oldText":"said","newText":"SHOUT"},
                            {"oldText":"not-in-the-file","newText":"x"}
                        ]
                    })
                )]),
            );
            let (request, body) = protocol.receive(&server);
            let failed = protocol.tool_result(&body, "edit-atomic");
            assert_eq!(failed["ok"], false, "{protocol:?}: {failed}");
            assert_eq!(std::fs::read(&demo).unwrap(), expected.as_bytes());

            // The corrected edit and a verifying read both succeed.
            protocol.calls_reply(
                request,
                json!([protocol.call(
                    "edit-retry",
                    "edit",
                    json!({
                        "path":"demo.txt",
                        "edits":[{"oldText":"said","newText":"recovered"}]
                    })
                )]),
            );
            let (request, body) = protocol.receive(&server);
            let recovered = protocol.tool_result(&body, "edit-retry");
            assert_eq!(recovered["ok"], true, "{protocol:?}: {recovered}");
            let final_text = "keep “Ｋ”  \nrecovered  \nTAIL\n";
            assert_eq!(std::fs::read(&demo).unwrap(), final_text.as_bytes());
            protocol.calls_reply(
                request,
                json!([protocol.call("read-final", "read", json!({"path":"demo.txt"}))]),
            );
            let (request, body) = protocol.receive(&server);
            let read = protocol.tool_result(&body, "read-final");
            assert_eq!(read["ok"], true);
            assert_eq!(read["content"], final_text);
            protocol.final_reply(request, "replace-ok");
            let output = cli.finish();
            assert!(output.status.success(), "{protocol:?}: {output:?}");
            assert_eq!(output.stdout, b"replace-ok\n");
            assert_clean_pipes(&output);
            no_more(&server);
        }
    }
}

#[test]
fn edit_mode_survives_model_switch() {
    for replace in [true, false] {
        let a = server();
        let b = server();
        let dir = tempfile::tempdir().unwrap();
        let demo = dir.path().join("demo.txt");
        std::fs::write(&demo, "one\n").unwrap();
        let models = json!({"providers": {
            "a": slash_provider(&a, "a", "completion", json!({"primary": {"model":"vendor-a"}})),
            "b": slash_provider(&b, "b", "response", json!({"secondary": {"model":"vendor-b"}}))}})
        .to_string();
        let settings = if replace {
            r#"{"default_model":"a/primary","edit":"replace"}"#
        } else {
            r#"{"default_model":"a/primary","edit":"hashline"}"#
        };
        let mut cli = Cli::start_with(dir.path(), None, settings, &models);
        let mut current = "one";

        // Completion -> Responses -> Completion with the same edit mode.
        for (step, (protocol, switch)) in [
            (Protocol::Chat, ""),
            (Protocol::Responses, "/model b/secondary\n"),
            (Protocol::Chat, "/model a/primary\n"),
        ]
        .into_iter()
        .enumerate()
        {
            let next = ["two", "three", "four"][step];
            cli.input(&format!("{switch}stage {step}\n"));
            let stage_server = match protocol {
                Protocol::Chat => &a,
                Protocol::Responses => &b,
            };

            // The schema still exposes the configured mode after each switch.
            let (request, body) = protocol.receive(stage_server);
            assert_edit_schema(&protocol.edit_tool(&body), replace);

            // A fresh read, never a replayed tool result from history.
            protocol.calls_reply(
                request,
                json!([protocol.call(&format!("read-{step}"), "read", json!({"path":"demo.txt"}))]),
            );
            let (request, body) = protocol.receive(stage_server);
            let read = protocol.tool_result(&body, &format!("read-{step}"));
            assert_eq!(read["ok"], true, "{protocol:?}: {read}");
            let edit_args = if replace {
                json!({"path":"demo.txt","edits":[
                    {"oldText":current,"newText":next}
                ]})
            } else {
                // Hashline anchors come from this stage's read.
                let anchors = hashline_anchors(read["content"].as_str().unwrap());
                assert_eq!(anchors.len(), 1, "{protocol:?}: {read}");
                assert_eq!(anchors[0].1, current);
                json!({"input":format!("{}\nPUT 1.=1:\n+{next}",hashline_header(read["content"].as_str().unwrap()))})
            };
            protocol.calls_reply(
                request,
                json!([protocol.call(&format!("edit-{step}"), "edit", edit_args)]),
            );
            let (request, body) = protocol.receive(stage_server);
            let edited = protocol.tool_result(&body, &format!("edit-{step}"));
            assert_eq!(edited["ok"], true, "{protocol:?}: {edited}");
            assert_eq!(edited["edits_applied"], 1);
            assert_eq!(
                std::fs::read(&demo).unwrap(),
                format!("{next}\n").as_bytes()
            );
            protocol.final_reply(request, &format!("done-{step}"));
            current = next;
        }

        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"done-0\ndone-1\ndone-2\n");
        assert_clean_pipes(&output);
        no_more(&a);
        no_more(&b);
    }
}

#[test]
fn hardened_tools_recover_after_limits_timeout_and_rejected_edits_in_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let external = outside.path().join("external");
        std::fs::write(&external, "keep").unwrap();
        std::fs::write(dir.path().join("demo"), "one\ntwo\nthree\n").unwrap();
        let settings = r#"{"read_max_bytes":8,"read_max_lines":1,"bash_timeout_seconds":1}"#;
        let cli =
            protocol.start_with_settings(&server, dir.path(), Some("hardened tools"), settings);
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(request, json!([
            protocol.call("page", "read", json!({"path":"demo"})),
            protocol.call("bad-limit", "read", json!({"path":"demo","limit":2})),
            protocol.call("outside", "write", json!({"path":external,"content":"bad"})),
            protocol.call("bad-edit", "edit", json!({"path":"demo","edits":[
                {"oldText":"one","newText":"bad"}, {"oldText":"missing","newText":"bad"}
            ]})),
            protocol.call("timeout", "bash", json!({"command":"printf before; printf error >&2; sleep 10; printf late > late"})),
            protocol.call("after-timeout", "write", json!({"path":"after","content":"kept\n"}))
        ]));
        let (request, body) = protocol.receive(&server);
        assert_eq!(
            protocol.tool_result(&body, "page"),
            json!({"ok":true,"source":"demo","content":"one\n","truncated":true,"lines_read":1,"next_offset":2,"next_path":"demo:2-","ranges":[{"start":1,"end":1}]})
        );
        for id in ["bad-limit", "outside", "bad-edit"] {
            assert_eq!(
                protocol.tool_result(&body, id)["ok"],
                false,
                "{protocol:?}: {id}"
            );
        }
        assert_eq!(
            std::fs::read(dir.path().join("demo")).unwrap(),
            b"one\ntwo\nthree\n"
        );
        assert_eq!(std::fs::read(&external).unwrap(), b"keep");
        let timeout = protocol.tool_result(&body, "timeout");
        assert_eq!(timeout["ok"], false);
        assert_eq!(timeout["timed_out"], true);
        assert_eq!(timeout["stdout"], "before");
        assert_eq!(timeout["stderr"], "error");
        assert!(!dir.path().join("late").exists());
        assert_eq!(protocol.tool_result(&body, "after-timeout")["ok"], true);
        protocol.calls_reply(
            request,
            json!([
                protocol.call("next-page", "read", json!({"path":"demo:2"})),
                protocol.call(
                    "repair",
                    "edit",
                    json!({"path":"demo","edits":[{"oldText":"two","newText":"fixed"}]})
                ),
                protocol.call("verify", "read", json!({"path":"demo:2"})),
                protocol.call("verify-after", "read", json!({"path":"after"}))
            ]),
        );
        let (request, body) = protocol.receive(&server);
        assert_eq!(protocol.tool_result(&body, "next-page")["content"], "two\n");
        assert_eq!(protocol.tool_result(&body, "repair")["ok"], true);
        assert_eq!(protocol.tool_result(&body, "verify")["content"], "fixed\n");
        assert_eq!(
            protocol.tool_result(&body, "verify-after")["content"],
            "kept\n"
        );
        assert_eq!(protocol.tool_result(&body, "timeout")["timed_out"], true);
        protocol.final_reply(request, "recovered");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"recovered\n");
        assert_clean_pipes(&output);
        no_more(&server);
    }
}

#[test]
fn tool_limits_remain_startup_snapshots_across_model_switch_and_new() {
    let a = server();
    let b = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("demo"), "one\ntwo\n").unwrap();
    let models = json!({"providers":{
        "a": slash_provider(&a, "a", "completion", json!({"primary": {"model":"vendor-a"}})),
        "b": slash_provider(&b, "b", "response", json!({"secondary": {"model":"vendor-b"}}))}})
    .to_string();
    let settings = r#"{"read_max_bytes":4,"read_max_lines":1,"bash_timeout_seconds":1}"#;
    let mut cli = Cli::start_with(dir.path(), None, settings, &models);
    for (step, protocol) in [Protocol::Chat, Protocol::Responses, Protocol::Chat]
        .into_iter()
        .enumerate()
    {
        let prefix = match step {
            0 => "",
            1 => "/model b/secondary\n/new\n",
            _ => "/model a/primary\n/new\n",
        };
        cli.input(&format!("{prefix}stage {step}\n"));
        let server = match protocol {
            Protocol::Chat => &a,
            Protocol::Responses => &b,
        };
        let (request, _) = protocol.receive(server);
        // 启动后改配置，切换模型和新建会话均不得放宽工具限制。
        std::fs::write(
            config_path(dir.path(), "settings.yml"),
            r#"{"read_max_bytes":65536,"read_max_lines":2000,"bash_timeout_seconds":120}"#,
        )
        .unwrap();
        let page = format!("page-{step}");
        let limit = format!("limit-{step}");
        let timeout = format!("timeout-{step}");
        protocol.calls_reply(
            request,
            json!([
                protocol.call(&page, "read", json!({"path":"demo"})),
                protocol.call(&limit, "read", json!({"path":"demo","limit":2})),
                protocol.call(&timeout, "bash", json!({"command":"sleep 10"}))
            ]),
        );
        let (request, body) = protocol.receive(server);
        assert_eq!(protocol.tool_result(&body, &page)["content"], "one\n");
        assert_eq!(protocol.tool_result(&body, &page)["next_offset"], 2);
        assert_eq!(protocol.tool_result(&body, &limit)["ok"], false);
        assert_eq!(protocol.tool_result(&body, &timeout)["timed_out"], true);
        protocol.final_reply(request, &format!("done-{step}"));
    }
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"done-0\ndone-1\ndone-2\n");
}

// 两种协议均验证进程内搜索和工具结果回填。
#[test]
fn glob_roundtrip_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("space dir")).unwrap();
        std::fs::write(
            dir.path().join("space dir/demo.txt"),
            "needle\nother\nneedle\n",
        )
        .unwrap();
        let models = match protocol {
            Protocol::Chat => fixture_models(&server),
            Protocol::Responses => response_models(&server, Some("response")),
        };
        std::fs::write(config_path(dir.path(), "settings.yml"), fixture_settings()).unwrap();
        std::fs::write(config_path(dir.path(), "models.yml"), models).unwrap();
        let mut cli = LiveCli::new(Cli::spawn(dir.path(), &["-p", "search files"], &[]));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([
                protocol.call(
                    "match",
                    "grep",
                    json!({"pattern":"needle","path":"space dir/demo.txt"})
                ),
                protocol.call("paths", "glob", json!({"path":"space dir/*.txt"})),
                protocol.call(
                    "absent",
                    "grep",
                    json!({"pattern":"absent","path":"space dir/demo.txt"})
                ),
                protocol.call(
                    "bad-regex",
                    "grep",
                    json!({"pattern":"[","path":"space dir/demo.txt"})
                ),
                protocol.call("bad-args", "grep", json!({"args":[]}))
            ]),
        );
        cli.wait_for(Pipe::Stderr, "*1|needle");
        cli.assert_running();
        let (request, body) = protocol.receive(&server);
        let matched = protocol.tool_result(&body, "match");
        assert_eq!(matched["ok"], true);
        assert!(matched["content"].as_str().unwrap().contains("*1|needle"));
        let found = protocol.tool_result(&body, "paths");
        assert_eq!(found["ok"], true);
        assert!(found["content"].as_str().unwrap().contains("demo.txt"));
        assert_eq!(
            protocol.tool_result(&body, "absent")["content"],
            "No matches found"
        );
        assert_eq!(
            protocol.tool_result(&body, "bad-regex")["content"],
            "No matches found"
        );
        assert_eq!(protocol.tool_result(&body, "bad-args")["ok"], false);
        protocol.final_reply(request, "search-complete");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        assert_eq!(output.stdout, b"search-complete\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("*1|needle"), "{stderr}");
        assert!(stderr.contains("demo.txt"), "{stderr}");
        assert!(!stderr.contains("[bash"), "{stderr}");
        assert!(stderr.contains("tool grep: ok"), "{stderr}");
        assert!(stderr.contains("invalid grep arguments"), "{stderr}");
        assert!(stderr.contains("tool glob: ok"), "{stderr}");
        assert_clean_pipes(&output);
        no_more(&server);
    }
}

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
#[test]
fn background_service_lifecycle_in_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let settings = r#"{"default_model":"fixture/primary","bash_timeout_seconds":1}"#;
        let cli =
            protocol.start_with_settings(&server, dir.path(), Some("background service"), settings);
        let service_pid = dir.path().join("service.pid");
        let start_command = format!(
            "printf '%s' $$ > service.pid && exec '{}' stats --port 0",
            env!("CARGO_BIN_EXE_pp").replace('\'', "'\\''")
        );
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "start",
                "bash",
                json!({"command": start_command, "mode": "background"})
            )]),
        );
        let (mut request, body) = protocol.receive(&server);
        let started = protocol.tool_result(&body, "start");
        assert_eq!(started["ok"], true, "{protocol:?}: {started}");
        let task_id = started["task_id"].as_str().unwrap().to_owned();
        assert!(task_id.starts_with("bg-"), "{protocol:?}: {started}");

        // Readiness barrier: poll bash_task status until the server prints its
        // URL, under a bounded deadline and never an unbounded poll.
        let ready = Instant::now() + Duration::from_secs(5);
        let address;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let call_id = format!("ready-{attempt}");
            protocol.calls_reply(
                request,
                json!([protocol.call(
                    &call_id,
                    "bash_task",
                    json!({"task_id": task_id, "action": "status"})
                )]),
            );
            let (next, body) = protocol.receive(&server);
            request = next;
            let status = protocol.tool_result(&body, &call_id);
            assert_eq!(status["ok"], true, "{protocol:?}: {status}");
            assert_eq!(status["status"], "running", "{protocol:?}: {status}");
            if let Some((_, tail)) = status["stdout"]
                .as_str()
                .and_then(|text| text.split_once("pp stats: http://"))
            {
                address = tail.split('/').next().unwrap().to_owned();
                break;
            }
            assert!(
                Instant::now() < ready,
                "{protocol:?}: service never became ready: {status}"
            );
            thread::sleep(Duration::from_millis(20));
        }

        // Hold this model request past the foreground deadline (1s timeout +
        // 1s grace): the background task must still be alive and serving.
        thread::sleep(Duration::from_millis(2500));
        assert!(
            rustix::process::test_kill_process(background_pid(&service_pid)).is_ok(),
            "{protocol:?}: background service died within the foreground deadline"
        );
        let probe_command = format!(
            "curl --fail --silent --show-error --max-time 1 \
             --write-out '\\nHTTP_STATUS:%{{http_code}}\\n' \
             'http://{address}/api/stats/overview?range=24h'"
        );
        protocol.calls_reply(
            request,
            json!([protocol.call("probe", "bash", json!({"command": probe_command}))]),
        );
        let (request, body) = protocol.receive(&server);
        let probe = protocol.tool_result(&body, "probe");
        assert_eq!(probe["ok"], true, "{protocol:?}: {probe}");
        assert_eq!(probe["exit_code"], 0, "{protocol:?}: {probe}");
        assert_eq!(probe["timed_out"], false, "{protocol:?}: {probe}");
        let stdout = probe["stdout"].as_str().unwrap();
        let (payload, tail) = stdout.rsplit_once("\nHTTP_STATUS:").unwrap_or_else(|| {
            panic!("{protocol:?}: probe output missing status marker: {stdout}")
        });
        let overview: Value = serde_json::from_str(payload.trim()).unwrap_or_else(|error| {
            panic!("{protocol:?}: probe body is not JSON ({error}): {payload}")
        });
        assert!(
            overview["summary"].get("requests").is_some(),
            "{protocol:?}: {overview}"
        );
        assert_eq!(tail, "200\n", "{protocol:?}: {stdout}");

        protocol.calls_reply(
            request,
            json!([protocol.call(
                "stop",
                "bash_task",
                json!({"task_id": task_id, "action": "stop"})
            )]),
        );
        let (request, body) = protocol.receive(&server);
        let stopped = protocol.tool_result(&body, "stop");
        assert_eq!(stopped["ok"], true, "{protocol:?}: {stopped}");
        assert_eq!(stopped["task_id"], task_id.as_str());
        assert_eq!(stopped["status"], "stopped", "{protocol:?}: {stopped}");
        cancel_wait_pid_dead(&service_pid);
        assert!(
            std::net::TcpStream::connect(address.as_str()).is_err(),
            "{protocol:?}: stopped service still accepts connections on {address}"
        );
        cancel_assert_no_scratch(dir.path());

        // Stopping one background task must not cancel the running user task.
        protocol.calls_reply(
            request,
            json!([protocol.call("after", "bash", json!({"command": "printf task-continues"}))]),
        );
        let (request, body) = protocol.receive(&server);
        let after = protocol.tool_result(&body, "after");
        assert_eq!(after["ok"], true, "{protocol:?}: {after}");
        assert_eq!(after["exit_code"], 0, "{protocol:?}: {after}");
        assert_eq!(after["stdout"], "task-continues", "{protocol:?}: {after}");
        protocol.final_reply(request, "background-service-verified");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        assert_eq!(
            output.stdout, b"background-service-verified\n",
            "{protocol:?}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("task {task_id}: running")),
            "{protocol:?}: {stderr}"
        );
        assert!(
            stderr.contains(&format!("task {task_id}: stopped")),
            "{protocol:?}: {stderr}"
        );
        assert!(
            stderr.contains("pp stats: http://127.0.0.1:"),
            "{protocol:?}: {stderr}"
        );
        assert!(stderr.contains("HTTP_STATUS"), "{protocol:?}: {stderr}");
        assert_clean_pipes(&output);
        no_more(&server);
    }
}

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
#[test]
fn background_task_cleanup_at_run_boundaries() {
    for protocol in PROTOCOLS {
        for boundary in [RunBoundary::FinalAnswer, RunBoundary::ModelError] {
            let server = server();
            let dir = tempfile::tempdir().unwrap();
            let settings = r#"{"default_model":"fixture/primary"}"#;
            std::fs::write(dir.path().join("marker"), "marker\n").unwrap();
            let service_pid = dir.path().join("service.pid");
            let second_pid = dir.path().join("second.pid");
            let mut cli = protocol.start_with_settings(&server, dir.path(), None, settings);
            cli.input("start a background task\n");
            let (request, _) = protocol.receive(&server);
            protocol.calls_reply(
                request,
                json!([protocol.call(
                    "start",
                    "bash",
                    json!({
                        // TERM 清理同时写满两条管道；任务收尾必须持续排空至标记落盘。
                        "command": "cleanup() { for ((i=0;i<64;i++)); do printf '%4096s' ''; printf '%4096s' '' >&2; done; printf done > cleanup.done; exit 0; }; trap cleanup TERM; printf '%s' $$ > service.pid; while :; do sleep 60 & wait $!; done",
                        "mode": "background"
                    })
                )]),
            );
            let (request, body) = protocol.receive(&server);
            let started = protocol.tool_result(&body, "start");
            assert_eq!(started["ok"], true, "{protocol:?} {boundary:?}: {started}");
            let old_id = started["task_id"].as_str().unwrap().to_owned();
            assert!(rustix::process::test_kill_process(background_pid(&service_pid)).is_ok());

            match boundary {
                RunBoundary::FinalAnswer => {
                    protocol.final_reply(request, "boundary-answer");
                    cli.wait_for(Pipe::Stdout, "boundary-answer");
                }
                RunBoundary::ModelError => {
                    request.respond(Response::from_string(
                        json!({"error":{"message":"fixture failure","type":"server_error"}}).to_string()
                    ).with_status_code(500)).unwrap();
                    cli.wait_for(Pipe::Stderr, "error: ");
                }
            }
            cancel_wait_pid_dead(&service_pid);
            assert_eq!(
                std::fs::read_to_string(dir.path().join("cleanup.done")).unwrap(),
                "done",
                "{protocol:?} {boundary:?}: TERM 清理未完成"
            );
            cancel_assert_no_scratch(dir.path());
            cli.assert_running();

            cli.input("next round\n");
            let (request, _) = protocol.receive(&server);
            protocol.calls_reply(
                request,
                json!([
                    protocol.call(
                        "old-status",
                        "bash_task",
                        json!({"task_id": old_id, "action": "status"})
                    ),
                    protocol.call(
                        "second-start",
                        "bash",
                        json!({
                            "command": "printf '%s' $$ > second.pid && exec sleep 60",
                            "mode": "background"
                        })
                    )
                ]),
            );
            let (request, body) = protocol.receive(&server);
            let old_status = protocol.tool_result(&body, "old-status");
            assert_eq!(
                old_status["ok"], false,
                "{protocol:?} {boundary:?}: {old_status}"
            );
            assert!(
                old_status["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("Unknown background task")),
                "{protocol:?} {boundary:?}: {old_status}"
            );
            let second = protocol.tool_result(&body, "second-start");
            assert_eq!(second["ok"], true, "{protocol:?} {boundary:?}: {second}");
            assert_eq!(
                second["status"], "running",
                "{protocol:?} {boundary:?}: {second}"
            );
            let second_id = second["task_id"].as_str().unwrap().to_owned();
            assert_ne!(second_id, old_id, "{protocol:?} {boundary:?}");
            assert!(rustix::process::test_kill_process(background_pid(&second_pid)).is_ok());
            protocol.final_reply(request, "boundary-cleanup-complete");
            cli.wait_for(Pipe::Stdout, "boundary-cleanup-complete");
            cancel_wait_pid_dead(&service_pid);
            cancel_wait_pid_dead(&second_pid);
            cancel_assert_no_scratch(dir.path());
            cli.input("/quit\n");
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{protocol:?} {boundary:?}: {output:?}"
            );
            let expected_stdout = match boundary {
                RunBoundary::FinalAnswer => "boundary-answer\nboundary-cleanup-complete\n",
                RunBoundary::ModelError => "boundary-cleanup-complete\n",
            };
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                expected_stdout,
                "{protocol:?} {boundary:?}"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(&format!("task {old_id}: running")),
                "{protocol:?} {boundary:?}: {stderr}"
            );
            assert!(
                stderr.contains(&format!("task {second_id}: running")),
                "{protocol:?} {boundary:?}: {stderr}"
            );
            assert_clean_pipes(&output);
            no_more(&server);
        }
    }
}

#[test]
fn grep_hashline_edit_and_pagination_roundtrip_both_protocols() {
    for protocol in PROTOCOLS {
        let provider = server();
        let dir = tempfile::tempdir().unwrap();
        for n in 0..21 {
            std::fs::write(
                dir.path().join(format!("{n:02}.txt")),
                "before\nneedle\nafter\n",
            )
            .unwrap();
        }
        let cli = protocol.start_with_settings(
            &provider,
            dir.path(),
            Some("search and edit"),
            r#"{"edit":"hashline"}"#,
        );
        let (request, body) = protocol.receive(&provider);
        let tool = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool.get("function").unwrap_or(tool))
            .find(|tool| tool["name"] == "grep")
            .unwrap();
        assert!(tool["parameters"]["properties"]["pattern"].is_object());
        protocol.calls_reply(
            request,
            json!([
                protocol.call(
                    "page",
                    "grep",
                    json!({"pattern":"needle","path":"*.txt","skip":20})
                ),
                protocol.call("file", "grep", json!({"pattern":"needle","path":"00.txt"}))
            ]),
        );
        let (request, body) = protocol.receive(&provider);
        assert_eq!(
            protocol.tool_result(&body, "page")["details"]["fileCount"],
            1
        );
        let result = protocol.tool_result(&body, "file");
        let header = hashline_header(result["content"].as_str().unwrap());
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "edit",
                "edit",
                json!({"input":format!("{header}\nPUT 2.=2:\n+changed")})
            )]),
        );
        let (request, body) = protocol.receive(&provider);
        assert_eq!(protocol.tool_result(&body, "edit")["ok"], true);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("00.txt")).unwrap(),
            "before\nchanged\nafter\n"
        );
        protocol.final_reply(request, "done");
        let result = cli.finish();
        assert!(result.status.success());
        no_more(&provider);
    }
}
