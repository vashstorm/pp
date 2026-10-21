use anyhow::{Context, Result, anyhow};
use serde_yaml::{Mapping, Value};

// 解析器错误可能包含凭证值，只返回固定的脱敏提示。
pub(crate) fn parse<T: serde::de::DeserializeOwned>(text: &str, kind: &str) -> Result<T> {
    // 先检查 YAML 的重复键，再通过严格的数据模型反序列化，
    // 避免数字转字符串或 null 被当成空列表、空映射。
    let value: Value = serde_yaml::from_str(text).map_err(|_| invalid_config(kind))?;
    if !value.is_mapping() {
        return Err(invalid_config(kind));
    }
    T::deserialize(StrictValue(value)).map_err(|_| invalid_config(kind))
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

fn invalid_config(kind: &str) -> anyhow::Error {
    anyhow!(
        "invalid YAML {kind} config: check syntax, field types, required fields, and unknown fields"
    )
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
    }
    // 流式映射等格式统一输出为块映射，字段值保持不变。
    serde_yaml::to_string(&expected).context("Failed to serialize YAML settings")
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
    fn yaml_updates_preserve_unrelated_blocks_and_comments() {
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
