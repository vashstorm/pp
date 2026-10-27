use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TokenUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    #[serde(default)]
    pub(crate) cached_tokens: Option<u64>,
    #[serde(default)]
    pub(crate) cache_write_tokens: Option<u64>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ContextUsage {
    pub(crate) estimated_tokens: u64,
    pub(crate) context_window: Option<u64>,
    pub(crate) last_request: Option<TokenUsage>,
    pub(crate) auto_compact: bool,
    pub(crate) threshold_percent: u8,
}

impl ContextUsage {
    pub(crate) fn percent(&self) -> Option<u128> {
        self.context_window
            .map(|window| u128::from(self.estimated_tokens) * 100 / u128::from(window))
    }

    pub(crate) fn cli_status(&self, model: &str) -> String {
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

#[cfg(test)]
#[derive(Default)]
struct ByteCount(u64);

#[cfg(test)]
impl Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len() as u64);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Byte length of a value's JSON encoding, without allocating the encoding.
#[cfg(test)]
pub(crate) fn serialized_len(value: &impl Serialize) -> Result<usize> {
    let mut count = ByteCount::default();
    serde_json::to_writer(&mut count, value)
        .map_err(|_| anyhow::anyhow!("estimating context usage failed"))?;
    Ok(count.0 as usize)
}

/// 原生 JSON 文本按三字节估一 token，内嵌图片每张估 4096 token。
/// 这只是容量启发式，既不是计费依据，也不分配完整历史的序列化副本。
pub(crate) fn estimate_tokens(value: &impl Serialize) -> Result<u64> {
    let mut count = EstimateCount::default();
    serde_json::to_writer(&mut count, value)
        .map_err(|_| anyhow::anyhow!("estimating context usage failed"))?;
    Ok(count.bytes.div_ceil(3))
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
        for &b in bytes {
            if self.image {
                if b == b'"' {
                    self.image = false;
                    self.string = false;
                    self.bytes = self.bytes.saturating_add(1);
                }
                continue;
            }
            self.bytes = self.bytes.saturating_add(1);
            if !self.string {
                if b == b'"' {
                    self.string = true;
                    self.candidate = true;
                    self.prefix.clear();
                }
                continue;
            }
            if self.escaped {
                self.escaped = false;
                self.candidate = false;
                continue;
            }
            if b == b'\\' {
                self.escaped = true;
                self.candidate = false;
                continue;
            }
            if b == b'"' {
                self.string = false;
                continue;
            }
            if self.candidate {
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
        let text = serde_json::json!({"text":"ordinary 中文 text with \\\"quotes\\\""});
        assert_eq!(
            estimate_tokens(&text).unwrap(),
            (serialized_len(&text).unwrap() as u64).div_ceil(3)
        );
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
}
