use anyhow::{Context, Result, anyhow};
use serde_path_to_error::{Path, Segment};
use serde_yaml::{Mapping, Value};
use std::fmt::Write;

pub fn parse<T: serde::de::DeserializeOwned>(text: &str, kind: &str) -> Result<T> {
    // 先检查 YAML 的重复键，再按数据模型校验已知字段的结构和类型，
    // 避免数字转字符串或 null 被当成空列表、空映射。
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
        // 宽松解析只在失败后用于定位，且仅在两次解析指向同一条目时采用坐标。
        let location =
            serde_path_to_error::deserialize::<_, T>(serde_yaml::Deserializer::from_str(text))
                .err()
                .filter(|located| located.path().to_string() == path.to_string())
                .and_then(|located| located.inner().location());
        field_error(kind, path, error.inner(), location)
    })
}

// 结构体必须来自映射，不能接受 Serde 默认支持的按位置填充数组。
// 所有嵌套值使用相同规则，保留原配置的严格类型边界。
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

// 解析器原文可能包含凭据。语法错误只取位置，反序列化错误只取键路径和安全的原因。
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
        // 未知键也可能是误填的凭据。保留父路径，用源文件坐标定位该键。
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
    // 常规块映射只替换目标字段；每个候选都重新解析并比较完整数据，
    // 防止嵌套字段、块字符串或特殊 YAML 语法被误改。
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
            // 仅当前缀确实表示原值时才保留行尾注释，避免误认字符串中的 #。
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
        // 迁移后只剩注释和空映射时，新增第一项也保留原注释。
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
    // 流式映射等格式统一输出为块映射，字段值保持不变。
    serde_yaml::to_string(&expected).context("Failed to serialize YAML settings")
}

/// 仅删除指定顶层字段；每个局部候选都与预期完整映射比较。
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
        // update：只替换目标字段，注释、字面块正文与其他字段保持原样。
        let text = "# 配置说明\nthink_level: high # 思考等级\nmcp_servers:\n  local:\n    env:\n      TEXT: |-\n        think_level: high\n        # 字符串内容\n    command: python3\ndisabled_skills:\n- old\n# 结尾说明\n";
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
        assert!(updated.contains("# 结尾说明"));
        let parsed: Mapping = parse(&updated, "settings").unwrap();
        assert_eq!(parsed["disabled_skills"][0], "new");
        let updated = update_field(&updated, "default_model", Value::String("a/b".into())).unwrap();
        assert!(updated.contains("# 配置说明"));
        assert!(updated.contains("default_model: a/b"));

        // remove 后再 update：只删除目标字段，注释与后续写入都保留。
        let migrated = remove_fields("# 保留说明\nthink_level: high\n", &["think_level"]).unwrap();
        let updated = update_field(
            &migrated,
            "disabled_skills",
            Value::Sequence(vec![Value::String("keep".into())]),
        )
        .unwrap();
        assert!(updated.starts_with("# 保留说明\n"));
        let parsed: Mapping = parse(&updated, "settings").unwrap();
        assert_eq!(parsed["disabled_skills"][0], "keep");
        assert!(!parsed.contains_key(Value::String("think_level".into())));

        // remove：注释、字面块正文、流式映射与锚点之外的字段均原样保留。
        for text in [
            "# 保留说明\nthink_level: high\nmcp_servers:\n  local:\n    command: fixture\n    env:\n      BODY: |-\n        think_level: high\n        # 正文不是注释\ndisabled_skills: [keep]\n",
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
