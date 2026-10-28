// TUI 任务级取消回归（计划验证项 1、2、4）：两协议 headers 前/SSE 空闲取消
// 与真实连接关闭、工具批次取消停止真实进程组、取消终态持久化与重启恢复合法性，
// 以及取消收尾持久化失败优先显示 persisting session。全部通过真实 PTY、本地
// loopback HTTP、原始 TCP fixture、进程/文件 marker 作栅栏观察行为，不用 sleep
// 猜测阶段。辅助项均以 `cancel_` 前缀命名，不新增 use 声明，避免与兄弟 include
// 文件冲突。文件后半为 `background_` 前缀的后台任务 PTY 场景：Esc 取消清理、
// Ctrl-C 退出闭环与 status/stop 生命周期展示。

const CANCEL_NOTE: &str = "Task cancelled. File changes were not rolled back.";
const CANCEL_SETTINGS: &str = "{\"default_model\":\"fixture/primary\",\"request_timeout_seconds\":120,\"notification_timeout_seconds\":2}";
// $$/$! 而非 BASHPID：macOS 旧 Bash 缺少 BASHPID；父 shell 忽略 TERM，
// 证明取消路径用 grace 后的 KILL 回收整个进程组。
const CANCEL_BATCH_COMMAND: &str = "trap '' TERM; printf 'cancel-ready\\n'; printf '%s' \"$$\" > bash.pid; sleep 30 & child=$!; printf '%s' \"$child\" > bash-child.pid; wait \"$child\"";

fn cancel_normalize(screen: &vt100::Screen) -> String {
    screen.contents().split_whitespace().collect::<String>()
}

fn cancel_count_normalized(screen: &vt100::Screen, needle: &str) -> usize {
    cancel_normalize(screen)
        .matches(&needle.split_whitespace().collect::<String>())
        .count()
}

// 取消收尾的最终帧栅栏：过渡帧可以先写出 Note，只有原生光标回到输入行
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

// 取消完成后：5 秒内回到 Ready、仍在主屏幕、恰好一条持久取消 Note，
// 且光标已回到输入行（最终帧）。
fn cancel_wait_cancelled_ready(pty: &mut Pty) {
    pty.wait_for("取消后回到 Ready 且仅一条持久 Note", |screen| {
        input_is_ready(screen)
            && !screen.alternate_screen()
            && cancel_cursor_on_input_row(screen)
            && cancel_count_normalized(screen, CANCEL_NOTE) == 1
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

fn cancel_assert_no_request(fixture: &Server) {
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
    fixture: &Server,
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
            json!({"command":CANCEL_BATCH_COMMAND}),
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
    assert!(bash["stdout"].as_str().unwrap().contains("cancel-ready"));
    let skipped = &outputs[2].1;
    assert_eq!(skipped["ok"], false);
    assert_eq!(skipped["cancelled"], true);
    assert_eq!(skipped["error"], "Not executed: task cancelled.");
}

// —— 验证项 1 的原始 TCP fixture：tiny_http 的 SseReply 只能写不能观察
// 对端关闭，这里读完整 Content-Length 请求并可检测 peer EOF/reset。 ——

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

// 验证项 1：两协议各跑 headers 前与部分 delta 后两种阶段。Esc 取消后旧连接
// 必须在提交下一任务前真的关闭；无隐式重试；草稿/光标/alternate screen 保留；
// 新请求原生历史包含两条用户消息各一次、没有半成品 assistant。
#[test]
fn tui_cancel_idle_headers_and_sse_close_connection_before_next_request_and_recover() {
    for protocol in ["completion", "response"] {
        for phase in ["headers", "sse"] {
            let raw = CancelRawModel::start();
            let home = tempfile::tempdir().unwrap();
            std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
            std::fs::write(config_path(home.path(), "models.yml"), raw.models(protocol)).unwrap();
            let mut pty = Pty::start_with_existing_files(home.path());
            pty.ready();
            pty.input("cancel idle prompt\r");
            let (path, body) = raw.expect_request();
            assert_eq!(path, api_path(protocol));
            let first_body = body.clone();
            assert_eq!(
                cancel_user_messages(&body, protocol),
                ["cancel idle prompt"]
            );
            if phase == "sse" {
                raw.delta(protocol, "partial-cancel-token");
                pty.text("partial-cancel-token");
            } else {
                raw.hold();
            }
            pty.input("保留草稿");
            pty.draft("保留草稿");
            pty.input("\x1b");
            cancel_wait_cancelled_ready(&mut pty);
            if phase == "sse" {
                assert_eq!(
                    cancel_count_normalized(pty.parser.screen(), "partial-cancel-token"),
                    1,
                    "已显示的 partial 必须保留"
                );
            }
            // 旧连接在提交下一任务前已经关闭，而不是等下一次 block_on 才清理。
            raw.expect_close();
            // 不先发送新用户消息就没有任何额外模型请求。
            raw.assert_silent();
            // 光标在草稿末尾：追加一个字符证明草稿与光标都被保留。
            pty.input("字");
            pty.draft("保留草稿字");
            pty.input("\r");
            let (path, body) = raw.expect_request();
            assert_eq!(path, api_path(protocol));
            let text = serde_json::to_string(&body).unwrap();
            assert!(!text.contains("pp.cancelled"), "pp.cancelled 不得进入请求");
            assert!(
                !text.contains("partial-cancel-token"),
                "半成品 assistant 不得进入原生历史"
            );
            if protocol == "completion" {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(
                    messages
                        .iter()
                        .filter(|item| item["role"] == "user")
                        .count(),
                    2
                );
                assert_eq!(
                    messages
                        .iter()
                        .filter(|item| item["role"] == "assistant")
                        .count(),
                    0
                );
                assert_eq!(messages[1]["content"], "cancel idle prompt");
                assert_eq!(messages[2]["content"], "保留草稿字");
            } else {
                let items = body["input"].as_array().unwrap();
                assert_eq!(items.len(), 2);
                response_user(&items[0], "cancel idle prompt");
                response_user(&items[1], "保留草稿字");
            }
            // 新 runtime/client 真正可用：下一次请求返回正常最终回答。
            raw.final_answer(protocol, "idle recovered answer");
            pty.text("idle recovered answer");
            pty.ready();
            pty.input("/quit\r");
            pty.finish();
            raw.assert_silent();
            let database = super::stats::database(home.path());
            let statuses: Vec<String> = database
                .prepare("SELECT status FROM requests ORDER BY id")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            assert_eq!(statuses, ["aborted", "ok"]);
            let contexts = super::stats::contexts(home.path());
            assert_eq!(contexts[0]["request"], first_body);
            assert!(contexts[1].is_null());
            let ttft: Option<f64> = database
                .query_row(
                    "SELECT ttft_ms FROM requests WHERE status='aborted'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(ttft.is_some(), phase == "sse");
        }
    }
}

// 验证项 2：两协议 write→bash→write 批次取消。父 shell 忽略 TERM 也能被回收，
// 保留的文件与已捕获输出不回滚、跳过的调用未执行，下一次请求中三个 tool
// result 与原 call_id 按序完整配对且不重复执行；取消本身不触发任何 HTTP。
#[test]
fn tui_cancel_tool_batch_stops_process_group_and_pairs_results_on_next_request() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        cancel_drive_batch(
            &mut pty,
            &fixture,
            protocol,
            "cancel batch prompt",
            project.path(),
        );
        pty.input("批后草稿");
        pty.draft("批后草稿");
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        cancel_assert_batch_stopped(project.path());
        // 取消收尾不触发任何模型请求。
        cancel_assert_no_request(&fixture);
        // 主动提交下一条任务（保留的草稿），验证配对与不重放。
        pty.input("\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        cancel_assert_paired_results(&body, protocol);
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["cancel batch prompt", "批后草稿"]
        );
        let text = serde_json::to_string(&body).unwrap();
        assert!(!text.contains("pp.cancelled"), "pp.cancelled 不得进入请求");
        if protocol == "completion" {
            final_reply(request, "batch recovered answer");
        } else {
            response_final(request, "batch recovered answer");
        }
        pty.text("batch recovered answer");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        cancel_assert_no_request(&fixture);
        // 旧工具从未被重放：文件状态保持取消后的样子。
        assert_eq!(
            std::fs::read_to_string(project.path().join("kept.txt")).unwrap(),
            "kept"
        );
        assert!(!project.path().join("skipped.txt").exists());
    }
}

// 验证项 4：headers 前、SSE 空闲、工具批次三种取消的持久化与重启。取消 Note
// 恰好一条、不误报 interrupted；/resume 列表为 aborted，完成新任务后可为 done；
// -r ID -p 与 -c ID 的原生请求只含已接受用户消息、完整旧历史和成对工具结果，
// 不执行旧工具，pp.cancelled 不进入请求。
#[test]
fn tui_cancel_terminal_state_persists_resumes_aborted_and_keeps_native_history_legal() {
    #[derive(Clone, Copy, PartialEq)]
    enum CancelPhase {
        Headers,
        Sse,
        Tools,
    }
    for protocol in ["completion", "response"] {
        for phase in [CancelPhase::Headers, CancelPhase::Sse, CancelPhase::Tools] {
            let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
            std::fs::write(
                config_path(home.path(), "models.yml"),
                response_models(&fixture, Some(protocol)),
            )
            .unwrap();
            let path = api_path(protocol);
            let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
            pty.ready();
            let mut pid_before = Vec::new();
            let mut child_before = Vec::new();
            match phase {
                CancelPhase::Headers => {
                    pty.input("persist cancel prompt\r");
                    // 请求已到达但未回应；持有 Request 防止对端提前 EOF。
                    let (_request, _) = pty.request_at(&fixture, path);
                    pty.input("\x1b");
                    cancel_wait_cancelled_ready(&mut pty);
                }
                CancelPhase::Sse => {
                    pty.input("persist cancel prompt\r");
                    let (request, _) = pty.request_at(&fixture, path);
                    let mut stream = SseReply::new(request);
                    streaming_delta(&mut stream, protocol, "persist-partial-token");
                    pty.text("persist-partial-token");
                    pty.input("\x1b");
                    cancel_wait_cancelled_ready(&mut pty);
                }
                CancelPhase::Tools => {
                    cancel_drive_batch(
                        &mut pty,
                        &fixture,
                        protocol,
                        "persist cancel prompt",
                        project.path(),
                    );
                    pty.input("\x1b");
                    cancel_wait_cancelled_ready(&mut pty);
                    cancel_assert_batch_stopped(project.path());
                    pid_before = std::fs::read(project.path().join("bash.pid")).unwrap();
                    child_before = std::fs::read(project.path().join("bash-child.pid")).unwrap();
                }
            }
            if phase == CancelPhase::Sse {
                assert_eq!(
                    cancel_count_normalized(pty.parser.screen(), "persist-partial-token"),
                    1
                );
            }
            pty.input("/quit\r");
            pty.finish();
            cancel_assert_no_request(&fixture);

            let id = saved_session_id(home.path());
            let journal = std::fs::read_to_string(
                home.path()
                    .join(".pp/agent/sessions")
                    .join(format!("{id}.jsonl")),
            )
            .unwrap();
            assert!(journal.contains("pp.cancelled"), "取消终态必须落盘");

            // 交互式 -r 恢复：Note 恰好一次，不出现假 interrupted 横幅。
            let mut resumed = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
            resumed.wait_for(
                "恢复取消会话：持久记录与唯一取消 Note",
                |screen| {
                    screen.contents().contains("persist cancel prompt")
                        && cancel_count_normalized(screen, CANCEL_NOTE) == 1
                },
            );
            assert!(
                !cancel_normalize(resumed.parser.screen())
                    .contains("Previousrequestwasinterrupted."),
                "主动取消不得误报为 interrupted"
            );
            assert_eq!(
                cancel_count_normalized(resumed.parser.screen(), "persist cancel prompt"),
                1
            );
            if phase == CancelPhase::Sse {
                assert_eq!(
                    cancel_count_normalized(resumed.parser.screen(), "persist-partial-token"),
                    1
                );
            }
            resumed.ready();
            if phase == CancelPhase::Tools {
                // 恢复不重放旧工具：PID marker 与文件保持不变。
                assert_eq!(
                    std::fs::read(project.path().join("bash.pid")).unwrap(),
                    pid_before
                );
                assert_eq!(
                    std::fs::read(project.path().join("bash-child.pid")).unwrap(),
                    child_before
                );
                assert_eq!(
                    std::fs::read_to_string(project.path().join("kept.txt")).unwrap(),
                    "kept"
                );
                assert!(!project.path().join("skipped.txt").exists());
            }
            cancel_assert_no_request(&fixture);

            // /resume 列表为 aborted 而不是 done。
            resume_boundary_open(&mut resumed);
            assert!(resumed.parser.screen().contents().contains("■ aborted"));
            assert!(!resumed.parser.screen().contents().contains("✓ done"));
            resume_boundary_close(&mut resumed);

            // 取消终态之后完成新任务，列表再次可为 done。
            resumed.input("persist recovery prompt\r");
            let (request, _) = resumed.request_at(&fixture, path);
            if protocol == "completion" {
                final_reply(request, "persist recovery answer");
            } else {
                response_final(request, "persist recovery answer");
            }
            resumed.text("persist recovery answer");
            resumed.ready();
            resume_boundary_open(&mut resumed);
            assert!(resumed.parser.screen().contents().contains("✓ done"));
            resume_boundary_close(&mut resumed);
            resumed.input("/quit\r");
            resumed.finish();
            cancel_assert_no_request(&fixture);

            // 行模式 -r ID -p：请求只含已接受用户消息与完整旧历史，无 pp.cancelled。
            let home_path = home.path().to_str().unwrap();
            let cli = Cli::spawn(
                project.path(),
                &["-r", id.as_str(), "-p", "persist cli follow"],
                &[("HOME", home_path)],
            );
            let (request, body) = receive_at(&fixture, path);
            assert_eq!(
                cancel_user_messages(&body, protocol),
                [
                    "persist cancel prompt",
                    "persist recovery prompt",
                    "persist cli follow"
                ]
            );
            assert_eq!(
                cancel_assistant_texts(&body, protocol),
                ["persist recovery answer"]
            );
            let text = serde_json::to_string(&body).unwrap();
            assert!(!text.contains("pp.cancelled"), "pp.cancelled 不得进入请求");
            assert!(
                !text.contains("persist-partial-token"),
                "半成品 assistant 不得进入原生历史"
            );
            if phase == CancelPhase::Tools {
                cancel_assert_paired_results(&body, protocol);
            } else {
                assert!(
                    cancel_tool_outputs(&body, protocol).is_empty(),
                    "非工具批次取消不得产生工具结果"
                );
            }
            if protocol == "completion" {
                final_reply(request, "persist cli answer");
            } else {
                response_final(request, "persist cli answer");
            }
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"persist cli answer\n");
            cancel_assert_no_request(&fixture);

            // 行模式 -c ID：发送保存的原生请求，历史仍合法、旧工具不执行。
            let cli = Cli::spawn(project.path(), &["-c", id.as_str()], &[("HOME", home_path)]);
            let (request, body) = receive_at(&fixture, path);
            assert_eq!(
                cancel_user_messages(&body, protocol),
                [
                    "persist cancel prompt",
                    "persist recovery prompt",
                    "persist cli follow"
                ]
            );
            assert_eq!(
                cancel_assistant_texts(&body, protocol),
                ["persist recovery answer", "persist cli answer"]
            );
            let text = serde_json::to_string(&body).unwrap();
            assert!(!text.contains("pp.cancelled"), "pp.cancelled 不得进入请求");
            assert!(!text.contains("persist-partial-token"));
            if phase == CancelPhase::Tools {
                cancel_assert_paired_results(&body, protocol);
            }
            if protocol == "completion" {
                final_reply(request, "persist continue answer");
            } else {
                response_final(request, "persist continue answer");
            }
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"persist continue answer\n");
            cancel_assert_no_request(&fixture);
            if phase == CancelPhase::Tools {
                assert_eq!(
                    std::fs::read_to_string(project.path().join("kept.txt")).unwrap(),
                    "kept"
                );
                assert!(!project.path().join("skipped.txt").exists());
            }
        }
    }
}

// 验证项 4 的持久化故障模式：取消收尾的 commit/sync 失败必须优先显示
// persisting session，不声称成功取消、不写 pp.cancelled、不再发模型请求，
// 同时取消路径仍要停止真实 bash 进程。
#[test]
fn tui_cancel_journal_failure_reports_persisting_session_not_success() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        // ulimit -f 256：header/用户记录写得下，流式的大内容让取消收尾的
        // native commit 以 EFBIG 失败。
        let mut pty = Pty::start_with_file_limit(project.path(), home.path(), 256, &[]);
        pty.ready();
        pty.input("overflow cancel batch\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let big = "A".repeat(400_000);
        let command =
            "printf 'overflow-ready\\n'; printf '%s' \"$$\" > overflow-bash.pid; sleep 30";
        if protocol == "completion" {
            reply(
                request,
                json!({
                    "role":"assistant", "content":big,
                    "tool_calls":[call("overflow-bash", "bash", json!({"command":command}))]
                }),
            );
        } else {
            response_reply(
                request,
                json!([
                    response_message("msg-overflow", json!([response_text(&big)])),
                    response_call("overflow-bash", "bash", json!({"command":command}))
                ]),
            );
        }
        // PID marker 是进入 bash 执行阶段的栅栏。
        cancel_wait_pid_files(project.path(), &["overflow-bash.pid"]);
        pty.input("\x1b");
        pty.wait_for(
            "取消收尾提交失败显示 persisting session 红色弹窗",
            |screen| {
                notification_bounds(screen, "Error", vt100::Color::Rgb(224, 108, 117)).is_some()
                    && cancel_normalize(screen).contains("persistingsession")
            },
        );
        // 不声称成功取消。
        assert_eq!(
            cancel_count_normalized(pty.parser.screen(), CANCEL_NOTE),
            0,
            "持久化失败不得显示取消 Note"
        );
        pty.ready();
        // 取消收尾失败后不再发模型请求。
        cancel_assert_no_request(&fixture);
        // 取消路径仍停止了真实 bash 进程。
        cancel_wait_pid_dead(&project.path().join("overflow-bash.pid"));
        pty.input("\x03");
        pty.finish();
        let id = saved_session_id(home.path());
        let journal = std::fs::read_to_string(
            home.path()
                .join(".pp/agent/sessions")
                .join(format!("{id}.jsonl")),
        )
        .unwrap();
        assert!(
            !journal.contains("pp.cancelled"),
            "取消终态提交失败时不得写 pp.cancelled"
        );
    }
}

// —— 后台任务 PTY 场景（计划 TUI 部分）：真实受管进程、PID 文件栅栏、
// 可见屏幕断言，不测静态标题文案。 ——

// 受管理的后台命令：父 shell 忽略 TERM，父子 PID 写入 marker 文件，
// 停止路径必须用 grace 后的 KILL 回收整个进程组。
fn background_hold_command(parent_marker: &str, child_marker: &str, log: Option<&str>) -> String {
    let mut command = String::from("trap '' TERM; ");
    if let Some(log) = log {
        command.push_str(&format!("printf '{log}\\n'; "));
    }
    command.push_str(&format!(
        "printf '%s' \"$$\" > {parent_marker}; sleep 30 & child=$!; printf '%s' \"$child\" > {child_marker}; wait \"$child\""
    ));
    command
}

// 按协议回复单个工具调用。
fn background_reply_call(request: Request, protocol: &str, id: &str, name: &str, args: Value) {
    if protocol == "completion" {
        calls_reply(request, json!([call(id, name, args)]));
    } else {
        response_reply(request, json!([response_call(id, name, args)]));
    }
}

// 断言请求中的工具结果与 call_id 恰好配对一次，返回其结果快照。
fn background_paired_snapshot(body: &Value, protocol: &str, call_id: &str) -> Value {
    let outputs = cancel_tool_outputs(body, protocol);
    assert_eq!(
        outputs
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        [call_id],
        "后台调用必须与原 call_id 恰好一次配对"
    );
    outputs[0].1.clone()
}

// 启动受管后台任务：PID marker 证明 bash 父子真正运行；启动后的下一次模型请求
// 携带启动结果并被持有（模型停在等待响应阶段）；屏幕必须显示 task_id 与 running，
// start 的完成不能被当作服务已退出。返回 (task_id, 启动快照, 持有的请求)。
fn background_start_and_wait(
    pty: &mut Pty,
    fixture: &Server,
    protocol: &str,
    project: &Path,
    start: &BackgroundStart<'_>,
) -> (String, Value, Request) {
    let [parent, child] = start.markers else {
        panic!("后台启动需要恰好两个 PID marker");
    };
    let command = background_hold_command(parent, child, start.log);
    pty.input(&format!("{}\r", start.prompt));
    let (request, _) = pty.request_at(fixture, api_path(protocol));
    background_reply_call(
        request,
        protocol,
        start.call_id,
        "bash",
        json!({"command": command, "mode": "background"}),
    );
    cancel_wait_pid_files(project, start.markers);
    let (request, body) = pty.request_at(fixture, api_path(protocol));
    let snapshot = background_paired_snapshot(&body, protocol, start.call_id);
    let task_id = snapshot["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("启动结果缺少 task_id：{snapshot}"))
        .to_owned();
    assert_eq!(snapshot["ok"], true);
    assert_eq!(snapshot["status"], "running");
    pty.wait_for("屏幕显示后台 task_id 与 running 状态", |screen| {
        cancel_normalize(screen).contains(&format!("Task{task_id}:running"))
    });
    (task_id, snapshot, request)
}

// 一次后台启动的输入：marker 列表同时用于生成命令与进程级栅栏。
struct BackgroundStart<'a> {
    prompt: &'a str,
    call_id: &'a str,
    markers: &'a [&'a str],
    log: Option<&'a str>,
}

// 两协议先启动后台任务并把下一模型请求停在等待响应阶段。Esc 后必须先清理
// 父子 PID 与 scratch，再回到 Ready 且恰好一条取消 Note；下一用户任务正常执行，
// 不重放旧启动调用。
#[test]
fn background_esc_cancels_managed_tasks_and_cleans_before_ready_without_replay() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let path = api_path(protocol);
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        let start = BackgroundStart {
            prompt: "background esc prompt",
            call_id: "bg-esc-start",
            markers: &["bg-esc-parent.pid", "bg-esc-child.pid"],
            log: None,
        };
        let (task_id, _snapshot, held) =
            background_start_and_wait(&mut pty, &fixture, protocol, project.path(), &start);
        let parent_before = std::fs::read(project.path().join("bg-esc-parent.pid")).unwrap();
        let child_before = std::fs::read(project.path().join("bg-esc-child.pid")).unwrap();
        pty.input("\x1b");
        // 先验证后台父子 PID 与 scratch 已清理，再观察 Ready 与唯一取消 Note。
        cancel_wait_pid_dead(&project.path().join("bg-esc-parent.pid"));
        cancel_wait_pid_dead(&project.path().join("bg-esc-child.pid"));
        cancel_assert_no_scratch(project.path());
        cancel_wait_cancelled_ready(&mut pty);
        drop(held);
        // 取消收尾不触发任何模型请求。
        cancel_assert_no_request(&fixture);
        // 下一用户任务正常执行，且旧启动调用不重放。
        pty.input("background esc next\r");
        let (request, body) = pty.request_at(&fixture, path);
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["background esc prompt", "background esc next"]
        );
        assert_eq!(cancel_called_ids(&body, protocol), ["bg-esc-start"]);
        let replayed = background_paired_snapshot(&body, protocol, "bg-esc-start");
        assert_eq!(replayed["task_id"], task_id.as_str());
        let text = serde_json::to_string(&body).unwrap();
        assert!(!text.contains("pp.cancelled"), "pp.cancelled 不得进入请求");
        if protocol == "completion" {
            final_reply(request, "background esc recovered");
        } else {
            response_final(request, "background esc recovered");
        }
        pty.text("background esc recovered");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        cancel_assert_no_request(&fixture);
        // 旧工具从未被重放：PID marker 保持取消后的字节，进程保持停止，无新 scratch。
        assert_eq!(
            std::fs::read(project.path().join("bg-esc-parent.pid")).unwrap(),
            parent_before
        );
        assert_eq!(
            std::fs::read(project.path().join("bg-esc-child.pid")).unwrap(),
            child_before
        );
        cancel_assert_no_scratch(project.path());
    }
}

// 后台任务存活且模型请求挂起（request_timeout_seconds=120）时按 Ctrl-C：PTY 必须
// 完成退出并恢复终端（finish 的 5 秒截止即证据，不等待整个 detached agent
// worker/HTTP），父子 PID 与 scratch 全部清理。
#[test]
fn background_ctrl_c_exits_and_cleans_tasks_while_model_request_is_pending() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        let start = BackgroundStart {
            prompt: "background ctrl-c prompt",
            call_id: "bg-cc-start",
            markers: &["bg-cc-parent.pid", "bg-cc-child.pid"],
            log: None,
        };
        let (_task_id, _snapshot, held) =
            background_start_and_wait(&mut pty, &fixture, protocol, project.path(), &start);
        pty.input("\x03");
        // 不杀掉 CLI 本身：退出由 Ctrl-C 键触发，finish 校验退出码、终端模式恢复。
        pty.finish();
        drop(held);
        cancel_wait_pid_dead(&project.path().join("bg-cc-parent.pid"));
        cancel_wait_pid_dead(&project.path().join("bg-cc-child.pid"));
        cancel_assert_no_scratch(project.path());
        cancel_assert_no_request(&fixture);
    }
}

// 同一后台生命周期的 status/stop 展示真实状态与累计日志；停止是正常路径，
// 不把 exit_code null 冒充为 Exit code 显示；前台卡片保留现有 Exit code/Wall。
#[test]
fn background_status_stop_lifecycle_displays_states_and_keeps_foreground_footer() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let path = api_path(protocol);
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        let start = BackgroundStart {
            prompt: "background lifecycle prompt",
            call_id: "bg-life-start",
            markers: &["bg-life-parent.pid", "bg-life-child.pid"],
            log: Some("bg-lifecycle-log"),
        };
        let (task_id, _snapshot, request) =
            background_start_and_wait(&mut pty, &fixture, protocol, project.path(), &start);
        // status：卡片显示调用（action · task_id）、running 状态与累计日志。
        background_reply_call(
            request,
            protocol,
            "bg-life-status",
            "bash_task",
            json!({"task_id": task_id.as_str(), "action": "status"}),
        );
        let (request, _) = pty.request_at(&fixture, path);
        pty.wait_for(
            "status 卡片显示任务 ID、running 与累计日志",
            |screen| {
                let text = cancel_normalize(screen);
                text.contains(&format!("bash_task·status·{task_id}"))
                    && text.contains("bg-lifecycle-log")
                    && text.contains(&format!("Task{task_id}:running"))
            },
        );
        // stop：同步等待进程/scratch 回收后返回，卡片显示真实 stopped 终态。
        background_reply_call(
            request,
            protocol,
            "bg-life-stop",
            "bash_task",
            json!({"task_id": task_id.as_str(), "action": "stop"}),
        );
        let (request, _) = pty.request_at(&fixture, path);
        cancel_wait_pid_dead(&project.path().join("bg-life-parent.pid"));
        cancel_wait_pid_dead(&project.path().join("bg-life-child.pid"));
        cancel_assert_no_scratch(project.path());
        pty.wait_for("stop 卡片显示 stopped 终态与调用标题", |screen| {
            let text = cancel_normalize(screen);
            text.contains(&format!("Task{task_id}:stopped"))
                && text.contains(&format!("bash_task·stop·{task_id}"))
        });
        // 信号停止是 stop 的预期结果：不得显示 Exit code: null 冒充已结束。
        assert!(
            !cancel_normalize(pty.parser.screen()).contains("Exitcode:null"),
            "stop 卡片不得显示 Exit code: null"
        );
        // 前台卡片仍保持现有 Exit code/Wall footer。
        background_reply_call(
            request,
            protocol,
            "bg-life-foreground",
            "bash",
            json!({"command": "printf 'fg-visible-marker'", "mode": "foreground"}),
        );
        let (request, _) = pty.request_at(&fixture, path);
        pty.wait_for("前台卡片保留 Exit code 与 Wall footer", |screen| {
            screen.contents().lines().any(|line| {
                let line = line.split_whitespace().collect::<String>();
                line.contains("Wall:") && line.contains("Exitcode:0")
            }) && cancel_normalize(screen).contains("fg-visible-marker")
        });
        if protocol == "completion" {
            final_reply(request, "background lifecycle answer");
        } else {
            response_final(request, "background lifecycle answer");
        }
        pty.text("background lifecycle answer");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        cancel_assert_no_request(&fixture);
    }
}

#[test]
fn request_retry_tui_notifies_and_cancels_backoff_before_recovering() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready();
        pty.input("retry cancelled prompt\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        request
            .respond(
                Response::from_string("private-error-body fixture-key")
                    .with_status_code(503)
                    .with_header(Header::from_bytes("Retry-After", "30").unwrap()),
            )
            .unwrap();
        pty.wait_for("重试通知展示等待时长与 HTTP 状态", |screen| {
            let text = screen.contents();
            text.contains("Retrying in 30s") && text.contains("HTTP status 503")
        });
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        cancel_assert_no_request(&fixture);
        assert!(!pty.parser.screen().contents().contains("fixture-key"));
        pty.input("after retry cancellation\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["retry cancelled prompt", "after retry cancellation"]
        );
        plan_final(request, protocol, "retry-recovery-complete");
        pty.text("retry-recovery-complete");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        no_more(&fixture);
    }
}
