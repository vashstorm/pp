use super::*;

#[test]
fn read_files_images_web_and_resume_send_real_content_both_protocols() {
    for protocol in PROTOCOLS {
        let server = server();
        let web = super::super::server();
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
