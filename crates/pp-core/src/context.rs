use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextUsage {
    pub estimated_tokens: u64,
    pub context_window: Option<u64>,
    pub last_request: Option<TokenUsage>,
    /// 整个会话所有普通请求的服务端 output tokens 之和；只随会话根变化重置。
    #[serde(default)]
    pub session_output_tokens: u64,
    pub auto_compact: bool,
    pub threshold_percent: u8,
}

impl ContextUsage {
    pub fn percent(&self) -> Option<u128> {
        self.context_window
            .map(|window| u128::from(self.estimated_tokens) * 100 / u128::from(window))
    }

    pub fn cli_status(&self, model: &str) -> String {
        let context = match (self.context_window, self.percent()) {
            (Some(window), Some(percent)) => format!(
                "Context: ~{} / {window} tokens ({percent}%, estimated)",
                self.estimated_tokens
            ),
            _ => format!("Context: ~{} / ? tokens (estimated)", self.estimated_tokens),
        };
        let request = match self.last_request {
            Some(usage) => format!(
                "Last request: input {} / output {} tokens",
                usage.input_tokens, usage.output_tokens
            ),
            None => "Last request: unavailable".into(),
        };
        let policy = if !self.auto_compact {
            "off".into()
        } else if self.context_window.is_none() {
            "inactive (context_window not configured)".into()
        } else {
            format!("on at {}%", self.threshold_percent)
        };
        format!("Model: {model} · {context} · {request} · Auto compact: {policy}\n")
    }
}

/// 原生 JSON 文本按三字节估一 token，内嵌图片每张估 4096 token。
/// 这只是容量启发式，既不是计费依据，也不分配完整历史的序列化副本。
pub fn estimate_tokens(value: &impl Serialize) -> Result<u64> {
    estimate_bytes(value).map(estimate_tokens_from_bytes)
}

/// 把估算字节数折算成 token：每三字节一 token，向上取整。
///
/// 全量估算与增量累加共用这一条折算规则，不允许各自取整。
pub fn estimate_tokens_from_bytes(bytes: u64) -> u64 {
    bytes.div_ceil(3)
}

/// 逐条消息累加的估算字节数：单条估算之和，加上 `n - 1` 个 JSON 数组逗号。
///
/// 它与 [`estimate_bytes`] 可组合：请求骨架（其余字段与数组方括号）的字节数加上
/// 本累计，等于整棵请求序列化后的字节数。空切片为 0。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MessageBytes {
    bytes: u64,
    count: usize,
}

impl MessageBytes {
    /// 追加一条已用 [`estimate_bytes`] 估算过的消息。
    pub fn push(&mut self, bytes: u64) {
        self.bytes = self.bytes.saturating_add(bytes);
        self.count = self.count.saturating_add(1);
    }

    /// 丢弃全部累计；整段替换历史后必须重算。
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    pub fn count(&self) -> usize {
        self.count
    }

    /// 消息数组内部（不含方括号）的估算字节数。
    pub fn total(&self) -> u64 {
        self.bytes
            .saturating_add(self.count.saturating_sub(1) as u64)
    }
}

/// 统计原生 JSON 文本的估算字节数（内嵌图片等价 4096 * 3 字节）。
pub fn estimate_bytes(value: &impl Serialize) -> Result<u64> {
    let mut count = EstimateCount::default();
    serde_json::to_writer(&mut count, value)
        .map_err(|_| anyhow::anyhow!("estimating context usage failed"))?;
    Ok(count.bytes)
}

#[derive(Default)]
struct EstimateCount {
    bytes: u64,
    string: bool,
    escaped: bool,
    prefix: Vec<u8>,
    candidate: bool,
    image: bool,
}

impl Write for EstimateCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut i = 0;
        while i < bytes.len() {
            if self.image {
                if let Some(rel) = bytes[i..].iter().position(|&b| b == b'"') {
                    self.image = false;
                    self.string = false;
                    self.bytes = self.bytes.saturating_add(1);
                    i += rel + 1;
                } else {
                    break;
                }
                continue;
            }
            if !self.string {
                if let Some(rel) = bytes[i..].iter().position(|&b| b == b'"') {
                    self.bytes = self.bytes.saturating_add((rel + 1) as u64);
                    self.string = true;
                    self.candidate = true;
                    self.prefix.clear();
                    i += rel + 1;
                } else {
                    self.bytes = self.bytes.saturating_add((bytes.len() - i) as u64);
                    break;
                }
                continue;
            }
            if self.escaped {
                self.escaped = false;
                self.candidate = false;
                self.bytes = self.bytes.saturating_add(1);
                i += 1;
                continue;
            }
            if !self.candidate {
                if let Some(rel) = bytes[i..].iter().position(|&b| b == b'"' || b == b'\\') {
                    self.bytes = self.bytes.saturating_add((rel + 1) as u64);
                    if bytes[i + rel] == b'\\' {
                        self.escaped = true;
                    } else {
                        self.string = false;
                    }
                    i += rel + 1;
                } else {
                    self.bytes = self.bytes.saturating_add((bytes.len() - i) as u64);
                    break;
                }
                continue;
            }
            let b = bytes[i];
            i += 1;
            self.bytes = self.bytes.saturating_add(1);
            if b == b'\\' {
                self.escaped = true;
                self.candidate = false;
                continue;
            }
            if b == b'"' {
                self.string = false;
                continue;
            }
            self.prefix.push(b);
            let initial = b"data:image/";
            if self.prefix.len() <= initial.len() {
                self.candidate = initial.starts_with(&self.prefix);
            } else if self.prefix.ends_with(b";base64,") {
                self.image = true;
                self.bytes = self.bytes.saturating_add(4096 * 3);
            } else if self.prefix.len() > 80 {
                self.candidate = false;
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_read_image_estimate_excludes_base64_without_copying_history() {
        let small = serde_json::json!({"image_url":"data:image/png;base64,AAAA"});
        let large = serde_json::json!({"image_url":format!("data:image/png;base64,{}", "A".repeat(100_000))});
        assert_eq!(
            estimate_tokens(&small).unwrap(),
            estimate_tokens(&large).unwrap()
        );
        assert!(estimate_tokens(&large).unwrap() >= 4096);
        let both = serde_json::json!([small, large]);
        assert!(estimate_tokens(&both).unwrap() >= 8192);
    }

    #[test]
    fn context_percent_handles_overcapacity_without_overflow() {
        let mut usage = ContextUsage {
            estimated_tokens: u64::MAX,
            context_window: Some(1),
            last_request: None,
            session_output_tokens: 0,
            auto_compact: true,
            threshold_percent: 80,
        };
        assert_eq!(usage.percent(), Some(u128::from(u64::MAX) * 100));
        usage.context_window = None;
        assert_eq!(usage.percent(), None);
    }

    #[test]
    fn context_estimate_redacts_serializer_errors() {
        struct Invalid;
        impl Serialize for Invalid {
            fn serialize<S: serde::Serializer>(
                &self,
                _: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("secret-payload"))
            }
        }
        assert_eq!(
            estimate_tokens(&Invalid).unwrap_err().to_string(),
            "estimating context usage failed"
        );
    }

    #[test]
    fn message_bytes_compose_with_request_skeleton() {
        let cases = [
            ("empty", serde_json::json!([])),
            (
                "plain",
                serde_json::json!([{"role": "user", "content": "hello"}]),
            ),
            (
                "escaped",
                serde_json::json!([{"content": "quote \" backslash \\ tab \t bell \u{7}"}]),
            ),
            (
                "image",
                serde_json::json!([{"image_url": format!("data:image/png;base64,{}", "A".repeat(5_000))}]),
            ),
            (
                "mixed",
                serde_json::json!([
                    {"role": "user", "content": "hi"},
                    {"image_url": "data:image/jpeg;base64,AAAA"},
                    {"tool_call": {"id": "c1", "arguments": "{\"path\": \"a.rs\"}"}}
                ]),
            ),
        ];
        let skeleton = serde_json::json!({"model": "m", "messages": [], "tools": [{"name": "t"}]});
        for (name, messages) in cases {
            let items = messages.as_array().expect("case fixture is an array");
            let full =
                serde_json::json!({"model": "m", "messages": messages, "tools": [{"name": "t"}]});
            let mut accumulated = MessageBytes::default();
            for item in items {
                accumulated.push(estimate_bytes(item).unwrap());
            }
            assert_eq!(accumulated.count(), items.len(), "{name}");
            assert_eq!(
                estimate_bytes(&skeleton).unwrap() + accumulated.total(),
                estimate_bytes(&full).unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn estimate_count_matches_across_chunk_boundaries() {
        let payload = serde_json::json!([
            {"text": "hello \"escaped\" \\n \\\"data:image/png;base64,not_an_image\\\""},
            {"image_url": format!("data:image/jpeg;base64,{}", "B".repeat(512))},
            {"tail": "data:image/not_base64_prefix_then_long_text"}
        ]);
        let serialized = serde_json::to_vec(&payload).unwrap();
        let baseline = estimate_tokens(&payload).unwrap();
        for chunk_size in [1usize, 3, 7, 32] {
            let mut count = EstimateCount::default();
            for chunk in serialized.chunks(chunk_size) {
                count.write_all(chunk).unwrap();
            }
            assert_eq!(count.bytes.div_ceil(3), baseline, "chunk_size={chunk_size}");
        }
    }
}
