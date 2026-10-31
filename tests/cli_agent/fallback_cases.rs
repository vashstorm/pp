use super::*;

fn models(server: &Server, primary: &str, backup: &str) -> String {
    json!({"providers": {
        "first": {"api_key":"fixture-key", "base_url":format!("http://{}/v1", server.server_addr()),
            "protocol":primary, "models":{"main":{"model":"main-model"}}},
        "second": {"api_key":"backup-key", "base_url":format!("http://{}/v1", server.server_addr()),
            "protocol":backup, "models":{"backup":{"model":"backup-model"}}}
    }})
    .to_string()
}

fn settings(budget: u64) -> String {
    json!({"rate_limit_retry_budget_seconds":budget,
        "model_roles":{"default":{"model":"first/main","effort":"none",
            "fallback":{"model":"second/backup","effort":"high"}}}})
    .to_string()
}

fn path(protocol: &str) -> &'static str {
    if protocol == "response" {
        "/v1/responses"
    } else {
        "/v1/chat/completions"
    }
}

fn finish(request: Request, protocol: &str, text: &str) {
    if protocol == "response" {
        response_final(request, text);
    } else {
        final_reply(request, text);
    }
}

fn reject(request: Request) {
    request
        .respond(
            Response::from_string("private fixture-key")
                .with_status_code(503)
                .with_header(Header::from_bytes("Retry-After", "0").unwrap()),
        )
        .unwrap();
}

#[test]
fn fallback_exhausts_each_model_once_without_nested_attempts() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            home.path(),
            Some("fail both models"),
            &settings(30),
            &models(&server, protocol, protocol),
        );
        for model in ["main-model", "backup-model"] {
            let mut previous = None;
            for _ in 0..10 {
                let (request, body) = receive_at(&server, path(protocol));
                assert_eq!(body["model"], model);
                if let Some(previous) = &previous {
                    assert_eq!(previous, &body);
                }
                previous = Some(body);
                reject(request);
            }
        }
        let output = cli.finish();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains("fixture-key"));
        let rows = stats::rows(home.path());
        assert_eq!(rows.len(), 2);
        assert_eq!(
            stats::contexts(home.path())
                .iter()
                .filter(|v| !v.is_null())
                .count(),
            1
        );
        no_more(&server);
    }
}

#[test]
fn fallback_is_task_scoped_and_manual_selection_detaches_role_across_protocols() {
    for (primary, backup) in [("completion", "response"), ("response", "completion")] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let mut cli = Cli::start_with(
            home.path(),
            None,
            &settings(0),
            &models(&server, primary, backup),
        );
        cli.input("first task\n");
        let (request, _) = receive_at(&server, path(primary));
        reject(request);
        let (request, body) = receive_at(&server, path(backup));
        assert_eq!(body["model"], "backup-model");
        if backup == "completion" {
            assert_eq!(body["reasoning_effort"], "high");
        } else {
            assert_eq!(body["reasoning"]["effort"], "high");
        }
        finish(request, backup, "fallback answer");
        cli.input("next task\n");
        let (request, body) = receive_at(&server, path(primary));
        assert_eq!(body["model"], "main-model");
        assert!(body.to_string().contains("fallback answer"));
        finish(request, primary, "primary answer");
        cli.input("/model second/backup\nlast task\n/quit\n");
        let (request, body) = receive_at(&server, path(backup));
        assert_eq!(body["model"], "backup-model");
        reject(request);
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(
            stats::contexts(home.path())
                .iter()
                .filter(|v| !v.is_null())
                .count(),
            1
        );
        no_more(&server);
    }
}

#[test]
fn fallback_retry_preserves_committed_tools_and_excludes_partial_stream() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            home.path(),
            Some("write once then answer"),
            "rate_limit_retry_budget_seconds: 1\n",
            &if protocol == "completion" {
                fixture_models(&server)
            } else {
                response_models(&server, Some("response"))
            },
        );
        let (request, _) = receive_at(&server, path(protocol));
        let args = json!({"command":"printf x >> count.txt"});
        if protocol == "completion" {
            calls_reply(request, json!([call("once", "bash", args)]));
        } else {
            response_reply(request, json!([response_call("once", "bash", args)]));
        }
        let (request, boundary) = receive_at(&server, path(protocol));
        let mut stream = SseReply::new(request);
        if protocol == "completion" {
            stream.event(chat_chunk(json!({"content":"discarded-fragment"}), None));
        } else {
            stream.event(json!({"type":"response.output_text.delta", "item_id":"partial", "output_index":0,"content_index":0,"sequence_number":1,"delta":"discarded-fragment"}));
        }
        stream.finish();
        let (request, retried) = receive_at(&server, path(protocol));
        assert_eq!(boundary, retried);
        assert!(!retried.to_string().contains("discarded-fragment"));
        finish(request, protocol, "complete answer");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("count.txt")).unwrap(),
            "x"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("excluded from history"));
        assert_eq!(stats::rows(home.path()).len(), 3);
        assert_eq!(
            stats::contexts(home.path())
                .iter()
                .filter(|v| !v.is_null())
                .count(),
            1
        );
        no_more(&server);
    }
}
