use super::*;

#[test]
fn read_http_redirects_without_provider_auth_and_redacts_errors() {
    let server = Arc::new(TestServer::start().unwrap());
    let url = format!("http://{}/first", server.addr());
    let responder = Arc::clone(&server);
    let worker = std::thread::spawn(move || {
        let request = responder
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(
            ![
                "authorization",
                "openai-organization",
                "openai-project",
                "cookie",
            ]
            .iter()
            .any(|name| request.headers().contains_key(*name))
        );
        request
            .respond(
                Response::builder()
                    .status(302)
                    .header("Location", "/final")
                    .body(Body::empty())
                    .unwrap(),
            )
            .unwrap();
        let request = responder
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(request.target(), "/final");
        request
            .respond(
                Response::builder()
                    .header("Content-Type", "text/plain")
                    .body(Body::from("web body"))
                    .unwrap(),
            )
            .unwrap();
    });
    let mut client = test_client("http://127.0.0.1:1/v1");
    let response = client
        .fetch_read_url(&url, &CancellationToken::new())
        .unwrap();
    assert_eq!(response.bytes, b"web body");
    assert!(response.final_url.ends_with("/final"));
    worker.join().unwrap();
    for url in [
        "http://user:secret@127.0.0.1/",
        "ftp://example.com/",
        "bad-url-secret",
    ] {
        let message = client
            .fetch_read_url(url, &CancellationToken::new())
            .err()
            .unwrap()
            .to_string();
        assert!(!message.contains("secret") && !message.contains("private-key"));
    }
}

#[test]
fn read_http_limits_redirects_and_response_body_without_retries() {
    for oversize in [false, true] {
        let server = Arc::new(TestServer::start().unwrap());
        let url = format!("http://{}/loop", server.addr());
        let responder = Arc::clone(&server);
        let worker = std::thread::spawn(move || {
            for _ in 0..if oversize { 1 } else { 6 } {
                let request = responder
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                if oversize {
                    let _ = request.respond(
                        Response::builder()
                            .body(Body::from(vec![b'x'; crate::http::MAX_INPUT_BYTES + 1]))
                            .unwrap(),
                    );
                } else {
                    request
                        .respond(
                            Response::builder()
                                .status(302)
                                .header("Location", "/loop")
                                .body(Body::empty())
                                .unwrap(),
                        )
                        .unwrap();
                }
            }
            assert!(
                responder
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap()
                    .is_none()
            );
        });
        let error = test_client("http://127.0.0.1:1/v1")
            .fetch_read_url(&url, &CancellationToken::new())
            .err()
            .unwrap();
        if oversize {
            assert!(error.to_string().contains("20 MiB"));
        }
        worker.join().unwrap();
    }
}

#[test]
fn read_http_gzip_limit_counts_decompressed_bytes() {
    let server = Arc::new(TestServer::start().unwrap());
    let url = format!("http://{}/compressed", server.addr());
    let responder = Arc::clone(&server);
    let worker = std::thread::spawn(move || {
        let request = responder
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let block = [b'x'; 8192];
        for _ in 0..(crate::http::MAX_INPUT_BYTES / block.len() + 1) {
            encoder.write_all(&block).unwrap();
        }
        let compressed = encoder.finish().unwrap();
        assert!(compressed.len() < crate::http::MAX_INPUT_BYTES);
        let _ = request.respond(
            Response::builder()
                .header("Content-Encoding", "gzip")
                .body(Body::from(compressed))
                .unwrap(),
        );
    });
    let error = test_client("http://127.0.0.1:1/v1")
        .fetch_read_url(&url, &CancellationToken::new())
        .err()
        .unwrap();
    assert!(error.to_string().contains("20 MiB"), "{error}");
    worker.join().unwrap();
}

#[test]
fn read_http_status_timeout_and_cancel_leave_no_background_fetch() {
    for mode in ["status", "timeout", "cancel"] {
        let server = Arc::new(TestServer::start().unwrap());
        let url = format!("http://{}/private-query", server.addr());
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let (done, release) = std::sync::mpsc::channel();
        let responder = Arc::clone(&server);
        let worker = std::thread::spawn(move || {
            let request = responder
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap();
            if mode == "status" {
                request
                    .respond(
                        Response::builder()
                            .status(429)
                            .body(Body::from("secret-body"))
                            .unwrap(),
                    )
                    .unwrap();
            } else if mode == "cancel" {
                let mut writer = request
                    .start_stream(StatusCode::OK, HeaderMap::new())
                    .unwrap();
                writer.send(Bytes::from_static(b"x")).unwrap();
                worker_cancel.cancel();
                release.recv_timeout(Duration::from_secs(5)).unwrap();
                writer.finish();
            } else {
                release.recv_timeout(Duration::from_secs(5)).unwrap();
                drop(request);
            }
            assert!(
                responder
                    .recv_timeout(Duration::from_millis(100))
                    .unwrap()
                    .is_none()
            );
        });
        let mut client = test_client("http://127.0.0.1:1/v1");
        if mode == "timeout" {
            client.read_http = Some(
                reqwest::Client::builder()
                    .timeout(Duration::from_millis(100))
                    .build()
                    .unwrap(),
            );
        }
        let started = Instant::now();
        let error = client.fetch_read_url(&url, &cancel).err().unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        if mode == "cancel" {
            assert!(error.is::<Cancelled>());
        }
        if mode == "status" {
            assert!(error.to_string().contains("429"));
        }
        assert!(
            !error.to_string().contains("secret-body")
                && !error.to_string().contains("private-query")
        );
        let _ = done.send(());
        worker.join().unwrap();
    }
}
