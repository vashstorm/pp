//! 两种编辑模式的纯暂存入口；磁盘提交由 Tools 负责。
use super::{hashline, replace};
use crate::config::EditMode;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HashlineArgs {
    input: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceArgs {
    path: String,
    edits: Vec<replace::Edit>,
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
                let args: ReplaceArgs =
                    serde_json::from_str(arguments).context("invalid edit arguments")?;
                Self::Replace {
                    path: args.path,
                    edits: args.edits,
                }
            }
        };
        ensure!(!request.path().trim().is_empty(), "path must not be blank");
        ensure!(
            !super::read::is_pipe_uri(request.path()),
            "pipe streams are read-only"
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
    fn pipeline_edit_rejects_pipe_streams_as_read_only() {
        let arguments = serde_json::json!({
            "path": "pipe://00112233445566778899aabbccddeeff/1",
            "edits": [{"oldText": "a", "newText": "b"}],
        })
        .to_string();
        let error = Request::parse(EditMode::Replace, &arguments)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(error, "pipe streams are read-only");
    }
}
