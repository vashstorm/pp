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
    path: String,
    #[serde(default)]
    edits: Option<Vec<replace::Edit>>,
    #[serde(default)]
    old_text: Option<String>,
    #[serde(default)]
    new_text: Option<String>,
}

/// Parse Replace mode arguments: accepts both the flat top-level `{path, oldText, newText}` and the batch `{path, edits: [...]}`;
/// when the model wraps a valid JSON array in a string for `edits`, restore it safely and give a concise hint for a broken string.
fn parse_replace_args(arguments: &str) -> Result<(String, Vec<replace::Edit>)> {
    let mut value: serde_json::Value =
        serde_json::from_str(arguments).context("invalid edit arguments")?;
    if let Some(obj) = value.as_object_mut()
        && let Some(serde_json::Value::String(raw)) = obj.get("edits")
    {
        match serde_json::from_str::<serde_json::Value>(raw) {
            Ok(parsed @ serde_json::Value::Array(_)) => {
                obj.insert("edits".into(), parsed);
            }
            _ => anyhow::bail!(
                "invalid edit arguments: `edits` must be a JSON array of {{oldText, newText}} objects, not a string (check JSON quoting/escaping inside `edits`)"
            ),
        }
    }
    let args: ReplaceArgs = serde_json::from_value(value).context("invalid edit arguments")?;
    let edits = match (args.edits, args.old_text, args.new_text) {
        (Some(edits), None, None) => edits,
        (None, Some(old_text), Some(new_text)) => vec![replace::Edit::new(old_text, new_text)],
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
            Self::Replace { edits, .. } => (edits.len(), replace::apply(source, edits)?),
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
}
