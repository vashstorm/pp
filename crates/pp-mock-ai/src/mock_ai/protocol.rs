//! Mock Responses instruction protocol: strict schema, group validation and usage aggregation.
//!
//! A script is either a bare instruction array or a `mock_script` array at the top
//! level of a normal Responses request; unknown instruction fields are rejected. Error messages are fixed and never contain request or script content.

use std::time::Duration;

use anyhow::anyhow;
use pp_core::context::TokenUsage;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

pub(super) const INVALID_MOCK_SCRIPT: &str = "invalid_mock_script";
pub(super) const MOCK_SCRIPT_REQUIRED: &str = "mock_script_required";

/// A script error the client can correct.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ScriptError {
    /// OpenAI-style error code.
    pub code: &'static str,
    /// Fixed error message that never contains request or script content.
    pub message: &'static str,
}

impl ScriptError {
    fn invalid(message: &'static str) -> Self {
        Self {
            code: INVALID_MOCK_SCRIPT,
            message,
        }
    }

    fn required() -> Self {
        Self {
            code: MOCK_SCRIPT_REQUIRED,
            message: "request does not include a mock_script array",
        }
    }
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ScriptError {}

/// A segment of message or reasoning text.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub(super) enum Text {
    /// A string is split by Unicode scalar; sent whole when the rate is unlimited.
    String(String),
    /// Explicit chunks; each non-empty element counts as one mock token.
    Chunks(Vec<String>),
}

impl Default for Text {
    fn default() -> Self {
        Self::String(String::new())
    }
}

impl Text {
    /// Whether this yields no message or reasoning content at all.
    pub(super) fn is_empty(&self) -> bool {
        match self {
            Self::String(text) => text.is_empty(),
            Self::Chunks(chunks) => chunks.iter().all(String::is_empty),
        }
    }

    /// The full text after joining.
    pub(super) fn joined(&self) -> String {
        match self {
            Self::String(text) => text.clone(),
            Self::Chunks(chunks) => chunks.concat(),
        }
    }
}

/// A real tool call; the mock server only sends it and never executes the tool.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Tool {
    pub name: String,
    pub arguments: Value,
}

/// One mock instruction executed in order.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Instruction {
    #[serde(default)]
    pub tool: Option<Tool>,
    #[serde(default)]
    pub content: Text,
    #[serde(default)]
    pub think: Text,
    #[serde(default)]
    pub tokens_per_second: f64,
    #[serde(default)]
    pub response: Option<Value>,
    #[serde(default)]
    pub auto_continue: bool,
    #[serde(default, deserialize_with = "deserialize_usage")]
    pub usage: Option<TokenUsage>,
    /// When set, this request fails with that HTTP status and sends no SSE;
    /// used to reproduce provider errors and automatic retries, and cannot appear together with other output fields.
    #[serde(default)]
    pub status: Option<u16>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsageFields {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cached_tokens: Option<u64>,
    #[serde(default)]
    cache_write_tokens: Option<u64>,
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl From<UsageFields> for TokenUsage {
    fn from(usage: UsageFields) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cached_tokens: usage.cached_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }
}

fn deserialize_usage<'de, D>(deserializer: D) -> std::result::Result<Option<TokenUsage>, D::Error>
where
    D: Deserializer<'de>,
{
    let usage = Option::<UsageFields>::deserialize(deserializer)?;
    Ok(usage.map(Into::into))
}

/// Parse and fully validate a new script submitted by the client.
///
/// The request is a bare instruction array, or a normal Responses request with a top-level `mock_script` array.
/// A missing array returns `mock_script_required`; an invalid array returns `invalid_mock_script`.
pub(super) fn parse_script(
    reader: &mut dyn std::io::Read,
) -> std::result::Result<Vec<Instruction>, ScriptError> {
    let value: Value = serde_json::from_reader(reader).map_err(|error| {
        if error.is_io() {
            ScriptError::invalid("mock script request could not be read")
        } else {
            ScriptError::invalid("mock script request is not valid JSON")
        }
    })?;
    let items = match value {
        Value::Array(items) => items,
        Value::Object(mut object) => match object.remove("mock_script") {
            Some(Value::Array(items)) => items,
            Some(_) => return Err(ScriptError::invalid("mock_script must be an array")),
            None => return Err(ScriptError::required()),
        },
        _ => return Err(ScriptError::required()),
    };
    if items.is_empty() {
        return Err(ScriptError::invalid("mock_script must not be empty"));
    }
    let mut instructions = Vec::with_capacity(items.len());
    for item in items {
        instructions
            .push(serde_json::from_value(item).map_err(|_| {
                ScriptError::invalid("mock_script contains an invalid instruction")
            })?);
    }
    validate_instructions(&instructions)?;
    Ok(instructions)
}

/// Aggregate mock usage for a run of consecutive instructions; returns `None` when all of them omit it.
///
/// The output uses the Responses completion field shape; cache tokens are not counted again in the input total.
pub(super) fn group_usage(instructions: &[Instruction]) -> anyhow::Result<Option<Value>> {
    let mut present = false;
    let mut input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    let mut cached_tokens = 0_u64;
    let mut cache_write_tokens: Option<u64> = None;
    let mut reasoning_tokens = 0_u64;
    for instruction in instructions {
        let Some(usage) = instruction.usage else {
            continue;
        };
        present = true;
        input_tokens = input_tokens
            .checked_add(usage.input_tokens)
            .ok_or_else(usage_error)?;
        output_tokens = output_tokens
            .checked_add(usage.output_tokens)
            .ok_or_else(usage_error)?;
        cached_tokens = cached_tokens
            .checked_add(usage.cached_tokens.unwrap_or(0))
            .ok_or_else(usage_error)?;
        reasoning_tokens = reasoning_tokens
            .checked_add(usage.reasoning_tokens.unwrap_or(0))
            .ok_or_else(usage_error)?;
        if let Some(write) = usage.cache_write_tokens {
            cache_write_tokens = Some(
                cache_write_tokens
                    .unwrap_or(0)
                    .checked_add(write)
                    .ok_or_else(usage_error)?,
            );
        }
    }
    if !present {
        return Ok(None);
    }
    let total_tokens = input_tokens
        .checked_add(output_tokens)
        .ok_or_else(usage_error)?;
    let input_tokens = u32::try_from(input_tokens).map_err(|_| usage_error())?;
    let output_tokens = u32::try_from(output_tokens).map_err(|_| usage_error())?;
    let cached_tokens = u32::try_from(cached_tokens).map_err(|_| usage_error())?;
    let reasoning_tokens = u32::try_from(reasoning_tokens).map_err(|_| usage_error())?;
    let total_tokens = u32::try_from(total_tokens).map_err(|_| usage_error())?;
    let cache_write_tokens = cache_write_tokens
        .map(|value| i64::try_from(value).map_err(|_| usage_error()))
        .transpose()?;
    let mut input_details = serde_json::Map::new();
    input_details.insert("cached_tokens".into(), json!(cached_tokens));
    if let Some(cache_write_tokens) = cache_write_tokens {
        input_details.insert("cache_write_tokens".into(), json!(cache_write_tokens));
    }
    Ok(Some(json!({
        "input_tokens": input_tokens,
        "input_tokens_details": Value::Object(input_details),
        "output_tokens": output_tokens,
        "output_tokens_details": { "reasoning_tokens": reasoning_tokens },
        "total_tokens": total_tokens,
    })))
}

fn usage_error() -> anyhow::Error {
    anyhow!("mock usage exceeds supported bounds")
}

fn validate_instructions(instructions: &[Instruction]) -> std::result::Result<(), ScriptError> {
    for instruction in instructions {
        validate_instruction(instruction)?;
    }
    let mut start = 0;
    while start < instructions.len() {
        // A failure instruction forms its own group and needs no text or tool call.
        if instructions[start].status.is_some() {
            start += 1;
            continue;
        }
        let end = instructions[start..]
            .iter()
            .position(|instruction| !instruction.auto_continue)
            .map_or(instructions.len(), |offset| start + offset + 1);
        let group = &instructions[start..end];
        if group[1..]
            .iter()
            .any(|instruction| instruction.status.is_some())
        {
            return Err(ScriptError::invalid(
                "mock failure instruction cannot follow auto_continue",
            ));
        }
        if !group.iter().any(has_visible_output) {
            return Err(ScriptError::invalid(
                "each mock response group needs text or a tool call",
            ));
        }
        if group_usage(group).is_err() {
            return Err(ScriptError::invalid("mock usage exceeds supported bounds"));
        }
        start = end;
    }
    Ok(())
}

fn validate_instruction(instruction: &Instruction) -> std::result::Result<(), ScriptError> {
    if let Some(tool) = &instruction.tool
        && tool.name.is_empty()
    {
        return Err(ScriptError::invalid("mock tool name must not be empty"));
    }
    if let Some(status) = instruction.status {
        if !(400..=599).contains(&status) {
            return Err(ScriptError::invalid(
                "mock status must be an HTTP error status",
            ));
        }
        // An explicit `response:null` counts as absent and never produces echoed text.
        let echoes_response = instruction
            .response
            .as_ref()
            .is_some_and(|response| !response.is_null());
        if instruction.auto_continue
            || instruction.tool.is_some()
            || !instruction.content.is_empty()
            || !instruction.think.is_empty()
            || echoes_response
            || instruction.usage.is_some()
        {
            return Err(ScriptError::invalid(
                "mock failure instruction cannot carry output",
            ));
        }
    }
    validate_rate(instruction)
}

fn validate_rate(instruction: &Instruction) -> std::result::Result<(), ScriptError> {
    let rate = instruction.tokens_per_second;
    if !rate.is_finite() || rate < 0.0 {
        return Err(ScriptError::invalid(
            "tokens_per_second must be finite and non-negative",
        ));
    }
    if rate == 0.0 {
        return Ok(());
    }
    let interval = Duration::try_from_secs_f64(1.0 / rate)
        .map_err(|_| ScriptError::invalid("tokens_per_second cannot be scheduled"))?;
    let tokens = instruction_token_count(instruction)?;
    // The first token is sent immediately, each later mock token adds one interval;
    // validate that the worst-case total across all tokens is representable so the runtime never panics.
    scheduling_horizon(interval, tokens).ok_or_else(rate_horizon_error)?;
    Ok(())
}

fn scheduling_horizon(interval: Duration, tokens: u64) -> Option<Duration> {
    let mut total = Duration::ZERO;
    let mut remaining = tokens;
    while remaining > 0 {
        let chunk = remaining.min(u64::from(u32::MAX)) as u32;
        let step = interval.checked_mul(chunk)?;
        total = total.checked_add(step)?;
        remaining -= u64::from(chunk);
    }
    Some(total)
}

fn rate_horizon_error() -> ScriptError {
    ScriptError::invalid("tokens_per_second scheduling exceeds the supported horizon")
}

fn token_count_error() -> ScriptError {
    ScriptError::invalid("mock script payload is too large")
}

fn instruction_token_count(instruction: &Instruction) -> std::result::Result<u64, ScriptError> {
    let mut tokens = text_token_count(&instruction.think)
        .checked_add(text_token_count(&instruction.content))
        .ok_or_else(token_count_error)?;
    if let Some(response) = &instruction.response {
        tokens = tokens
            .checked_add(response_token_count(response)?)
            .ok_or_else(token_count_error)?;
    }
    if let Some(tool) = &instruction.tool {
        tokens = tokens
            .checked_add(compact_token_count(&tool.arguments)?)
            .ok_or_else(token_count_error)?;
    }
    Ok(tokens)
}

fn text_token_count(text: &Text) -> u64 {
    match text {
        Text::String(text) => scalar_count(text),
        Text::Chunks(chunks) => chunks.iter().filter(|chunk| !chunk.is_empty()).count() as u64,
    }
}

fn response_token_count(response: &Value) -> std::result::Result<u64, ScriptError> {
    match response {
        Value::Null => Ok(0),
        Value::String(text) => Ok(scalar_count(text)),
        value => compact_token_count(value),
    }
}

fn compact_token_count(value: &Value) -> std::result::Result<u64, ScriptError> {
    let compact = serde_json::to_string(value)
        .map_err(|_| ScriptError::invalid("mock script payload cannot be serialized"))?;
    Ok(scalar_count(&compact))
}

fn scalar_count(text: &str) -> u64 {
    text.chars().count() as u64
}

fn has_visible_output(instruction: &Instruction) -> bool {
    instruction.tool.is_some()
        || !instruction.content.is_empty()
        || instruction
            .response
            .as_ref()
            .is_some_and(response_has_visible_output)
}

fn response_has_visible_output(response: &Value) -> bool {
    match response {
        Value::Null => false,
        Value::String(text) => !text.is_empty(),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(text: &str) -> std::result::Result<Vec<Instruction>, ScriptError> {
        let mut reader = Cursor::new(text.as_bytes());
        parse_script(&mut reader)
    }

    fn code(text: &str) -> &'static str {
        parse(text).expect_err("script should be rejected").code
    }

    fn instruction(content: Text, auto_continue: bool) -> Instruction {
        Instruction {
            tool: None,
            content,
            think: Text::default(),
            tokens_per_second: 0.0,
            response: None,
            auto_continue,
            usage: None,
            status: None,
        }
    }

    fn with_usage(content: Text, usage: TokenUsage) -> Instruction {
        Instruction {
            usage: Some(usage),
            ..instruction(content, false)
        }
    }

    fn instruction_with_rate(content: Text, rate: f64) -> Instruction {
        Instruction {
            tokens_per_second: rate,
            ..instruction(content, false)
        }
    }

    #[test]
    fn script_parsing_validation_failures_and_token_rates() {
        {
            let bare = parse(r#"[{"content":"hello"}]"#).unwrap();
            assert_eq!(bare.len(), 1);
            assert_eq!(bare[0].content.joined(), "hello");
            assert!(bare[0].think.is_empty());
            assert_eq!(bare[0].tokens_per_second, 0.0);
            assert!(bare[0].response.is_none());
            assert!(!bare[0].auto_continue);
            assert!(bare[0].tool.is_none());
            assert!(bare[0].usage.is_none());

            let wrapped = parse(
                r#"{"model":"pp-mock-root","input":[],"mock_script":[{"content":["a","","b"]}]}"#,
            )
            .unwrap();
            assert_eq!(
                wrapped[0].content,
                Text::Chunks(vec!["a".into(), "".into(), "b".into()])
            );
            assert_eq!(wrapped[0].content.joined(), "ab");
        }
        {
            assert_eq!(code("{}"), "mock_script_required");
            assert_eq!(code(r#"{"input":[]}"#), "mock_script_required");
            assert_eq!(code("null"), "mock_script_required");
            assert_eq!(code("[]"), "invalid_mock_script");
            assert_eq!(code(r#"{"mock_script":null}"#), "invalid_mock_script");
            assert_eq!(code(r#"{"mock_script":{}}"#), "invalid_mock_script");
            assert_eq!(code("not json"), "invalid_mock_script");
            assert_eq!(
                code(r#"[{"content":"x","unknown":1}]"#),
                "invalid_mock_script"
            );
            assert_eq!(
                code(r#"[{"tool":{"name":"","arguments":{}}}]"#),
                "invalid_mock_script"
            );
            assert_eq!(code(r#"[{"tool":{"name":"read"}}]"#), "invalid_mock_script");
            assert_eq!(
                code(r#"[{"content":"x","usage":{"input_tokens":1,"output_tokens":1,"other":2}}]"#),
                "invalid_mock_script"
            );
            assert_eq!(
                code(r#"[{"content":"x","usage":{"input_tokens":1}}]"#),
                "invalid_mock_script"
            );
        }
        {
            assert_eq!(code(r#"[{"think":"only"}]"#), "invalid_mock_script");
            assert_eq!(code(r#"[{"content":["",""]}]"#), "invalid_mock_script");
            assert_eq!(code(r#"[{"response":null}]"#), "invalid_mock_script");
            assert_eq!(
                code(r#"[{"content":"a","auto_continue":false},{"think":"only"}]"#),
                "invalid_mock_script"
            );
            assert_eq!(
                code(
                    r#"[{"think":"plan","auto_continue":true},{"think":"only","auto_continue":false}]"#
                ),
                "invalid_mock_script"
            );

            assert!(parse(r#"[{"think":"plan","auto_continue":true},{"content":"done"}]"#).is_ok());
            assert!(parse(r#"[{"response":{"echo":true}}]"#).is_ok());
            assert!(parse(r#"[{"response":"echo"}]"#).is_ok());
            assert!(
                parse(r#"[{"response":null,"tool":{"name":"read","arguments":{"path":"a"}}}]"#)
                    .is_ok()
            );
            assert!(
            parse(
                r#"[{"content":"a","auto_continue":false},{"tool":{"name":"read","arguments":{}}}]"#
            )
            .is_ok()
        );
        }
        {
            let parsed = parse(r#"[{"status":503},{"content":"recovered"}]"#).unwrap();
            assert_eq!(parsed[0].status, Some(503));
            assert_eq!(parsed[1].status, None);
            // A failure step forms its own group and needs no text; rejected when it carries output fields or a non-error status.
            for script in [
                r#"[{"status":200}]"#,
                r#"[{"status":399}]"#,
                r#"[{"status":600}]"#,
                r#"[{"status":503,"content":"x"}]"#,
                r#"[{"status":503,"think":"x"}]"#,
                r#"[{"status":503,"auto_continue":true}]"#,
                r#"[{"status":503,"tool":{"name":"read","arguments":{}}}]"#,
                r#"[{"status":503,"usage":{"input_tokens":1,"output_tokens":1}}]"#,
                r#"[{"status":503,"response":{"echo":true}}]"#,
                r#"[{"status":400,"response":"echo"}]"#,
                r#"[{"status":503},{}]"#,
            ] {
                assert_eq!(code(script), "invalid_mock_script", "{script}");
            }
            // An explicit `response:null` produces no echoed text and is still a valid standalone failure instruction.
            assert!(parse(r#"[{"status":503,"response":null}]"#).is_ok());
        }
        {
            assert!(parse(r#"[{"content":"abc","tokens_per_second":12.5}]"#).is_ok());
            assert!(parse(r#"[{"content":"abc","tokens_per_second":0}]"#).is_ok());
            assert!(parse(r#"[{"content":"abc","tokens_per_second":-0.0}]"#).is_ok());
            assert!(parse(r#"[{"content":"a","tokens_per_second":1e9}]"#).is_ok());
            // 1e19 seconds is representable; the worst-case total for two Unicode scalars overflows.
            assert_eq!(
                code(r#"[{"content":"xy","tokens_per_second":1e-19}]"#),
                "invalid_mock_script"
            );
            // 1e20 seconds is not representable by itself; rejected even without any payload token.
            let empty_payload = Instruction {
                tokens_per_second: 1e-20,
                ..instruction(Text::default(), false)
            };
            assert!(validate_rate(&empty_payload).is_err());
            assert_eq!(
                code(r#"[{"tool":{"name":"read","arguments":{}},"tokens_per_second":1e-20}]"#),
                "invalid_mock_script"
            );
            // An empty element of explicit chunks is not a token.
            assert!(parse(r#"[{"content":["","x"],"tokens_per_second":1e-19}]"#).is_ok());
            assert_eq!(
                code(r#"[{"content":["","x","y"],"tokens_per_second":1e-19}]"#),
                "invalid_mock_script"
            );
            // A non-string `response` counts tokens by the scalars of its compact JSON.
            assert!(parse(r#"[{"response":"x","tokens_per_second":1e-19}]"#).is_ok());
            assert_eq!(
                code(r#"[{"response":{"a":1},"tokens_per_second":1e-19}]"#),
                "invalid_mock_script"
            );
            // `think` and tool arguments are paced at the same entry rate.
            assert_eq!(
                code(r#"[{"think":"xy","content":"a","tokens_per_second":1e-19}]"#),
                "invalid_mock_script"
            );
            assert_eq!(
                code(r#"[{"tool":{"name":"read","arguments":{}},"tokens_per_second":1e-19}]"#),
                "invalid_mock_script"
            );

            for rate in [
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
                -1.0,
                f64::MIN_POSITIVE,
            ] {
                assert!(
                    validate_instructions(&[instruction_with_rate(Text::String("x".into()), rate)])
                        .is_err()
                );
            }
            assert!(
                validate_instructions(&[instruction_with_rate(Text::String("x".into()), -0.0)])
                    .is_ok()
            );
        }
    }

    #[test]
    fn group_usage_summing_overflow_and_per_group_bounds() {
        {
            let instructions = parse(
            r#"[{"content":"a","auto_continue":true,"usage":{"input_tokens":120,"output_tokens":30,"cached_tokens":40,"cache_write_tokens":5}},{"content":"b","usage":{"input_tokens":1,"output_tokens":2}}]"#,
        )
        .unwrap();
            assert_eq!(
                group_usage(&instructions).unwrap(),
                Some(json!({
                    "input_tokens": 121,
                    "input_tokens_details": {
                        "cached_tokens": 40,
                        "cache_write_tokens": 5
                    },
                    "output_tokens": 32,
                    "output_tokens_details": { "reasoning_tokens": 0 },
                    "total_tokens": 153
                }))
            );

            let without_usage = parse(r#"[{"content":"a"}]"#).unwrap();
            assert_eq!(group_usage(&without_usage).unwrap(), None);

            let zero_write = parse(
            r#"[{"content":"a","usage":{"input_tokens":1,"output_tokens":2,"cached_tokens":0,"cache_write_tokens":0}}]"#,
        )
        .unwrap();
            assert_eq!(
                group_usage(&zero_write).unwrap().unwrap()["input_tokens_details"]["cache_write_tokens"],
                json!(0)
            );
        }
        {
            let max = TokenUsage {
                input_tokens: u64::MAX,
                output_tokens: 0,
                cached_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            };
            let one = TokenUsage {
                input_tokens: 1,
                output_tokens: 0,
                cached_tokens: None,
                cache_write_tokens: None,
                reasoning_tokens: None,
            };
            let instructions = [
                with_usage(Text::String("a".into()), max),
                with_usage(Text::String("b".into()), one),
            ];
            assert!(group_usage(&instructions).is_err());

            let cache_max = TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: None,
                cache_write_tokens: Some(u64::MAX),
                reasoning_tokens: None,
            };
            let instructions = [
                with_usage(Text::String("a".into()), cache_max),
                with_usage(
                    Text::String("b".into()),
                    TokenUsage {
                        cache_write_tokens: Some(1),
                        ..cache_max
                    },
                ),
            ];
            assert!(group_usage(&instructions).is_err());
        }
        {
            assert_eq!(
                code(r#"[{"content":"a","usage":{"input_tokens":4294967296,"output_tokens":0}}]"#),
                "invalid_mock_script"
            );
            assert_eq!(
                code(r#"[{"content":"a","usage":{"input_tokens":4294967295,"output_tokens":1}}]"#),
                "invalid_mock_script"
            );
            assert_eq!(
                code(
                    r#"[{"content":"a","usage":{"input_tokens":1,"output_tokens":0,"cached_tokens":4294967296}}]"#
                ),
                "invalid_mock_script"
            );
            assert_eq!(
                code(
                    r#"[{"content":"a","usage":{"input_tokens":1,"output_tokens":0,"cache_write_tokens":9223372036854775808}}]"#
                ),
                "invalid_mock_script"
            );
            // Different response groups are aggregated independently; the inputs of two groups are not summed.
            assert!(
            parse(
                r#"[{"content":"a","usage":{"input_tokens":4294967295,"output_tokens":0}},{"content":"b","usage":{"input_tokens":4294967295,"output_tokens":0}}]"#
            )
            .is_ok()
        );
        }
    }
}
