use anyhow::{Context, Result, anyhow};
use serde_path_to_error::{Path, Segment};
use serde_yaml::{Mapping, Value};
use std::fmt::Write;

pub fn parse<T: serde::de::DeserializeOwned>(text: &str, kind: &str) -> Result<T> {
    // Check YAML duplicate keys first, then validate the structure and types of known fields against the data model,
    // so a number coerced to a string, or null, is never taken for an empty list or empty map.
    let value: Value = serde_yaml::from_str(text).map_err(|error| yaml_error(kind, &error))?;
    decode(value, text, kind)
}

pub(crate) fn decode<T: serde::de::DeserializeOwned>(
    value: Value,
    text: &str,
    kind: &str,
) -> Result<T> {
    if !value.is_mapping() {
        return Err(anyhow!(
            "invalid YAML {kind} config: expected a top-level mapping"
        ));
    }
    serde_path_to_error::deserialize(StrictValue(value)).map_err(|error| {
        let path = error.path();
        // Lenient parsing is used only to locate failures after the fact, and its location is accepted only when both parses point at the same entry.
        let location =
            serde_path_to_error::deserialize::<_, T>(serde_yaml::Deserializer::from_str(text))
                .err()
                .filter(|located| located.path().to_string() == path.to_string())
                .and_then(|located| located.inner().location());
        field_error(kind, path, error.inner(), location)
    })
}

// Structs must come from mappings and must not accept the positionally filled arrays Serde supports by default.
// Every nested value follows the same rule, preserving the original configuration's strict type boundaries.
struct StrictValue(Value);

impl<'de> serde::de::IntoDeserializer<'de, serde_yaml::Error> for StrictValue {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

impl<'de> serde::Deserializer<'de> for StrictValue {
    type Error = serde_yaml::Error;

    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        match self.0 {
            Value::Sequence(values) => visitor.visit_seq(serde::de::value::SeqDeserializer::new(
                values.into_iter().map(StrictValue),
            )),
            Value::Mapping(values) => visitor.visit_map(serde::de::value::MapDeserializer::new(
                values
                    .into_iter()
                    .map(|(key, value)| (StrictValue(key), StrictValue(value))),
            )),
            value => value.deserialize_any(visitor),
        }
    }

    fn deserialize_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if !self.0.is_mapping() {
            return Err(serde::de::Error::custom("expected a mapping"));
        }
        self.deserialize_any(visitor)
    }

    fn deserialize_option<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if self.0.is_null() {
            visitor.visit_none()
        } else {
            visitor.visit_some(self)
        }
    }

    fn deserialize_newtype_struct<V: serde::de::Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_enum<V: serde::de::Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if !self.0.is_string() {
            return Err(serde::de::Error::custom("expected an enum string"));
        }
        self.0.deserialize_enum(name, variants, visitor)
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 u8 u16 u32 u64 f32 f64 char str string bytes byte_buf
        unit unit_struct seq tuple tuple_struct map identifier ignored_any
    }
}

// Parser messages may contain credentials. Syntax errors keep only the location, and deserialization errors keep only the key path and a safe reason.
fn yaml_error(kind: &str, error: &serde_yaml::Error) -> anyhow::Error {
    let message = error.to_string();
    let reason =
        if message.starts_with("duplicate entry ") || message.contains(": duplicate entry ") {
            "duplicate mapping key"
        } else {
            "invalid YAML syntax or document structure"
        };
    match error.location() {
        Some(location) => anyhow!(
            "invalid YAML {kind} config at line {}, column {}: {reason}",
            location.line(),
            location.column()
        ),
        None => anyhow!("invalid YAML {kind} config: {reason}"),
    }
}

fn field_error(
    kind: &str,
    path: &Path,
    error: &serde_yaml::Error,
    location: Option<serde_yaml::Location>,
) -> anyhow::Error {
    const EXPECTED_TYPES: &[&str] = &[
        "a boolean",
        "a string",
        "a sequence",
        "a map",
        "a mapping of ids to entries",
        "u8",
        "u16",
        "u32",
        "u64",
        "usize",
        "f64",
    ];
    let message = error.to_string();
    let unknown_field = message.starts_with("unknown field ");
    let mut field = if kind == "auth" {
        String::new()
    } else {
        // Unknown keys may also be mistyped credentials. Keep the parent path and locate the key with source file coordinates.
        let depth = path.iter().len().saturating_sub(usize::from(unknown_field));
        field_path(path.iter().take(depth))
    };
    let reason = if let Some(missing) = message
        .strip_prefix("missing field `")
        .and_then(|name| name.strip_suffix('`'))
    {
        append_key(&mut field, missing);
        "missing required field"
    } else if unknown_field {
        "unknown field"
    } else if message.starts_with("unknown variant ") {
        "unsupported enum value"
    } else if message.starts_with("invalid type:") {
        "invalid field type"
    } else if message == "expected a mapping" {
        "expected a mapping"
    } else if message == "expected an enum string" {
        "expected a string for an enum field"
    } else {
        "invalid field value or structure"
    };
    let mut detail = format!("invalid YAML {kind} config");
    if !field.is_empty() {
        let _ = write!(detail, " at `{field}`");
    }
    if let Some(location) = location {
        let _ = write!(
            detail,
            " (line {}, column {})",
            location.line(),
            location.column()
        );
    }
    let _ = write!(detail, ": {reason}");
    if (message.starts_with("invalid type:") || message.starts_with("invalid value:"))
        && let Some((_, expected)) = message.rsplit_once(", expected ")
        && EXPECTED_TYPES.contains(&expected)
    {
        let _ = write!(detail, " (expected {expected})");
    }
    anyhow!(detail)
}

fn field_path<'a>(mut segments: impl Iterator<Item = &'a Segment>) -> String {
    let mut field = String::new();
    for segment in segments.by_ref().take(16) {
        match segment {
            Segment::Map { key } => append_key(&mut field, key),
            Segment::Seq { index } => {
                let _ = write!(field, "[{index}]");
            }
            Segment::Enum { .. } | Segment::Unknown => field.push_str("[?]"),
        }
    }
    if segments.next().is_some() {
        field.push_str("[…]");
    }
    field
}

fn append_key(path: &mut String, key: &str) {
    if key.chars().take(65).count() > 64 {
        path.push_str("[…]");
    } else if !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        if !path.is_empty() {
            path.push('.');
        }
        path.push_str(key);
    } else {
        let _ = write!(path, "[{key:?}]");
    }
}

pub(crate) fn update_field(text: &str, name: &str, value: Value) -> Result<String> {
    let mut expected: Mapping = parse(text, "settings")?;
    let key = Value::String(name.into());
    let previous = expected.insert(key.clone(), value.clone());
    let replacement = serde_yaml::to_string(&Mapping::from_iter([(key.clone(), value)]))
        .context("Failed to serialize YAML setting")?;
    // Regular block mappings only replace the target field; every candidate is re-parsed and compared against the full data,
    // so nested fields, block strings and unusual YAML syntax are never corrupted.
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let indent = line.len() - line.trim_start().len();
        let matches = line.split_once(':').is_some_and(|(field, _)| {
            serde_yaml::from_str::<Value>(field.trim()).ok().as_ref() == Some(&key)
        });
        if matches {
            let mut field = replacement
                .lines()
                .map(|line| format!("{}{line}\n", " ".repeat(indent)))
                .collect::<String>();
            // Keep the trailing comment only when the prefix really denotes the previous value, so `#` inside a string is not mistaken for a comment.
            for (offset, _) in line.match_indices(" #") {
                if serde_yaml::from_str::<Mapping>(&line[..offset])
                    .ok()
                    .and_then(|m| m.get(&key).cloned())
                    == previous
                {
                    field = format!("{} {}\n", field.trim_end(), line[offset..].trim());
                    break;
                }
            }
            let mut end = start;
            for removed in text[start..].split_inclusive('\n') {
                end += removed.len();
                let candidate = format!("{}{field}{}", &text[..start], &text[end..]);
                if serde_yaml::from_str::<Mapping>(&candidate).ok().as_ref() == Some(&expected) {
                    return Ok(candidate);
                }
            }
        }
        start += line.len();
    }
    if previous.is_none() {
        let candidate = format!("{}\n{replacement}", text.trim_end());
        if serde_yaml::from_str::<Mapping>(&candidate).ok().as_ref() == Some(&expected) {
            return Ok(candidate);
        }
        // When only comments and an empty mapping remain after migration, adding the first entry keeps the original comments.
        if expected.len() == 1 {
            for (offset, _) in text.match_indices("{}") {
                let candidate = format!(
                    "{}{}{}",
                    &text[..offset],
                    replacement.trim_end(),
                    &text[offset + 2..]
                );
                if parse::<Mapping>(&candidate, "settings").ok().as_ref() == Some(&expected) {
                    return Ok(candidate);
                }
            }
        }
    }
    // Formats such as flow mappings are always written back as block mappings with unchanged field values.
    serde_yaml::to_string(&expected).context("Failed to serialize YAML settings")
}

/// Deletes only the named top-level fields; every partial candidate is compared against the full expected mapping.
pub(crate) fn remove_fields(text: &str, names: &[&str]) -> Result<String> {
    let mut result = text.to_owned();
    for name in names {
        let mut expected: Mapping = parse(&result, "settings")?;
        let key = Value::String((*name).into());
        if expected.remove(&key).is_none() {
            continue;
        }
        let mut start = 0;
        let mut replacement = None;
        for line in result.split_inclusive('\n') {
            let matches = line.split_once(':').is_some_and(|(field, _)| {
                serde_yaml::from_str::<Value>(field.trim()).ok().as_ref() == Some(&key)
            });
            if matches {
                let mut end = start;
                for removed in result[start..].split_inclusive('\n') {
                    end += removed.len();
                    let candidate = format!("{}{}", &result[..start], &result[end..]);
                    if parse::<Mapping>(&candidate, "settings").ok().as_ref() == Some(&expected) {
                        replacement = Some(candidate);
                        break;
                    }
                    if expected.is_empty() {
                        let candidate = format!("{candidate}\n{{}}\n");
                        if parse::<Mapping>(&candidate, "settings").ok().as_ref() == Some(&expected)
                        {
                            replacement = Some(candidate);
                            break;
                        }
                    }
                }
            }
            if replacement.is_some() {
                break;
            }
            start += line.len();
        }
        result = match replacement {
            Some(candidate) => candidate,
            None => {
                serde_yaml::to_string(&expected).context("Failed to serialize YAML settings")?
            }
        };
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_rejects_duplicate_keys_null_collections_and_implicit_string_conversion() {
        for text in [
            "a: first\na: secret-marker\n",
            "a: 42",
            "a: true",
            "a: null",
            "a: .nan",
            "a: .inf",
        ] {
            let error =
                parse::<std::collections::BTreeMap<String, String>>(text, "settings").unwrap_err();
            assert!(!error.to_string().contains("secret-marker"));
        }
        assert!(parse::<Vec<String>>("null", "settings").is_err());
        assert!(parse::<Mapping>("", "settings").is_err());
        assert!(parse::<Mapping>("[]", "settings").is_err());
    }

    #[test]
    fn update_and_remove_preserve_comments_and_unrelated_values() {
        // update: replace only the target field; comments, literal block bodies and other fields stay untouched.
        let text = "# config note\nthink_level: high # thinking level\nmcp_servers:\n  local:\n    env:\n      TEXT: |-\n        think_level: high\n        # string content\n    command: python3\ndisabled_skills:\n- old\n# trailing note\n";
        let updated = update_field(text, "think_level", Value::String("low".into())).unwrap();
        assert_eq!(
            updated,
            text.replacen("think_level: high", "think_level: low", 1)
        );
        let updated = update_field(
            &updated,
            "disabled_skills",
            Value::Sequence(vec![Value::String("new".into())]),
        )
        .unwrap();
        assert!(updated.contains("# trailing note"));
        let parsed: Mapping = parse(&updated, "settings").unwrap();
        assert_eq!(parsed["disabled_skills"][0], "new");
        let updated = update_field(&updated, "default_model", Value::String("a/b".into())).unwrap();
        assert!(updated.contains("# config note"));
        assert!(updated.contains("default_model: a/b"));

        // remove followed by update: delete only the target field; comments and later writes are both preserved.
        let migrated =
            remove_fields("# preserved note\nthink_level: high\n", &["think_level"]).unwrap();
        let updated = update_field(
            &migrated,
            "disabled_skills",
            Value::Sequence(vec![Value::String("keep".into())]),
        )
        .unwrap();
        assert!(updated.starts_with("# preserved note\n"));
        let parsed: Mapping = parse(&updated, "settings").unwrap();
        assert_eq!(parsed["disabled_skills"][0], "keep");
        assert!(!parsed.contains_key(Value::String("think_level".into())));

        // remove: comments, literal block bodies, flow mappings and everything outside the anchors stay untouched.
        for text in [
            "# preserved note\nthink_level: high\nmcp_servers:\n  local:\n    command: fixture\n    env:\n      BODY: |-\n        think_level: high\n        # body is not a comment\ndisabled_skills: [keep]\n",
            "{think_level: high, mcp_servers: {local: {command: fixture}}, disabled_skills: [keep]}",
            "base: &base {nested: unchanged}\nthink_level: high\ncopy: *base\n",
        ] {
            let mut expected: Mapping = parse(text, "settings").unwrap();
            expected.remove(Value::String("think_level".into()));
            let updated = remove_fields(text, &["think_level", "missing"]).unwrap();
            assert_eq!(parse::<Mapping>(&updated, "settings").unwrap(), expected);
            assert_eq!(remove_fields(&updated, &["think_level"]).unwrap(), updated);
        }
        assert!(remove_fields("think_level: low\nthink_level: high", &["think_level"]).is_err());
    }

    #[test]
    fn yaml_flow_mapping_is_saved_as_yaml_without_changing_other_values() {
        let updated = update_field(
            r#"{"think_level":"high","default_model":"a/b # literal"}"#,
            "think_level",
            Value::String("low".into()),
        )
        .unwrap();
        let parsed: Mapping = parse(&updated, "settings").unwrap();
        assert_eq!(parsed["default_model"], "a/b # literal");
        assert_eq!(parsed["think_level"], "low");
        assert!(!updated.starts_with('{'));
    }
}
