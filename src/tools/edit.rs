//! 两种编辑模式的纯暂存入口；磁盘提交由 Tools 负责。
use super::{diff, hashline, replace};
use crate::{config::EditMode, events::FileDiff};
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
    pub diff: FileDiff,
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
        let diff = diff::build(Some(source), &output);
        Ok(Staged {
            output,
            count,
            diff,
        })
    }
}
