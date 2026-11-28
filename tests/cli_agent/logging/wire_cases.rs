//! Wire-text preservation for a Chat streaming failure: a wire payload containing whitespace stays observable byte for
//! byte, and the counting and redaction semantics are unchanged. Reuses the parent module helpers and loopback provider.

use super::*;

#[test]
fn logging_chat_stream_keeps_wire_chunk_bytes_and_redacts() {
    let server = server();
    let work = tempfile::tempdir().unwrap();
    let cli = Cli::start_with(
        work.path(),
        Some("private-wire-prompt"),
        fixture_settings(),
        &fixture_models(&server),
    );
    let (request, _) = receive(&server);
    // Raw SSE frames: a valid chunk carrying whitespace decodes successfully first, then invalid JSON triggers the
    // failure so the failure evidence holds the first chunk's wire text.
    let wire = r#" { "api_key" : "fixture-key" , "id" : "chatcmpl-wire" , "object" : "chat.completion.chunk" , "created" : 1 , "model" : "fixture-model" , "choices" : [ { "index" : 0 , "delta" : { "content" : "wire-evidence" } } ] } "#;
    let mut stream = request.start_sse().unwrap();
    stream.frame(wire).unwrap();
    stream
        .frame(r#"{ "evidence" : "broken" , "choices" : ] }"#)
        .unwrap();
    stream.finish();
    let output = cli.finish();
    assert!(!output.status.success());

    // Before the successful decode the failure evidence holds a single chunk; serde_json's RawValue buffers from the
    // value's first byte, preserving inner whitespace and field order (the old compact re-serialization broke both) and
    // dropping only the transport whitespace around the value; credential values stay masked.
    let records = log_records(work.path());
    let failure = records
        .iter()
        .find(|record| event_name(record) == "ai_request_failed")
        .unwrap();
    assert_eq!(failure["fields"]["received_events"].as_u64(), Some(1));
    let redacted = wire.trim().replace("fixture-key", "[REDACTED]");
    assert_eq!(
        failure["fields"]["recent_events"].as_str().unwrap(),
        format!("{:?}", vec![redacted]),
        "recent events must keep the raw wire bytes with interior whitespace"
    );

    // Credentials and the request prompt are never written to disk, and stderr leaks no evidence.
    assert_no_secrets(work.path(), &["fixture-key", "private-wire-prompt"]);
    assert!(log_text(work.path()).contains("wire-evidence"));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("fixture-key"), "{stderr}");
    assert!(!stderr.contains("wire-evidence"), "{stderr}");
    assert!(!stderr.contains("private-wire-prompt"), "{stderr}");
    assert!(
        stderr.contains("decoding Chat Completions response"),
        "{stderr}"
    );
    no_more(&server);
}
