//! Chat 流式失败现场的线上原文保留行为：带空白的线上 payload 逐字节可观察、
//! 计数与脱敏语义不变。沿用父模块的 helper 与回环 provider。

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
    // 原始 SSE frame：带空白的合法分片先成功解码，非法 JSON 再触发失败，
    // 让失败现场持有第一个分片的线上原文。
    let wire = r#" { "api_key" : "fixture-key" , "id" : "chatcmpl-wire" , "object" : "chat.completion.chunk" , "created" : 1 , "model" : "fixture-model" , "choices" : [ { "index" : 0 , "delta" : { "content" : "wire-evidence" } } ] } "#;
    let mut stream = request.start_sse().unwrap();
    stream.frame(wire).unwrap();
    stream
        .frame(r#"{ "evidence" : "broken" , "choices" : ] }"#)
        .unwrap();
    stream.finish();
    let output = cli.finish();
    assert!(!output.status.success());

    // 成功解码前失败现场只收到一个分片；serde_json 的 RawValue 从值首字节开始
    // 缓冲，保留分片内部空白与字段顺序（旧的紧凑重排会破坏这些），仅去掉值
    // 前后的传输空白；凭据值仍被遮蔽。
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

    // 凭据与请求提示词不落盘，stderr 不泄漏现场。
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
