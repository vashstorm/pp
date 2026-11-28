//! Non-interactive `--output-schema` runtime constraint injection and `jsonschema` validation retry state.

use anyhow::Result;
use serde_json::Value;

const MAX_SCHEMA_RETRIES: u8 = 2;

#[derive(Clone, Debug)]
pub(in crate::agent) struct OutputSchema {
    raw_json: String,
    schema: Value,
    retries_left: u8,
}

impl OutputSchema {
    pub(in crate::agent) fn new(schema: Value) -> Result<Self> {
        pp_tools::validate_schema(&schema)?;
        let raw_json = serde_json::to_string(&schema)?;
        Ok(Self {
            raw_json,
            schema,
            retries_left: MAX_SCHEMA_RETRIES,
        })
    }

    pub(in crate::agent) fn reset_retries(&mut self) {
        self.retries_left = MAX_SCHEMA_RETRIES;
    }

    pub(in crate::agent) fn consume_retry(&mut self) -> bool {
        if self.retries_left == 0 {
            return false;
        }
        self.retries_left -= 1;
        true
    }

    pub(in crate::agent) fn instruction_message(&self) -> String {
        format!(
            "Output requirement: your final assistant response MUST be a single valid JSON value conforming to this JSON Schema:\n{}\n\nOutput ONLY raw JSON. Do not wrap it in markdown code fences or add extra text before or after the JSON.",
            self.raw_json
        )
    }

    pub(in crate::agent) fn correction_message(reason: &str) -> String {
        format!(
            "Your previous response did not match the required --output-schema ({reason}). Reply again with ONLY raw valid JSON conforming strictly to the schema."
        )
    }

    pub(in crate::agent) fn validate(&self, text: &str) -> std::result::Result<(), String> {
        let trimmed = text.trim();
        let value: Value =
            serde_json::from_str(trimmed).map_err(|error| format!("invalid JSON: {error}"))?;
        pp_tools::validate_output(&self.schema, &value).map_err(|error| format!("{error:#}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_raw_json_and_tracks_retry_budget() {
        let mut schema = OutputSchema::new(json!({
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
            "additionalProperties": false
        }))
        .unwrap();

        schema.validate(r#"{"answer":"ok"}"#).unwrap();
        assert!(
            schema
                .validate("```json\n{\"answer\":\"ok\"}\n```")
                .is_err()
        );
        assert!(schema.validate(r#"{"wrong":1}"#).is_err());
        assert!(schema.consume_retry());
        assert!(schema.consume_retry());
        assert!(!schema.consume_retry());
        schema.reset_retries();
        assert!(schema.consume_retry());
    }
}
