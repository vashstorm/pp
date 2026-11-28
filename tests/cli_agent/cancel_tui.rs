// TUI task-level cancellation regression (plan verification items 1, 2, 4): cancelling before the
// headers / while the SSE stream is idle for both protocols, together with the real connection close;
// tool-batch cancellation stopping the real process group; persistence of the cancelled terminal
// state and the legality of restart recovery; and a persistence failure during cancellation cleanup
// surfacing "persisting session" first. Everything observes behavior through the real PTY, local
// loopback HTTP, a raw TCP fixture and process/file markers as barriers, never by guessing stages with
// sleep. Helpers are all named with the `cancel_` prefix and no use declaration is added, to avoid
// clashing with sibling include files. The second half holds `background_`-prefixed background task
// PTY scenarios: Esc cancellation cleanup, the Ctrl-C exit loop and status/stop lifecycle rendering.

// The only visible status line of cancellation cleanup; cancelling no longer appends a hint block to
// the conversation.
const CANCEL_DONE: &str = "✻ Cancelled ·";
const CANCEL_SETTINGS: &str = "{\"default_model\":\"fixture/primary\",\"request_timeout_seconds\":120,\"notification_timeout_seconds\":2}";
// An explicit external sh supplies a standalone shell PID (the embedded shell does not fake $$); the
// parent sh ignores TERM, proving the cancellation path reclaims the whole process group with KILL
// after the grace period.
const CANCEL_BATCH_COMMAND: &str = "sh -c 'trap \"\" TERM; printf \"cancel-ready\\n\"; printf \"%s\" \"$$\" > bash.pid; sleep 30 & child=$!; printf \"%s\" \"$child\" > bash-child.pid; wait \"$child\"'";

fn cancel_normalize(screen: &vt100::Screen) -> String {
    screen.contents().split_whitespace().collect::<String>()
}

fn cancel_count_normalized(screen: &vt100::Screen, needle: &str) -> usize {
    cancel_normalize(screen)
        .matches(&needle.split_whitespace().collect::<String>())
        .count()
}

// Final-frame barrier for cancellation cleanup: a transition frame may already write the cancelled
// status line, and only the native cursor returning to the input row means the frame is really painted.
fn cancel_cursor_on_input_row(screen: &vt100::Screen) -> bool {
    let Some(row) = screen
        .contents()
        .lines()
        .position(|line| line.starts_with('❯'))
    else {
        return false;
    };
    let (cursor_row, _column) = screen.cursor_position();
    !screen.hide_cursor() && usize::from(cursor_row) == row
}

// After cancellation completes: back to Ready within 5 seconds, still on the main screen, exactly one
// cancellation status line and the cursor back on the input row (final frame).
fn cancel_wait_cancelled_ready(pty: &mut Pty) {
    pty.wait_for("Ready is restored after cancelling with exactly one cancellation status line", |screen| {
        input_is_ready(screen)
            && !screen.alternate_screen()
            && cancel_cursor_on_input_row(screen)
            && cancel_count_normalized(screen, CANCEL_DONE) == 1
    });
}

fn cancel_user_messages(body: &Value, protocol: &str) -> Vec<String> {
    let items = body[if protocol == "completion" {
        "messages"
    } else {
        "input"
    }]
    .as_array()
    .unwrap();
    items
        .iter()
        .filter(|item| item["role"] == "user")
        .map(slash_message_text)
        .collect()
}

fn cancel_assistant_texts(body: &Value, protocol: &str) -> Vec<String> {
    let items = body[if protocol == "completion" {
        "messages"
    } else {
        "input"
    }]
    .as_array()
    .unwrap();
    items
        .iter()
        .filter(|item| item["role"] == "assistant" && !item["content"].is_null())
        .map(slash_message_text)
        .filter(|text| !text.is_empty())
        .collect()
}

// Return (call_id, result JSON) in history order, covering the paired tool results of both protocols.
fn cancel_tool_outputs(body: &Value, protocol: &str) -> Vec<(String, Value)> {
    let items = body[if protocol == "completion" {
        "messages"
    } else {
        "input"
    }]
    .as_array()
    .unwrap();
    items
        .iter()
        .filter_map(|item| {
            if protocol == "completion" && item["role"] == "tool" {
                let id = item["tool_call_id"].as_str().unwrap();
                Some((id.to_owned(), result(item, id)))
            } else if protocol == "response" && item["type"] == "function_call_output" {
                let id = item["call_id"].as_str().unwrap();
                Some((id.to_owned(), response_result(item, id)))
            } else {
                None
            }
        })
        .collect()
}

fn cancel_called_ids(body: &Value, protocol: &str) -> Vec<String> {
    if protocol == "completion" {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            // Count only assistants with tool_calls; plain text replies contain no calls.
            .filter(|item| item["role"] == "assistant" && item["tool_calls"].is_array())
            .flat_map(|item| item["tool_calls"].as_array().unwrap().iter())
            .map(|call| call["id"].as_str().unwrap().to_owned())
            .collect()
    } else {
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["type"] == "function_call")
            .map(|item| item["call_id"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn cancel_assert_no_request(fixture: &TestServer) {
    let deadline = Instant::now() + Duration::from_millis(300);
    while let Some(request) = fixture
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .unwrap()
    {
        let body = request.json().unwrap();
        assert!(
            is_task_title(&body),
            "unexpected extra model request after cancelling: {body}"
        );
        // The title side channel may arrive after the main task's terminal state; close the response so
        // the client does not wait until the timeout.
        let _ = request.respond(Response::builder().status(204).body(Body::empty()).unwrap());
    }
}

// Drive a write→bash→write batch and wait until bash enters its waiting phase.
fn cancel_drive_batch(
    pty: &mut Pty,
    fixture: &TestServer,
    protocol: &str,
    prompt: &str,
    project: &Path,
) {
    pty.input(&format!("{prompt}\r"));
    let (request, _) = pty.request_at(fixture, api_path(protocol));
    let batch = [
        (
            "batch-kept",
            "write",
            json!({"path":"kept.txt","content":"kept"}),
        ),
        (
            "batch-bash",
            "bash",
            json!({"command":CANCEL_BATCH_COMMAND,"timeout_seconds":60}),
        ),
        (
            "batch-skip",
            "write",
            json!({"path":"skipped.txt","content":"must-not-exist"}),
        ),
    ];
    if protocol == "completion" {
        calls_reply(
            request,
            batch
                .iter()
                .map(|(id, name, args)| call(id, name, args.clone()))
                .collect(),
        );
    } else {
        response_reply(
            request,
            Value::Array(
                batch
                    .iter()
                    .map(|(id, name, args)| response_call(id, name, args.clone()))
                    .collect(),
            ),
        );
    }
    // The file marker is the barrier for entering the bash execution phase; no sleep guessing.
    cancel_wait_pid_files(project, &["bash.pid", "bash-child.pid"]);
    pty.text("Timeout: 60s");
}

// Real side effects after the batch is cancelled: kept files unchanged, skipped files absent, both
// PIDs stopped, scratch cleaned up.
fn cancel_assert_batch_stopped(project: &Path) {
    assert_eq!(
        std::fs::read_to_string(project.join("kept.txt")).unwrap(),
        "kept"
    );
    assert!(!project.join("skipped.txt").exists());
    cancel_wait_pid_dead(&project.join("bash.pid"));
    cancel_wait_pid_dead(&project.join("bash-child.pid"));
    cancel_assert_no_scratch(project);
}

fn cancel_assert_paired_results(body: &Value, protocol: &str) {
    let outputs = cancel_tool_outputs(body, protocol);
    assert_eq!(
        outputs
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["batch-kept", "batch-bash", "batch-skip"],
        "the three tool results must pair with their original call_id in order"
    );
    assert_eq!(
        cancel_called_ids(body, protocol),
        ["batch-kept", "batch-bash", "batch-skip"]
    );
    let kept = &outputs[0].1;
    assert_eq!(kept["ok"], true);
    assert_eq!(kept["bytes_written"], 4);
    let bash = &outputs[1].1;
    assert_eq!(bash["ok"], false);
    assert_eq!(bash["cancelled"], true);
    assert_eq!(bash["timed_out"], false);
    assert!(bash["output"].as_str().unwrap().contains("cancel-ready"));
    let skipped = &outputs[2].1;
    assert_eq!(skipped["ok"], false);
    assert_eq!(skipped["cancelled"], true);
    assert_eq!(skipped["error"], "Not executed: task cancelled.");
}

// --- Raw TCP fixture for verification item 1: it reads the full Content-Length request and can detect
// peer EOF/reset, which the shared test transport cannot observe. ---

enum CancelRawEvent {
    Request { path: String, body: Value },
    Closed,
}

enum CancelRawCommand {
    Hold,
    Delta {
        protocol: &'static str,
        text: String,
    },
    Final {
        protocol: &'static str,
        text: String,
    },
}

struct CancelRawModel {
    events: Receiver<CancelRawEvent>,
    control: mpsc::Sender<CancelRawCommand>,
    address: String,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl CancelRawModel {
    fn start() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let (event_sender, events) = mpsc::channel();
        let (control, control_receiver) = mpsc::channel();
        let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let worker_running = running.clone();
        let thread = thread::spawn(move || {
            cancel_raw_serve(listener, event_sender, control_receiver, worker_running);
        });
        Self {
            events,
            control,
            address,
            running,
            thread: Some(thread),
        }
    }

    fn models(&self, protocol: &str) -> String {
        json!({
            "providers":{"fixture": {"protocol":protocol,"api_key":"fixture-key",
                "base_url":format!("http://{}/v1", self.address),
                "models":{"primary": {"model":"fixture-model"}}}}
        })
        .to_string()
    }

    fn expect_request(&self) -> (String, Value) {
        match self.events.recv_timeout(DEADLINE) {
            Ok(CancelRawEvent::Request { path, body }) => (path, body),
            Ok(CancelRawEvent::Closed) => {
                panic!("the model connection was closed before the request arrived")
            }
            Err(_) => panic!("timed out waiting for a model request on the raw fixture"),
        }
    }

    // The old connection's EOF/reset must be observable before the next task is submitted.
    fn expect_close(&self) {
        match self.events.recv_timeout(DEADLINE) {
            Ok(CancelRawEvent::Closed) => {}
            Ok(CancelRawEvent::Request { .. }) => {
                panic!("received an extra model request while waiting for the connection to close")
            }
            Err(_) => panic!("the cancelled model connection did not close within the deadline"),
        }
    }

    fn hold(&self) {
        self.control
            .send(CancelRawCommand::Hold)
            .expect("raw fixture thread is alive");
    }

    fn delta(&self, protocol: &'static str, text: &str) {
        self.control
            .send(CancelRawCommand::Delta {
                protocol,
                text: text.to_owned(),
            })
            .expect("raw fixture thread is alive");
    }

    fn final_answer(&self, protocol: &'static str, text: &str) {
        self.control
            .send(CancelRawCommand::Final {
                protocol,
                text: text.to_owned(),
            })
            .expect("raw fixture thread is alive");
    }

    fn assert_silent(&self) {
        assert!(
            self.events
                .recv_timeout(Duration::from_millis(300))
                .is_err(),
            "extra model traffic appeared on the raw fixture after cancelling"
        );
    }
}

impl Drop for CancelRawModel {
    fn drop(&mut self) {
        self.running
            .store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn cancel_raw_serve(
    listener: std::net::TcpListener,
    events: mpsc::Sender<CancelRawEvent>,
    control: Receiver<CancelRawCommand>,
    running: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    while running.load(std::sync::atomic::Ordering::Relaxed) {
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(_) => break,
        };
        let Some((path, body)) = cancel_raw_read_request(&mut stream) else {
            break;
        };
        let _ = events.send(CancelRawEvent::Request { path, body });
        stream.set_nonblocking(true).unwrap();
        let mut connection = CancelRawStream::new(stream);
        while running.load(std::sync::atomic::Ordering::Relaxed) {
            match control.recv_timeout(Duration::from_millis(5)) {
                Ok(CancelRawCommand::Hold) => {}
                Ok(CancelRawCommand::Delta { protocol, text }) => connection.delta(protocol, &text),
                Ok(CancelRawCommand::Final { protocol, text }) => {
                    connection.final_reply(protocol, &text);
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if connection.closed() {
                let _ = events.send(CancelRawEvent::Closed);
                break;
            }
        }
    }
}

// Read the complete Content-Length request; after the headers the peer is no longer consumed, so this
// only serves to detect a close.
fn cancel_raw_read_request(stream: &mut std::net::TcpStream) -> Option<(String, Value)> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let path = head
        .split("\r\n")
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .expect("HTTP request line contains a path")
        .to_owned();
    let length: usize = head
        .split("\r\n")
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|line| line.rsplit(':').next())
        .and_then(|value| value.trim().parse().ok())
        .expect("model requests must carry Content-Length");
    let body_start = header_end + 4;
    while buffer.len() < body_start + length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8(buffer[body_start..body_start + length].to_vec()).ok()?;
    let body: Value = serde_json::from_str(&body).expect("model request body is valid JSON");
    Some((path, body))
}

struct CancelRawStream {
    stream: std::net::TcpStream,
    opened: bool,
    probe: [u8; 64],
}

impl CancelRawStream {
    fn new(stream: std::net::TcpStream) -> Self {
        Self {
            stream,
            opened: false,
            probe: [0; 64],
        }
    }

    fn open(&mut self) {
        if !self.opened {
            self.stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap();
            self.stream.flush().unwrap();
            self.opened = true;
        }
    }

    fn frame(&mut self, data: &str) {
        let frame = format!("data: {data}\n\n");
        write!(self.stream, "{:x}\r\n{}\r\n", frame.len(), frame).unwrap();
        self.stream.flush().unwrap();
    }

    // Same byte format as the existing streaming_delta.
    fn delta(&mut self, protocol: &str, text: &str) {
        self.open();
        if protocol == "completion" {
            self.frame(&chat_chunk(json!({"role":"assistant","content":text}), None).to_string());
        } else {
            self.frame(
                &json!({
                    "type":"response.output_text.delta", "sequence_number":0,
                    "item_id":"msg-live", "output_index":0, "content_index":0,
                    "delta":text, "logprobs":[]
                })
                .to_string(),
            );
        }
    }

    // Same byte format as the existing streaming_complete.
    fn final_reply(&mut self, protocol: &str, text: &str) {
        self.open();
        if protocol == "completion" {
            self.frame(&chat_chunk(json!({"role":"assistant","content":text}), None).to_string());
            self.frame(&chat_chunk(json!({}), Some("stop")).to_string());
            self.frame("[DONE]");
        } else {
            self.frame(
                &json!({
                    "type":"response.output_text.delta", "sequence_number":0,
                    "item_id":"msg-live", "output_index":0, "content_index":0,
                    "delta":text, "logprobs":[]
                })
                .to_string(),
            );
            self.frame(
                &json!({
                    "type":"response.completed", "sequence_number":1,
                    "response":response_body(json!([
                        response_message("msg-live", json!([response_text(text)]))
                    ]))
                })
                .to_string(),
            );
        }
        self.stream.write_all(b"0\r\n\r\n").unwrap();
        self.stream.flush().unwrap();
    }

    fn closed(&mut self) -> bool {
        match self.stream.read(&mut self.probe) {
            Ok(0) => true,
            Ok(_) => false,
            Err(error) => error.kind() != std::io::ErrorKind::WouldBlock,
        }
    }
}

include!("cancel_tui/lifecycle_cases.rs");
include!("cancel_tui/background_cases.rs");
