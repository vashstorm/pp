// TUI 任务级取消回归（计划验证项 1、2、4）：两协议 headers 前/SSE 空闲取消
// 与真实连接关闭、工具批次取消停止真实进程组、取消终态持久化与重启恢复合法性，
// 以及取消收尾持久化失败优先显示 persisting session。全部通过真实 PTY、本地
// loopback HTTP、原始 TCP fixture、进程/文件 marker 作栅栏观察行为，不用 sleep
// 猜测阶段。辅助项均以 `cancel_` 前缀命名，不新增 use 声明，避免与兄弟 include
// 文件冲突。文件后半为 `background_` 前缀的后台任务 PTY 场景：Esc 取消清理、
// Ctrl-C 退出闭环与 status/stop 生命周期展示。

// 取消收尾唯一可见的状态行；取消不再向对话追加提示块。
const CANCEL_DONE: &str = "✻ Cancelled ·";
const CANCEL_SETTINGS: &str = "{\"default_model\":\"fixture/primary\",\"request_timeout_seconds\":120,\"notification_timeout_seconds\":2}";
// 显式外部 sh 提供独立 shell PID（内嵌 shell 不伪造 $$）；父 sh 忽略 TERM，
// 证明取消路径用 grace 后的 KILL 回收整个进程组。
const CANCEL_BATCH_COMMAND: &str = "sh -c 'trap \"\" TERM; printf \"cancel-ready\\n\"; printf \"%s\" \"$$\" > bash.pid; sleep 30 & child=$!; printf \"%s\" \"$child\" > bash-child.pid; wait \"$child\"'";

fn cancel_normalize(screen: &vt100::Screen) -> String {
    screen.contents().split_whitespace().collect::<String>()
}

fn cancel_count_normalized(screen: &vt100::Screen, needle: &str) -> usize {
    cancel_normalize(screen)
        .matches(&needle.split_whitespace().collect::<String>())
        .count()
}

// 取消收尾的最终帧栅栏：过渡帧可以先写出取消状态行，只有原生光标回到输入行
// 才代表这一帧真正画完。
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

// 取消完成后：5 秒内回到 Ready、仍在主屏幕、恰好一条取消完成状态行，
// 且光标已回到输入行（最终帧）。
fn cancel_wait_cancelled_ready(pty: &mut Pty) {
    pty.wait_for("取消后回到 Ready 且仅一条取消状态行", |screen| {
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

// 按 history 顺序返回 (call_id, result JSON)，覆盖两种协议的成对工具结果。
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
            // 只统计带 tool_calls 的 assistant；纯文本回复没有调用。
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
    assert!(
        fixture
            .recv_timeout(Duration::from_millis(300))
            .unwrap()
            .is_none(),
        "取消后出现了多余的模型请求"
    );
}

// 驱动 write→bash→write 批次并等到 bash 进入等待阶段。
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
    // 文件 marker 是进入 bash 执行阶段的栅栏，不用 sleep 猜测。
    cancel_wait_pid_files(project, &["bash.pid", "bash-child.pid"]);
    pty.text("Timeout: 60s");
}

// 批次取消后的真实副作用：保留的文件不变、跳过的文件不存在、两个 PID 停止、
// scratch 清理。
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
        "三个 tool result 必须与原 call_id 按序配对"
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

// —— 验证项 1 的原始 TCP fixture：这里读完整 Content-Length 请求并可检测
// peer EOF/reset，共享测试传输无法观察对端关闭。 ——

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
                panic!("模型连接在请求到达前就被关闭")
            }
            Err(_) => panic!("等待原始 fixture 上的模型请求超时"),
        }
    }

    // 旧连接的 EOF/reset 必须在提交下一任务之前就能观察到。
    fn expect_close(&self) {
        match self.events.recv_timeout(DEADLINE) {
            Ok(CancelRawEvent::Closed) => {}
            Ok(CancelRawEvent::Request { .. }) => {
                panic!("等待连接关闭时收到了多余的模型请求")
            }
            Err(_) => panic!("被取消的模型连接在截止时间内没有关闭"),
        }
    }

    fn hold(&self) {
        self.control
            .send(CancelRawCommand::Hold)
            .expect("raw fixture 线程存活");
    }

    fn delta(&self, protocol: &'static str, text: &str) {
        self.control
            .send(CancelRawCommand::Delta {
                protocol,
                text: text.to_owned(),
            })
            .expect("raw fixture 线程存活");
    }

    fn final_answer(&self, protocol: &'static str, text: &str) {
        self.control
            .send(CancelRawCommand::Final {
                protocol,
                text: text.to_owned(),
            })
            .expect("raw fixture 线程存活");
    }

    fn assert_silent(&self) {
        assert!(
            self.events
                .recv_timeout(Duration::from_millis(300))
                .is_err(),
            "取消后原始 fixture 上出现了多余的模型流量"
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

// 读取完整 Content-Length 请求；header 之后不再消费对端数据，只用于检测关闭。
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
        .expect("HTTP 请求行包含路径")
        .to_owned();
    let length: usize = head
        .split("\r\n")
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|line| line.rsplit(':').next())
        .and_then(|value| value.trim().parse().ok())
        .expect("模型请求必须携带 Content-Length");
    let body_start = header_end + 4;
    while buffer.len() < body_start + length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8(buffer[body_start..body_start + length].to_vec()).ok()?;
    let body: Value = serde_json::from_str(&body).expect("模型请求体是合法 JSON");
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

    // 与现有 streaming_delta 相同的字节格式。
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

    // 与现有 streaming_complete 相同的字节格式。
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
