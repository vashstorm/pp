//! Behavior pins for Chat BYOT raw chunk retention and error classification.
//!
//! The failure context relies on `ChatEvent` keeping the raw wire text at the deserialization boundary; three things are pinned here:
//! a valid frame's `raw` matches the input bytes byte for byte (whitespace included), invalid JSON fails at the BYOT boundary and
//! keeps the serde_json Syntax/Eof category, and extreme numbers that RawValue accepts but Value rejects
//! still land in the existing `invalid JSON structure` bucket through `redacted_error`.

use super::ChatEvent;
use serde_json::error::Category;

// Same deserialization boundary as BYOT: the SDK calls from_str directly on the SSE data payload.
fn decode(raw: &str) -> Result<ChatEvent, serde_json::Error> {
    serde_json::from_str::<ChatEvent>(raw)
}

#[test]
fn chat_event_preserves_wire_bytes_and_syntax_category() {
    // Valid frame: RawValue buffers from the value's first byte, keeping inner whitespace and field order,
    // which the old compact re-serialization flattened away; transport whitespace around the value is not part of the JSON value.
    let wire = r#" {"id":"chatcmpl-1" ,  "object":"chat.completion.chunk", "choices":[]} "#;
    let event = decode(wire).unwrap();
    assert_eq!(event.raw, wire.trim());
    assert!(event.raw.contains(r#""chatcmpl-1" ,  "object""#));
    assert_eq!(event.value["id"], "chatcmpl-1");

    // Syntax errors and truncated frames both fail at the BYOT boundary; the serde_json category is preserved.
    for (frame, category) in [
        (r#"{"evidence":"broken",}"#, Category::Syntax),
        (r#"{"choices":[{"delta":"partial"#, Category::Eof),
        (r#"not json at all"#, Category::Syntax),
    ] {
        let error = decode(frame).err().unwrap();
        assert_eq!(error.classify(), category, "{frame}: {error}");
    }

    // RawValue only validates syntax, so extreme numbers get through; Value's rejection goes through a custom conversion,
    // classifies as Data and still lands in the invalid JSON structure bucket via redacted_error.
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
