//! Pure staging entry for both edit modes; disk commits are owned by Tools.
use super::{hashline, replace};
use anyhow::{Context, Result, ensure};
use pp_config::config::EditMode;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HashlineArgs {
    input: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplaceArgs {
    #[serde(alias = "file_path", alias = "filePath")]
    path: String,
    #[serde(default)]
    edits: Option<Vec<replace::Edit>>,
    #[serde(default, alias = "old_text", alias = "old_string", alias = "oldString")]
    old_text: Option<String>,
    #[serde(default, alias = "new_text", alias = "new_string", alias = "newString")]
    new_text: Option<String>,
    #[serde(default, alias = "replace_all")]
    replace_all: Option<bool>,
}

fn coerce_bool_field(obj: &mut serde_json::Map<String, serde_json::Value>, key: &str) {
    if let Some(serde_json::Value::String(raw)) = obj.get(key) {
        if raw.trim().eq_ignore_ascii_case("true") {
            obj.insert(key.into(), serde_json::Value::Bool(true));
        } else if raw.trim().eq_ignore_ascii_case("false") {
            obj.insert(key.into(), serde_json::Value::Bool(false));
        }
    }
}

/// Parse Replace mode arguments: accepts flat top-level `{path, oldText, newText, replaceAll?}`,
/// batch `{path, edits: [...]}`, singular `edit` alias, single-object `edits`, and stringified JSON `edits`.
fn parse_replace_args(arguments: &str) -> Result<(String, Vec<replace::Edit>)> {
    let mut value: serde_json::Value =
        serde_json::from_str(arguments).context("invalid edit arguments")?;
    if let Some(obj) = value.as_object_mut() {
        // Normalize singular `edit` alias when `edits` is absent.
        if !obj.contains_key("edits")
            && let Some(single) = obj.remove("edit")
            && !single.is_null()
        {
            obj.insert("edits".into(), single);
        }
        coerce_bool_field(obj, "replaceAll");
        coerce_bool_field(obj, "replace_all");
        match obj.get("edits") {
            Some(serde_json::Value::String(raw)) => {
                match serde_json::from_str::<serde_json::Value>(raw) {
                    Ok(parsed @ serde_json::Value::Array(_)) => {
                        obj.insert("edits".into(), parsed);
                    }
                    Ok(parsed @ serde_json::Value::Object(_)) => {
                        obj.insert("edits".into(), serde_json::Value::Array(vec![parsed]));
                    }
                    _ => anyhow::bail!(
                        "invalid edit arguments: `edits` must be a JSON array of {{oldText, newText}} objects, not a string (check JSON quoting/escaping inside `edits`)"
                    ),
                }
            }
            Some(serde_json::Value::Object(_)) => {
                if let Some(single) = obj.remove("edits") {
                    obj.insert("edits".into(), serde_json::Value::Array(vec![single]));
                }
            }
            _ => {}
        }
        if let Some(serde_json::Value::Array(items)) = obj.get_mut("edits") {
            for item in items {
                if let Some(item_obj) = item.as_object_mut() {
                    coerce_bool_field(item_obj, "replaceAll");
                    coerce_bool_field(item_obj, "replace_all");
                }
            }
        }
    }
    let args: ReplaceArgs = serde_json::from_value(value).context("invalid edit arguments")?;
    let default_replace_all = args.replace_all.unwrap_or(false);
    let edits = match (args.edits, args.old_text, args.new_text) {
        (Some(mut edits), None, None) => {
            if default_replace_all {
                for edit in &mut edits {
                    edit.apply_default_replace_all(true);
                }
            }
            edits
        }
        (None, Some(old_text), Some(new_text)) => {
            vec![replace::Edit::new(old_text, new_text, default_replace_all)]
        }
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            anyhow::bail!(
                "invalid edit arguments: cannot combine `edits` with top-level `oldText`/`newText`"
            );
        }
        (None, Some(_), None) => {
            anyhow::bail!("invalid edit arguments: missing `newText` for top-level `oldText`");
        }
        (None, None, Some(_)) => {
            anyhow::bail!("invalid edit arguments: missing `oldText` for top-level `newText`");
        }
        (None, None, None) => {
            anyhow::bail!("invalid edit arguments: provide `oldText`/`newText` or `edits`");
        }
    };
    Ok((args.path, edits))
}
pub(super) enum Request {
    Hashline(hashline::Patch),
    Replace {
        path: String,
        edits: Vec<replace::Edit>,
    },
}
pub(super) struct Staged {
    pub output: String,
    pub count: usize,
}
impl Request {
    pub(super) fn parse(mode: EditMode, arguments: &str) -> Result<Self> {
        let request = match mode {
            EditMode::Hashline => {
                let args: HashlineArgs = serde_json::from_str(arguments)
                    .context("invalid edit arguments; hashline requires input")?;
                Self::Hashline(hashline::parse(&args.input)?)
            }
            EditMode::Replace => {
                let (path, edits) = parse_replace_args(arguments)?;
                Self::Replace { path, edits }
            }
        };
        ensure!(!request.path().trim().is_empty(), "path must not be blank");
        ensure!(
            !request.path().starts_with(super::LEGACY_PIPE_URI),
            "{}",
            super::LEGACY_PIPE_ERROR
        );
        Ok(request)
    }
    pub(super) fn path(&self) -> &str {
        match self {
            Self::Hashline(p) => &p.path,
            Self::Replace { path, .. } => path,
        }
    }
    pub(super) fn stage(&self, source: &str) -> Result<Staged> {
        let (count, output) = match self {
            Self::Hashline(p) => (p.edits.len(), hashline::apply(source, &p.edits)?),
            Self::Replace { edits, .. } => {
                let (output, count) = replace::apply(source, edits)?;
                (count, output)
            }
        };
        Ok(Staged { output, count })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_rejects_legacy_pipe_uri() {
        let arguments = serde_json::json!({
            "path": "pipe://00112233445566778899aabbccddeeff/1",
            "edits": [{"oldText": "a", "newText": "b"}],
        })
        .to_string();
        let error = Request::parse(EditMode::Replace, &arguments)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(error, super::super::LEGACY_PIPE_ERROR);
    }

    #[test]
    fn replace_accepts_valid_json_string_edits_and_guides_malformed_strings() {
        let valid = serde_json::json!({
            "path": "demo.txt",
            "edits": r#"[{"oldText":"a","newText":"b"}]"#,
        })
        .to_string();
        let parsed = Request::parse(EditMode::Replace, &valid).unwrap();
        assert_eq!(parsed.stage("a\n").unwrap().output, "b\n");

        let malformed = serde_json::json!({
            "path": "demo.txt",
            "edits": r#"[{"newText": "b",oldText": "a"}]"#,
        })
        .to_string();
        let err = Request::parse(EditMode::Replace, &malformed)
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("must be a JSON array") && !err.contains("oldText\": \"a"),
            "{err}"
        );
    }

    #[test]
    fn replace_accepts_common_aliases_single_object_and_replace_all() {
        let alias = serde_json::json!({
            "file_path": "demo.txt",
            "old_string": "foo",
            "new_string": "bar",
            "replace_all": "true",
        })
        .to_string();
        let parsed = Request::parse(EditMode::Replace, &alias).unwrap();
        let staged = parsed.stage("foo\nfoo\n").unwrap();
        assert_eq!(staged.output, "bar\nbar\n");
        assert_eq!(staged.count, 2);

        let singular = serde_json::json!({
            "path": "demo.txt",
            "edit": {"old_text": "foo", "new_text": "baz"},
        })
        .to_string();
        let parsed = Request::parse(EditMode::Replace, &singular).unwrap();
        assert_eq!(parsed.stage("foo\n").unwrap().output, "baz\n");

        let single_obj_edits = serde_json::json!({
            "path": "demo.txt",
            "edits": {"oldText": "foo", "newText": "qux"},
        })
        .to_string();
        let parsed = Request::parse(EditMode::Replace, &single_obj_edits).unwrap();
        assert_eq!(parsed.stage("foo\n").unwrap().output, "qux\n");

        let top_replace_all_inherited = serde_json::json!({
            "path": "demo.txt",
            "replaceAll": true,
            "edits": [
                {"oldText": "a", "newText": "A"},
                {"oldText": "b", "newText": "B", "replaceAll": false}
            ],
        })
        .to_string();
        let parsed = Request::parse(EditMode::Replace, &top_replace_all_inherited).unwrap();
        assert!(parsed.stage("a a b b\n").is_err());
        let staged = parsed.stage("a a b\n").unwrap();
        assert_eq!(staged.output, "A A B\n");
        assert_eq!(staged.count, 3);
    }
}
