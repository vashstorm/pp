//! Chat BYOT 原始分片保留与错误分类的行为钉子。
//!
//! 失败现场依赖 `ChatEvent` 在反序列化边界保留线上原文；这里钉住三件事：
//! 合法帧的 `raw` 与输入字节逐字节一致（含空白）、非法 JSON 在 BYOT 边界失败且
//! serde_json 分类保持 Syntax／Eof、RawValue 通过而 Value 拒绝的极端数字
//! 经 `redacted_error` 仍落进现有 `invalid JSON structure` 桶。

use super::ChatEvent;
use serde_json::error::Category;

// 与 BYOT 相同的反序列化边界：SDK 直接对 SSE 的 data 载荷调用 from_str。
fn decode(raw: &str) -> Result<ChatEvent, serde_json::Error> {
    serde_json::from_str::<ChatEvent>(raw)
}

#[test]
fn chat_event_preserves_wire_bytes_and_syntax_category() {
    // 合法帧：RawValue 从值首字节开始缓冲，保留内部空白与字段顺序，
    // 旧的紧凑重排会把这些全部抹平；值前后的传输空白不属于 JSON 值本身。
    let wire = r#" {"id":"chatcmpl-1" ,  "object":"chat.completion.chunk", "choices":[]} "#;
    let event = decode(wire).unwrap();
    assert_eq!(event.raw, wire.trim());
    assert!(event.raw.contains(r#""chatcmpl-1" ,  "object""#));
    assert_eq!(event.value["id"], "chatcmpl-1");

    // 语法错误与截断帧都在 BYOT 边界失败，serde_json 分类原样保留。
    for (frame, category) in [
        (r#"{"evidence":"broken",}"#, Category::Syntax),
        (r#"{"choices":[{"delta":"partial"#, Category::Eof),
        (r#"not json at all"#, Category::Syntax),
    ] {
        let error = decode(frame).err().unwrap();
        assert_eq!(error.classify(), category, "{frame}: {error}");
    }

    // RawValue 只做语法校验，极端数字可以通过；Value 的拒绝经 custom 转换，
    // 分类为 Data，最终仍由 redacted_error 落进 invalid JSON structure 桶。
    let frame = r#"{"choices":[],"usage":{"total_tokens":1e999}}"#;
    let error = decode(frame).err().unwrap();
    assert_eq!(error.classify(), Category::Data);
    let redacted = crate::retry::redacted_error(
        async_openai::error::OpenAIError::JSONDeserialize(error, frame.to_owned()),
        "Chat Completions",
    )
    .to_string();
    assert!(redacted.contains("invalid JSON structure"), "{redacted}");
    assert!(redacted.contains("line 1"), "{redacted}");
}
