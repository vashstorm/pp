use super::*;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    time::{Duration, Instant},
};

/// 回环响应的增量读取器：先解开 HTTP chunked framing，再按空行切出 SSE 事件。
struct Reply {
    status: u16,
    chunked: bool,
    reader: BufReader<TcpStream>,
    body: Vec<u8>,
    finished: bool,
}

impl Reply {
    fn open(stream: TcpStream) -> std::io::Result<Self> {
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        Self::from_reader(BufReader::new(stream))
    }

    fn from_reader(mut reader: BufReader<TcpStream>) -> std::io::Result<Self> {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let status = line
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .ok_or_else(|| std::io::Error::other("invalid HTTP status line"))?;
        let mut chunked = false;
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 || line == "\r\n" {
                break;
            }
            let header = line.to_ascii_lowercase();
            if header.starts_with("transfer-encoding:") && header.contains("chunked") {
                chunked = true;
            }
        }
        Ok(Self {
            status,
            chunked,
            reader,
            body: Vec::new(),
            finished: false,
        })
    }

    fn set_read_timeout(&self, timeout: Duration) -> std::io::Result<()> {
        self.reader.get_ref().set_read_timeout(Some(timeout))
    }

    /// 解码并追加更多 body 字节；`false` 表示已到 EOF。
    fn fill(&mut self) -> std::io::Result<bool> {
        if self.finished {
            return Ok(false);
        }
        if !self.chunked {
            let mut rest = Vec::new();
            self.reader.read_to_end(&mut rest)?;
            self.body.extend_from_slice(&rest);
            self.finished = true;
            return Ok(!rest.is_empty());
        }
        let mut line = String::new();
        if self.reader.read_line(&mut line)? == 0 {
            self.finished = true;
            return Ok(false);
        }
        let size = usize::from_str_radix(line.trim_end(), 16)
            .map_err(|_| std::io::Error::other("invalid chunk size"))?;
        if size == 0 {
            line.clear();
            let _ = self.reader.read_line(&mut line)?;
            self.finished = true;
            return Ok(false);
        }
        let mut chunk = vec![0; size];
        self.reader.read_exact(&mut chunk)?;
        self.body.extend_from_slice(&chunk);
        let mut terminator = [0; 2];
        self.reader.read_exact(&mut terminator)?;
        if terminator != *b"\r\n" {
            return Err(std::io::Error::other("invalid chunk terminator"));
        }
        Ok(true)
    }

    /// 下一个完整 SSE 事件（不含结束空行）；EOF 返回 `None`。
    fn event(&mut self) -> std::io::Result<Option<String>> {
        loop {
            if let Some(index) = self.body.windows(2).position(|window| window == b"\n\n") {
                let event = String::from_utf8(self.body[..index].to_vec())
                    .map_err(|_| std::io::Error::other("SSE event is not UTF-8"))?;
                self.body.drain(..index + 2);
                return Ok(Some(event));
            }
            if !self.fill()? {
                return Ok(None);
            }
        }
    }

    /// 解码剩余字节，供错误响应正文断言使用。
    fn text(&mut self) -> std::io::Result<String> {
        while self.fill()? {}
        let text = String::from_utf8_lossy(&self.body).into_owned();
        self.body.clear();
        Ok(text)
    }
}

fn open(endpoint: &MockEndpoint, method: &str, path: &str, body: &str) -> Reply {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port())).unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    stream.flush().unwrap();
    Reply::open(stream).unwrap()
}

fn collect(mut reply: Reply) -> (u16, Vec<(Instant, Value)>, String) {
    let status = reply.status;
    let mut frames = Vec::new();
    while let Some(event) = reply.event().unwrap() {
        frames.push((Instant::now(), data(&event)));
    }
    let body = reply.text().unwrap();
    (status, frames, body)
}

/// 取出 SSE 事件的 `data` 负载，先按协议校验再解析。
fn data(event: &str) -> Value {
    let raw = event
        .lines()
        .find_map(|line| line.strip_prefix("data: "))
        .unwrap_or_else(|| panic!("SSE 事件必须带 data 行: {event}"));
    serde_json::from_str::<async_openai::types::responses::ResponseStreamEvent>(raw)
        .unwrap_or_else(|error| panic!("invalid Responses event: {error}: {raw}"));
    serde_json::from_str(raw).unwrap()
}

fn post(endpoint: &MockEndpoint, path: &str, body: &str) -> (u16, Vec<(Instant, Value)>, String) {
    collect(open(endpoint, "POST", path, body))
}

fn text(frames: &[(Instant, Value)]) -> String {
    frames
        .iter()
        .filter(|(_, v)| v["type"] == "response.output_text.delta")
        .map(|(_, v)| v["delta"].as_str().unwrap())
        .collect()
}

#[test]
fn mock_ai_fifo_reload_and_same_response_continuation() {
    let server = MockServer::start().unwrap();
    let endpoint = server.endpoint();
    let path = "/mock/fifo/root/v1/responses";
    let script = json!([
        {"tool":{"name":"read","arguments":{"path":"file.txt"}}},
        {"content":"second"}
    ])
    .to_string();
    let (status, first, _) = post(&endpoint, path, &script);
    assert_eq!(status, 200);
    let done = &first
        .iter()
        .find(|(_, v)| v["type"] == "response.completed")
        .unwrap()
        .1;
    assert_eq!(done["response"]["output"][0]["name"], "read");
    assert_eq!(done["response"]["model"], "pp-mock-root");
    let (status, second, _) = post(&endpoint, path, "not JSON at all");
    assert_eq!(status, 200);
    assert_eq!(text(&second), "second");
    let (status, _, body) = post(&endpoint, path, "{}");
    assert_eq!(status, 400);
    assert!(body.contains("mock_script_required"));
    assert_eq!(
        text(&post(&endpoint, path, r#"[{"response":"new-batch"}]"#).1),
        "new-batch"
    );

    let script = json!([
        {"content":"A","auto_continue":true},
        {"response":{"echo":true}},
        {"content":"B"}
    ])
    .to_string();
    let (_, frames, _) = post(&endpoint, path, &script);
    assert_eq!(text(&frames), "A{\"echo\":true}");
    assert_eq!(
        frames
            .iter()
            .filter(|(_, v)| v["type"] == "response.completed")
            .count(),
        1
    );
    assert_eq!(
        text(
            &post(
                &endpoint,
                path,
                r#"{"mock_script":[{"content":"ignored"}],"model":"wrong"}"#
            )
            .1
        ),
        "B"
    );
}

#[test]
fn mock_ai_routes_invalid_scripts_do_not_advance() {
    let server = MockServer::start().unwrap();
    let endpoint = server.endpoint();
    let path = "/v1/responses";
    assert_eq!(post(&endpoint, "/v1/chat/completions", "[]").0, 404);
    assert_eq!(post(&endpoint, "/mock/../root/v1/responses", "[]").0, 404);
    assert_eq!(collect(open(&endpoint, "GET", path, "")).0, 405);
    for script in [
        "[]",
        r#"[{"think":"alone"}]"#,
        r#"[{"content":"a","tokens_per_second":-1}]"#,
    ] {
        assert_eq!(post(&endpoint, path, script).0, 400);
    }
    assert_eq!(
        text(&post(&endpoint, path, r#"[{"content":"one"},{"content":"two"}]"#).1),
        "one"
    );
    assert_eq!(collect(open(&endpoint, "GET", path, "")).0, 405);
    assert_eq!(text(&post(&endpoint, path, "[]").1), "two");
}

#[test]
fn mock_ai_routes_match_the_raw_request_target() {
    let server = MockServer::start().unwrap();
    let endpoint = server.endpoint();
    // query、百分号编码与尾斜杠都不被正规化成合法路由。
    for path in [
        "/mock/run/root/v1/responses?stream=true",
        "/mock/run%2Froot/v1/responses",
        "/mock/run/root/v1/responses/",
        "/v1/responses?x=1",
    ] {
        assert_eq!(post(&endpoint, path, "[]").0, 404, "{path}");
    }
    // 路由先于方法：合法路径的非 POST 是空 405，未知路径任何方法都是 404。
    assert_eq!(collect(open(&endpoint, "GET", "/v1/responses", "")).0, 405);
    assert_eq!(
        collect(open(&endpoint, "GET", "/mock/run/root/v1/responses", "")).0,
        405
    );
    assert_eq!(collect(open(&endpoint, "GET", "/missing", "")).0, 404);
    assert!(endpoint.assert_exhausted("run").is_err());
}

#[test]
fn mock_ai_accepts_request_bodies_above_two_mebibytes() {
    let server = MockServer::start().unwrap();
    let endpoint = server.endpoint();
    let body = json!({
        "model": "unused",
        "input": "x".repeat(2 * 1024 * 1024 + 7),
        "mock_script": [{"content": "large"}]
    })
    .to_string();
    let (status, frames, _) = post(&endpoint, "/mock/large/root/v1/responses", &body);
    assert_eq!(status, 200);
    // 只回显脚本结果，不把请求正文拼进响应。
    assert_eq!(text(&frames), "large");
    let output = frames.last().unwrap().1["response"]["output"]
        .as_array()
        .unwrap();
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["content"][0]["text"], "large");
    endpoint.assert_exhausted("large").unwrap();
}

#[test]
fn mock_ai_throttle_busy_lanes_and_cancellation() {
    let server = MockServer::start().unwrap();
    let endpoint = server.endpoint();
    let path = "/mock/rate/root/v1/responses";
    let (_, frames, _) = post(
        &endpoint,
        path,
        r#"[{"content":["a","b","c","d"],"tokens_per_second":20}]"#,
    );
    let tokens: Vec<_> = frames
        .iter()
        .filter(|(_, v)| v["type"] == "response.output_text.delta")
        .collect();
    assert_eq!(text(&frames), "abcd");
    assert!(tokens.last().unwrap().0.duration_since(tokens[0].0) >= Duration::from_millis(135));

    let mut slow = open(
        &endpoint,
        "POST",
        path,
        r#"[{"content":["first","later"],"tokens_per_second":0.1},{"content":"next"}]"#,
    );
    loop {
        let event = slow.event().unwrap().expect("慢流必须发送首个 token");
        if data(&event)["delta"] == "first" {
            break;
        }
    }
    assert_eq!(post(&endpoint, path, "arbitrary").0, 409);
    let started = Instant::now();
    assert_eq!(
        text(
            &post(
                &endpoint,
                "/mock/rate/fast/v1/responses",
                r#"[{"content":"fast"}]"#
            )
            .1
        ),
        "fast"
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    endpoint.cancel_run("rate");
    while slow.event().unwrap().is_some() {}
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        text(&post(&endpoint, path, r#"[{"content":"fresh"}]"#).1),
        "fresh"
    );
}

#[test]
fn mock_ai_unicode_items_usage_and_tool_group_boundaries() {
    let server = MockServer::start().unwrap();
    let endpoint = server.endpoint();
    let path = "/mock/items/review/v1/responses";
    let script = json!([
        {"think":["想","🦀"],"auto_continue":true},
        {"content":["正","文"],"response":{"echo":true},
         "tool":{"name":"write","arguments":{"path":"x","content":"你好"}},
         "usage":{"input_tokens":120,"output_tokens":30,"cached_tokens":40,"cache_write_tokens":5}}
    ])
    .to_string();
    let (_, frames, _) = post(&endpoint, path, &script);
    let think: String = frames
        .iter()
        .filter(|(_, v)| v["type"] == "response.reasoning_summary_text.delta")
        .map(|(_, v)| v["delta"].as_str().unwrap())
        .collect();
    assert_eq!(think, "想🦀");
    assert_eq!(text(&frames), "正文{\"echo\":true}");
    let done = &frames.last().unwrap().1["response"];
    assert_eq!(done["usage"]["input_tokens"], 120);
    assert_eq!(done["usage"]["output_tokens"], 30);
    assert_eq!(done["usage"]["total_tokens"], 150);
    assert_eq!(done["usage"]["input_tokens_details"]["cached_tokens"], 40);
    assert_eq!(
        done["usage"]["input_tokens_details"]["cache_write_tokens"],
        5
    );
    let output = done["output"].as_array().unwrap();
    assert_eq!(output[0]["type"], "reasoning");
    assert_eq!(output.last().unwrap()["type"], "function_call");
    let completed_text: String = output
        .iter()
        .filter_map(|item| item["content"].as_array())
        .flatten()
        .filter_map(|part| part["text"].as_str())
        .collect();
    assert_eq!(completed_text, text(&frames));
    assert_eq!(
        output.last().unwrap()["arguments"],
        r#"{"content":"你好","path":"x"}"#
    );
    for auto in [false, true] {
        let script = json!([
            {"tool":{"name":"read","arguments":{"path":"one"}},"auto_continue":auto},
            {"tool":{"name":"read","arguments":{"path":"two"}}},
            {"content":"finished"}
        ])
        .to_string();
        let (_, first, _) = post(&endpoint, path, &script);
        let output = first.last().unwrap().1["response"]["output"]
            .as_array()
            .unwrap();
        assert_eq!(output.len(), if auto { 2 } else { 1 });
        if auto {
            assert_ne!(output[0]["call_id"], output[1]["call_id"]);
        } else {
            let (_, next, _) = post(&endpoint, path, "{}");
            assert_eq!(
                next.last().unwrap().1["response"]["output"][0]["arguments"],
                r#"{"path":"two"}"#
            );
        }
        assert_eq!(text(&post(&endpoint, path, "{}").1), "finished");
    }
}

#[test]
fn mock_ai_busy_request_does_not_consume_the_next_group() {
    let server = MockServer::start().unwrap();
    let endpoint = server.endpoint();
    let path = "/mock/busy/root/v1/responses";
    let mut stream = open(
        &endpoint,
        "POST",
        path,
        r#"[{"content":["first","tail"],"tokens_per_second":1},{"content":"next"}]"#,
    );
    loop {
        let event = stream.event().unwrap().expect("首个 delta 必须到达");
        if data(&event)["delta"] == "first" {
            break;
        }
    }
    assert_eq!(post(&endpoint, path, "ignored busy body").0, 409);
    while stream.event().unwrap().is_some() {}
    assert_eq!(text(&post(&endpoint, path, "not JSON").1), "next");
    assert_eq!(post(&endpoint, path, "{}").0, 400);
}

#[test]
fn mock_ai_busy_queue_rejects_before_reading_the_script_body() {
    let server = MockServer::start().unwrap();
    let endpoint = server.endpoint();
    let path = "/mock/window/root/v1/responses";
    let script = json!([{"content": "first"}]).to_string();
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    // 首请求只发 headers：handler 已在 busy 内等待正文。
    write!(
        stream,
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",
        script.len()
    )
    .unwrap();
    stream.flush().unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line.trim_end(), "HTTP/1.1 100 Continue");
    line.clear();
    reader.read_line(&mut line).unwrap();
    assert_eq!(line, "\r\n");

    // 同一 lane 的上传窗口内立即 409，其他 lane 不受影响。
    assert_eq!(post(&endpoint, path, "[]").0, 409);
    assert_eq!(
        text(
            &post(
                &endpoint,
                "/mock/window/other/v1/responses",
                r#"[{"content":"second"}]"#
            )
            .1
        ),
        "second"
    );

    reader.get_mut().write_all(script.as_bytes()).unwrap();
    reader.get_mut().flush().unwrap();
    let (status, frames, _) = collect(Reply::from_reader(reader).unwrap());
    assert_eq!(status, 200);
    assert_eq!(text(&frames), "first");
    endpoint.assert_exhausted("window").unwrap();
}

#[test]
fn mock_ai_owner_drop_stops_a_streaming_response() {
    let server = MockServer::start().unwrap();
    let endpoint = server.endpoint();
    let mut reply = open(
        &endpoint,
        "POST",
        "/mock/drop/root/v1/responses",
        r#"[{"content":["first","tail"],"tokens_per_second":0.2}]"#,
    );
    let mut seen = String::new();
    loop {
        let event = reply.event().unwrap().expect("首个 delta 必须到达");
        seen.push_str(&event);
        if data(&event)["type"] == "response.output_text.delta" {
            break;
        }
    }
    reply.set_read_timeout(Duration::from_secs(2)).unwrap();
    drop(server);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut completed = false;
    loop {
        match reply.event() {
            Ok(Some(event)) => completed |= data(&event)["type"] == "response.completed",
            Ok(None) => break,
            Err(error) => panic!("连接未在 2s 内关闭: {error}"),
        }
    }
    assert!(Instant::now() <= deadline);
    assert!(!completed, "owner drop must not emit response.completed");
    assert!(seen.contains("response.output_text.delta"));
}
