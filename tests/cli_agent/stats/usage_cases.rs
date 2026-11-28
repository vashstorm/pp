use super::*;

#[test]
fn stats_responses_captures_real_usage_and_keeps_unknown_cache_unknown() {
    for (cached, written) in [
        (Some(40), Some(5)),
        (Some(40), Some(0)),
        (Some(40), None),
        (None, None),
    ] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            dir.path(),
            Some("stats response"),
            fixture_settings(),
            &response_models(&server, Some("response")),
        );
        let (request, first_body) = receive_at(&server, "/v1/responses");
        let output = json!([response_message(
            "msg-stats",
            json!([{"type":"output_text","text":"response done","annotations":[]}])
        )]);
        let mut body = response_body(output);
        if let Some(value) = cached {
            body["usage"] = json!({"input_tokens":50,"output_tokens":10,"total_tokens":60,"input_tokens_details":{"cached_tokens":value},"output_tokens_details":{"reasoning_tokens":0}});
            if let Some(written) = written {
                body["usage"]["input_tokens_details"]["cache_write_tokens"] = json!(written);
            }
        }
        let mut stream = request.start_sse().unwrap();
        stream.event(&json!({"type":"response.output_text.delta","sequence_number":0,"item_id":"msg-stats","output_index":0,"content_index":0,"delta":"response done","logprobs":[]})).unwrap();
        stream
            .event(&json!({"type":"response.completed","sequence_number":1,"response":body}))
            .unwrap();
        stream.finish();
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let records = rows(dir.path());
        assert_eq!(records.len(), 1);
        assert_eq!(contexts(dir.path())[0]["request"], first_body);
        assert_eq!(records[0]["cache"], json!(cached));
        assert_eq!(records[0]["cache_write"], json!(written));
        assert_eq!(
            records[0]["input"],
            if cached.is_some() {
                json!(50)
            } else {
                Value::Null
            }
        );
        assert_eq!(
            records[0]["output"],
            if cached.is_some() {
                json!(10)
            } else {
                Value::Null
            }
        );
        assert!(records[0]["ttft"].is_number());
    }
}
