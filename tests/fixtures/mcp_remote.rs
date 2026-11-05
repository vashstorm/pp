// 回环 MCP 服务；只使用测试凭证，连接和处理线程均由夹具统一回收。
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

pub struct RemoteServer {
    pub url: String,
    state: Arc<State>,
    accept: Option<thread::JoinHandle<()>>,
}
struct State {
    mode: String,
    stopped: AtomicBool,
    changed: AtomicBool,
    calls: AtomicUsize,
    log: Mutex<Vec<Value>>,
    sockets: Mutex<Vec<TcpStream>>,
    stream: Mutex<Option<TcpStream>>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
}
impl RemoteServer {
    pub fn new(mode: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(State {
            mode: mode.into(),
            stopped: AtomicBool::new(false),
            changed: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
            log: Mutex::new(Vec::new()),
            sockets: Mutex::new(Vec::new()),
            stream: Mutex::new(None),
            workers: Mutex::new(Vec::new()),
        });
        let shared = state.clone();
        let accept = thread::spawn(move || {
            while !shared.stopped.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        // macOS 的 accept 会继承监听器的非阻塞标志；处理线程使用阻塞读取。
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(5)))
                            .unwrap();
                        socket
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        shared
                            .sockets
                            .lock()
                            .unwrap()
                            .push(socket.try_clone().unwrap());
                        let state = shared.clone();
                        let worker = thread::spawn(move || {
                            let _ = serve(socket, &state);
                        });
                        shared.workers.lock().unwrap().push(worker);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        });
        Self {
            url,
            state,
            accept: Some(accept),
        }
    }
    pub fn config(&self, transport: &str, timeout: u64) -> Value {
        json!({"mcpServers":{"remote":{
            "type":transport,"url":format!("{}/{}",self.url,if transport == "sse" {"sse"} else {"mcp"}),
            "headers":{"X-Test-Key":"literal-secret"},"timeout":timeout
        }}})
    }
    pub fn log(&self) -> Vec<Value> {
        self.state.log.lock().unwrap().clone()
    }
    pub fn changed(&self) {
        self.state.changed.store(true, Ordering::SeqCst);
    }
    pub fn wait_calls(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.state.calls.load(Ordering::SeqCst) < count {
            assert!(Instant::now() < deadline, "未收到远程工具调用");
            thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for RemoteServer {
    fn drop(&mut self) {
        self.state.stopped.store(true, Ordering::SeqCst);
        self.accept.take().unwrap().join().unwrap();
        for socket in self.state.sockets.lock().unwrap().iter() {
            let _ = socket.shutdown(Shutdown::Both);
        }
        self.state.stream.lock().unwrap().take();
        for worker in self.state.workers.lock().unwrap().drain(..) {
            worker.join().unwrap();
        }
    }
}

fn serve(mut socket: TcpStream, state: &State) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        let mut chunk = [0; 4096];
        let read = match socket.read(&mut chunk) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&chunk[..read]);
        assert!(bytes.len() < 1_048_576);
    };
    let header = String::from_utf8(bytes[..header_end].to_vec()).unwrap();
    let mut lines = header.lines();
    let mut first = lines.next().unwrap().split_whitespace();
    let method = first.next().unwrap().to_owned();
    let path = first.next().unwrap().to_owned();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .map(|s| s.parse().unwrap())
        .unwrap_or(0);
    while bytes.len() < header_end + length {
        let mut chunk = [0; 8192];
        let read = match socket.read(&mut chunk) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if read == 0 {
            return Ok(());
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    let body: Value = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap()
    };
    state
        .log
        .lock()
        .unwrap()
        .push(json!({"method":method,"path":path,"headers":headers,"body":body}));
    if method == "DELETE" {
        return reply(&mut socket, 405, "text/plain", b"", "");
    }
    if method == "GET" {
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: heartbeat\r\n\r\n")?;
        let endpoint = if state.mode == "cross_origin" {
            "http://localhost:1/secret".to_owned()
        } else if state.mode == "absolute_endpoint" {
            format!("http://{}/messages?session=secret-session", headers["host"])
        } else {
            "/messages?session=secret-session".to_owned()
        };
        write!(socket, "event: endpoint\r\ndata: {endpoint}\r\n\r\n")?;
        *state.stream.lock().unwrap() = Some(socket.try_clone()?);
        while !state.stopped.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(10));
            socket.write_all(b": heartbeat\n\n")?;
        }
        return Ok(());
    }
    if body.get("method").is_none()
        || body["method"] == "notifications/initialized"
        || body["method"] == "notifications/cancelled"
    {
        return reply(&mut socket, 202, "text/plain", b"", "");
    }
    let calling = body["method"] == "tools/call";
    if calling {
        state.calls.fetch_add(1, Ordering::SeqCst);
    }
    if state.mode == "stall_headers" && body["method"] == "initialize" {
        while !state.stopped.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(5));
        }
        return Ok(());
    }
    let legacy = path.starts_with("/messages");
    if calling {
        match state.mode.as_str() {
            "status401" => return reply(&mut socket, 401, "text/plain", b"secret-auth-error", ""),
            "status404" => {
                return reply(&mut socket, 404, "text/plain", b"secret-session-error", "");
            }
            "status500" => {
                return reply(&mut socket, 500, "text/plain", b"secret-server-error", "");
            }
            "redirect" => {
                return reply(
                    &mut socket,
                    307,
                    "text/plain",
                    b"secret-redirect",
                    "Location: http://127.0.0.1:1/secret\r\n",
                );
            }
            "disconnect" => return Ok(()),
            "bad_json" => {
                return reply(
                    &mut socket,
                    200,
                    "application/json",
                    b"secret-invalid-json",
                    "",
                );
            }
            "oversized" => {
                return reply(
                    &mut socket,
                    200,
                    "application/json",
                    &vec![b'x'; 1_048_576],
                    "",
                );
            }
            "stall_call" | "stall_once"
                if state.mode == "stall_call" || state.calls.load(Ordering::SeqCst) == 1 =>
            {
                if legacy {
                    return reply(&mut socket, 202, "text/plain", b"", "");
                }
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")?;
                while !state.stopped.load(Ordering::SeqCst) {
                    socket.write_all(b": heartbeat\n\n")?;
                    thread::sleep(Duration::from_millis(10));
                }
                return Ok(());
            }
            _ => {}
        }
    }
    let result = match body["method"].as_str().unwrap() {
        "initialize" => {
            json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"loopback","version":"1"}})
        }
        "tools/list" if body["params"].get("cursor").is_none() => json!({"tools":[{
            "name":"echo","description":if state.changed.load(Ordering::SeqCst) {"changed"} else {"远程测试工具"},
            "inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}
        }],"nextCursor":"second"}),
        "tools/list" => {
            json!({"tools":[{"name":"fail","inputSchema":{"type":"object","properties":{}}}]})
        }
        "tools/call" => {
            json!({"content":[{"type":"text","text":body["params"]["arguments"]["text"]}],"structuredContent":{"count":1},"isError":body["params"]["name"] == "fail"})
        }
        _ => panic!("未知测试请求"),
    };
    let id = if state.mode == "wrong_id" && calling {
        json!("wrong")
    } else {
        body["id"].clone()
    };
    let message = if state.mode == "rpc_error" && calling {
        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32603,"message":"secret-rpc-error"}})
    } else {
        json!({"jsonrpc":"2.0","id":id,"result":result})
    };
    let mut events = String::new();
    if calling {
        events.push_str(&format!(
            "event: message\ndata: {}\n\n",
            json!({"jsonrpc":"2.0","id":"server-ping","method":"ping"})
        ));
        events.push_str(&format!(
            "data: {}\n\n",
            json!({"jsonrpc":"2.0","id":"unsupported","method":"sampling/createMessage"})
        ));
    }
    events.push_str(&format!("event: message\ndata: {message}\n\n"));
    if legacy {
        if let Some(stream) = state.stream.lock().unwrap().as_mut() {
            stream.write_all(events.as_bytes())?;
        }
        reply(&mut socket, 202, "text/plain", b"", "")
    } else {
        let extra = if body["method"] == "initialize" && state.mode != "no_session" {
            "MCP-Session-Id: secret-session\r\n"
        } else {
            ""
        };
        if state.mode == "sse" || state.mode == "wrong_id" {
            reply(
                &mut socket,
                200,
                "Text/Event-Stream; Charset=UTF-8",
                events.as_bytes(),
                extra,
            )
        } else {
            reply(
                &mut socket,
                200,
                "application/json",
                message.to_string().as_bytes(),
                extra,
            )
        }
    }
}
fn reply(
    socket: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra: &str,
) -> std::io::Result<()> {
    write!(
        socket,
        "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    )?;
    socket.write_all(body)
}
