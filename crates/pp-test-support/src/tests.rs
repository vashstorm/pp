use super::*;
use std::{
    io::{Read, Write},
    net::TcpStream,
    time::Instant,
};

use axum::body::Body;

/// 固定的小 gzip 数据（`hello`，mtime 0）：用于验证原始字节未被改写或自动解压。
const GZIP: &[u8] = &[
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x07,
    0x00, 0x86, 0xa6, 0x10, 0x36, 0x05, 0x00, 0x00, 0x00,
];

fn client(server: &TestServer) -> TcpStream {
    let stream = TcpStream::connect(server.addr()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
}

fn send(stream: &mut TcpStream, request: &str) {
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
}

fn read_reply(stream: &mut TcpStream) -> String {
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).unwrap();
    String::from_utf8(reply).unwrap()
}

#[test]
fn captures_request_and_returns_response_bytes_unmodified() {
    let server = TestServer::start().unwrap();
    let url = format!("http://{}/v1/responses?stream=true", server.addr());
    let payload = (0..2 * 1024 * 1024 + 7)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let expected = payload.clone();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            // 关闭自动解压，才能断言原始 gzip 字节未被改写。
            let client = reqwest::Client::builder().no_gzip().build().unwrap();
            let response = client
                .post(url)
                .header("x-fixture", "kept")
                .body(payload)
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let bytes = response.bytes().await.unwrap();
            (status, headers, bytes)
        })
    });

    let request = server
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .expect("客户端请求必须到达");
    assert_eq!(request.method(), Method::POST);
    assert_eq!(request.target(), "/v1/responses?stream=true");
    assert_eq!(request.headers()["x-fixture"], "kept");
    assert_eq!(request.body(), expected.as_slice());
    request
        .respond(
            Response::builder()
                .status(418)
                .header("content-type", "application/gzip")
                .header("content-encoding", "gzip")
                .header("x-custom", "kept")
                .header("retry-after", "30")
                .body(Body::from(GZIP.to_vec()))
                .unwrap(),
        )
        .unwrap();
    let (status, headers, bytes) = worker.join().unwrap();
    assert_eq!(status, 418);
    assert_eq!(headers["content-type"], "application/gzip");
    assert_eq!(headers["x-custom"], "kept");
    assert_eq!(headers["retry-after"], "30");
    assert_eq!(bytes.as_ref(), GZIP);
}

#[test]
fn held_request_sends_no_headers_and_other_connections_proceed() {
    let server = TestServer::start().unwrap();
    let mut held = client(&server);
    send(
        &mut held,
        "POST /held HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    );
    let held_request = server
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(held_request.target(), "/held");
    held.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut probe = [0; 1];
    let error = held.read(&mut probe).unwrap_err();
    assert!(
        matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ),
        "held request must not send a response header: {error}"
    );

    // 第二条连接在第一条仍未响应时照常收发。
    let mut other = client(&server);
    send(
        &mut other,
        "GET /other HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n",
    );
    let other_request = server
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    other_request
        .respond(Response::builder().body(Body::from("second")).unwrap())
        .unwrap();
    let reply = read_reply(&mut other);
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.ends_with("second"), "{reply}");

    held.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    held_request
        .respond(Response::builder().body(Body::from("held")).unwrap())
        .unwrap();
    let reply = read_reply(&mut held);
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(reply.ends_with("held"), "{reply}");
}

#[test]
fn unresponded_request_answers_empty_500_only_after_drop() {
    let server = TestServer::start().unwrap();
    let mut client = client(&server);
    send(
        &mut client,
        "POST /gone HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
    );
    let request = server
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    drop(request);
    let reply = read_reply(&mut client);
    assert!(reply.starts_with("HTTP/1.1 500"), "{reply}");
    assert!(reply.ends_with("\r\n\r\n"), "{reply}");
}

#[test]
fn streams_sse_frames_and_stops_the_listener_when_dropped() {
    let server = TestServer::start().unwrap();
    let addr = server.addr();
    let (chunks, received) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let client = reqwest::Client::builder().no_gzip().build().unwrap();
            let response = client
                .get(format!("http://{addr}/stream"))
                .send()
                .await
                .unwrap();
            let mut stream = response.bytes_stream();
            let mut body = Vec::new();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.unwrap();
                body.extend_from_slice(&chunk);
                let _ = chunks.send(String::from_utf8_lossy(&chunk).into_owned());
            }
            String::from_utf8(body).unwrap()
        })
    });

    let request = server
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    let mut sse = request.start_sse().unwrap();
    sse.frame("first").unwrap();
    // 生产端尚未 finish，客户端已读到第一段。
    let chunk = received.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(chunk.contains("data: first\n\n"), "{chunk}");
    sse.frame("héllo").unwrap();
    let chunk = received.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(chunk.contains("data: héllo\n\n"), "{chunk}");
    sse.frame("[DONE]").unwrap();
    let chunk = received.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(chunk.contains("data: [DONE]\n\n"), "{chunk}");
    sse.finish();
    let body = worker.join().unwrap();
    assert_eq!(body, "data: first\n\ndata: héllo\n\ndata: [DONE]\n\n");

    // 未完成响应存在时 Drop owner：连接在 2s 内关闭，监听地址不再可连。
    let mut held = client(&server);
    send(
        &mut held,
        "GET /held HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n",
    );
    let held_request = server
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(held_request.target(), "/held");
    assert_eq!(held_request.headers()["host"], "fixture");
    held.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    drop(server);
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buffer = [0; 256];
    let mut reply = Vec::new();
    loop {
        match held.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => reply.extend_from_slice(&buffer[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                assert!(Instant::now() < deadline, "connection survived owner drop");
            }
            Err(_) => break,
        }
    }
    assert!(reply.is_empty(), "no response bytes expected: {reply:?}");
    let mut closed = false;
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(50)).is_err() {
            closed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(closed, "listener survived owner drop");
    assert_eq!(
        held_request
            .respond(Response::new(Body::empty()))
            .unwrap_err()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
}
