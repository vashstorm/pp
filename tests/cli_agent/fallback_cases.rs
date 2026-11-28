use super::*;

fn path(protocol: &str) -> &'static str {
    if protocol == "response" {
        "/v1/responses"
    } else {
        "/v1/chat/completions"
    }
}

fn finish(request: CapturedRequest, protocol: &str, text: &str) {
    if protocol == "response" {
        response_final(request, text);
    } else {
        final_reply(request, text);
    }
}

#[test]
fn fallback_retry_preserves_committed_tools_and_excludes_partial_stream() {
    let protocol = "response";
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
    let mut stream = request.start_sse().unwrap();
    if protocol == "completion" {
        stream
            .event(&chat_chunk(json!({"content":"discarded-fragment"}), None))
            .unwrap();
    } else {
        stream.event(&json!({"type":"response.output_text.delta", "item_id":"partial", "output_index":0,"content_index":0,"sequence_number":1,"delta":"discarded-fragment"})).unwrap();
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
