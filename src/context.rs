use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::{self, Write};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct TokenUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
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

/// Fixed UTF-8/JSON heuristic, not a tokenizer. Includes native request controls,
/// tools and opaque reasoning; never allocates a serialized copy of history.
pub(crate) fn estimate_tokens(value: &impl Serialize) -> Result<u64> {
    #[derive(Default)]
    struct ByteCount(u64);
    impl Write for ByteCount {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len() as u64);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = ByteCount::default();
    serde_json::to_writer(&mut count, value)
        .map_err(|_| anyhow::anyhow!("estimating context usage failed"))?;
    Ok(count.0.div_ceil(3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn context_estimate_counts_utf8_and_json_escaping() {
        for value in [json!("中文"), json!("\"\\\n"), json!({"input": "中\n"})] {
            let bytes = serde_json::to_vec(&value).unwrap();
            assert_eq!(
                estimate_tokens(&value).unwrap(),
                (bytes.len() as u64).div_ceil(3)
            );
        }
    }

    #[test]
    fn context_estimate_includes_tool_schemas_and_native_reasoning() {
        let base = json!({"input": []});
        let tools = json!({"input": [], "tools": [{"type":"function","name":"read","parameters":{"type":"object","properties":{"path":{"type":"string"}}}}]});
        let reasoning =
            json!({"input": [{"type":"reasoning","encrypted_content":"opaque-native-state"}]});
        assert!(estimate_tokens(&tools).unwrap() > estimate_tokens(&base).unwrap());
        assert!(estimate_tokens(&reasoning).unwrap() > estimate_tokens(&base).unwrap());
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
