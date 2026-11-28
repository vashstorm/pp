use super::*;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
};

#[test]
fn rate_limited_opening_retries_then_finishes_on_either_protocol() {
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
            Some("retry once"),
            "{\n\"default_model\": \"fixture/primary\",\n\"rate_limit_retry_budget_seconds\": 30,\n}\n",
            &models,
        );
        let (first, first_body) = if responses {
            receive_at(&server, "/v1/responses")
        } else {
            receive(&server)
        };
        first
            .respond(
                Response::builder()
                    .status(429)
                    .header("Retry-After", "0")
                    .body(Body::from("private-error-body fixture-key"))
                    .unwrap(),
            )
            .unwrap();
        let (second, second_body) = if responses {
            receive_at(&server, "/v1/responses")
        } else {
            receive(&server)
        };
        // The retried request carries the same credentials.
        assert_eq!(authorization(&second), "Bearer fixture-key");
        assert_eq!(first_body, second_body);
        if responses {
            response_final(second, "recovered");
        } else {
            final_reply(second, "recovered");
        }
        let output = cli.finish();
        assert!(output.status.success(), "responses={responses}");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "recovered\n");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("rate limited; retrying in 0s (attempt 1)"),
            "responses={responses}, {error}"
        );
        assert!(!error.contains("fixture-key"), "{error}");
        let contexts = stats::contexts(dir.path());
        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0]["request"], first_body);
        no_more(&server);
    }
}

// The transport-failure branches must observe real on-the-wire bytes, so this file keeps its own private raw TCP
// fixture.
fn raw_models(addr: SocketAddr, responses: bool) -> String {
    if responses {
        json!({"providers": {"fixture": {
            "api_key": "fixture-key",
            "base_url": format!("http://{addr}/v1"),
            "protocol": "response",
            "models": {"primary": {"model": "fixture-model"}}
        }}})
        .to_string()
    } else {
        format!(
            "# Loopback service for this test process\nproviders:\n  fixture:\n    protocol: completion\n    api_key: fixture-key\n    base_url: http://{addr}/v1\n    models:\n      primary:\n        model: fixture-model\n"
        )
    }
}

fn raw_accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "model request did not arrive");
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("model connection failed: {error}"),
        }
    }
}

// Read one complete Content-Length request: the headers up to the empty line, with the body filled in strictly by
// length.
fn raw_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let boundary = loop {
        if let Some(boundary) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break boundary;
        }
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "connection closed before request headers");
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = std::str::from_utf8(&buffer[..boundary]).unwrap();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap().split(' ');
    assert_eq!(request_line.next(), Some("POST"));
    let target = request_line.next().unwrap().to_owned();
    let length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        .expect("model request must carry Content-Length");
    let mut body = buffer[boundary + 4..].to_vec();
    while body.len() < length {
        let read = stream.read(&mut chunk).unwrap();
        assert!(read > 0, "connection closed before request body");
        body.extend_from_slice(&chunk[..read]);
    }
    assert_eq!(body.len(), length, "request body exceeded Content-Length");
    (target, body)
}

fn raw_respond(stream: &mut TcpStream, body: &[u8]) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
}

fn raw_no_more(listener: &TcpListener, stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut chunk = [0u8; 1];
    match stream.read(&mut chunk) {
        Ok(0) => {}
        Ok(_) => panic!("unexpected third model request"),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::ConnectionReset
            ) => {}
        Err(error) => panic!("unexpected third model request: {error}"),
    }
    match listener.accept() {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(_) => panic!("unexpected third model connection"),
        Err(_) => {}
    }
}

// Chat stream: the same two delta frames as reply plus the terminating marker.
fn raw_chat_sse(text: &str) -> Vec<u8> {
    let mut body = String::new();
    for frame in [
        chat_chunk(json!({"role": "assistant", "content": text}), None),
        chat_chunk(json!({}), Some("stop")),
    ] {
        body.push_str(&format!("data: {frame}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body.into_bytes()
}

// Responses stream: reproduces the delta→content_part.done→output_item.done→response.completed order of
// response_final.
fn raw_response_sse(text: &str) -> Vec<u8> {
    let item = response_message("msg-final", json!([response_text(text)]));
    let output = json!([item.clone()]);
    let events = [
        json!({
            "type": "response.output_text.delta", "sequence_number": 0,
            "item_id": item["id"], "output_index": 0, "content_index": 0,
            "delta": text, "logprobs": []
        }),
        json!({
            "type": "response.content_part.done", "sequence_number": 1,
            "item_id": item["id"], "output_index": 0, "content_index": 0,
            "part": response_text(text)
        }),
        json!({
            "type": "response.output_item.done", "sequence_number": 2,
            "output_index": 0, "item": item
        }),
        json!({
            "type": "response.completed", "sequence_number": 3,
            "response": response_body(output)
        }),
    ];
    let mut body = String::new();
    for event in events {
        body.push_str(&format!("data: {event}\n\n"));
    }
    body.into_bytes()
}
