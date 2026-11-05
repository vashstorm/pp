use super::*;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    net::TcpStream,
    time::Instant,
};

fn open(endpoint: &MockEndpoint, method: &str, path: &str, body: &str) -> TcpStream {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint.port())).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    write!(stream, "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    stream.flush().unwrap();
    stream
}

fn collect(stream: TcpStream) -> (u16, Vec<(Instant, Value)>, String) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
    }
    let mut frames = Vec::new();
    let mut body = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if let Some(data) = line.trim_end().strip_prefix("data: ") {
            serde_json::from_str::<async_openai::types::responses::ResponseStreamEvent>(data)
                .unwrap_or_else(|error| panic!("invalid Responses event: {error}: {data}"));
            frames.push((Instant::now(), serde_json::from_str(data).unwrap()));
        }
        body.push_str(&line);
    }
    (status, frames, body)
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

    let slow = open(
        &endpoint,
        "POST",
        path,
        r#"[{"content":["first","later"],"tokens_per_second":0.1},{"content":"next"}]"#,
    );
    let mut reader = BufReader::new(slow);
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line.starts_with("data: ") && line.contains("first") {
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
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
    }
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
    let stream = open(
        &endpoint,
        "POST",
        path,
        r#"[{"content":["first","tail"],"tokens_per_second":1},{"content":"next"}]"#,
    );
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line.starts_with("data: ") && line.contains("\"delta\":\"first\"") {
            break;
        }
    }
    assert_eq!(post(&endpoint, path, "ignored busy body").0, 409);
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
    }
    assert_eq!(text(&post(&endpoint, path, "not JSON").1), "next");
    assert_eq!(post(&endpoint, path, "{}").0, 400);
}
