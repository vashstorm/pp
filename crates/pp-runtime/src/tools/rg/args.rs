//! `rg` 的工具 schema、参数结构与筛选参数构造。
use anyhow::{Result, ensure};
use async_openai::types::chat::FunctionObject;
use pp_config::config::RgSettings;
use serde::Deserialize;
use serde_json::json;

pub(in crate::tools) fn definition() -> FunctionObject {
    FunctionObject { name: "rg".into(), description: Some("Search files or one artifact://<numeric-id> URI in process: Rust regex, PCRE2, literal fallback. path: files/dirs/globs/!exclusions separated by ';', file:20-40 ranges; default '.'. Case-sensitive, includes hidden files, respects ignore files. -i inverts case. context/-C set both sides; -A/-B override after/before; otherwise config. Conflicting aliases fail. -F matches literally; -w matches whole words; -t/-g/glob filter the directory walk only (repeatable, !name excludes; -g overrides ignore files, glob is its alias, explicit files always search); max_count caps matches per file (0 finds none). output_mode: content(default), files_with_matches(paths), count(per-file + all-file total; matching lines for single-line patterns, non-overlapping matches for multiline). countComplete=false means partial. Content always includes line numbers and supports context and Hashline snapshots: [path#TAG]; only complete returned lines editable. Other modes preserve snapshots. skip paginates files; bounded output. Artifacts MUST use THIS tool, never bash grep/rg: URIs are not paths; per-call snapshot, no edit anchors; single-line covers saved logs, multiline capped at 4 MiB with warning.".into()), parameters: Some(json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"case":{"type":"boolean"},"-i":{"type":"boolean"},"-n":{"type":"boolean","description":"Optional compatibility flag. Content always includes line numbers."},"context":{"type":"integer","minimum":0},"-C":{"type":"integer","minimum":0},"-A":{"type":"integer","minimum":0},"-B":{"type":"integer","minimum":0},"-F":{"type":"boolean"},"-w":{"type":"boolean"},"-t":{"type":["string","array"],"items":{"type":"string"}},"-g":{"type":["string","array"],"items":{"type":"string"}},"glob":{"type":["string","array"],"items":{"type":"string"},"description":"Alias of -g: override glob or list for the directory walk."},"max_count":{"type":["number","null"]},"output_mode":{"type":"string","enum":["content","files_with_matches","count"]},"gitignore":{"type":"boolean"},"skip":{"type":["number","null"]}},"required":["pattern"],"additionalProperties":false})), ..Default::default() }
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum OutputMode {
    #[default]
    Content,
    FilesWithMatches,
    Count,
}
impl OutputMode {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::FilesWithMatches => "files_with_matches",
            Self::Count => "count",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Args {
    pub(super) pattern: String,
    #[serde(default)]
    pub(super) path: Option<String>,
    #[serde(default, rename = "case")]
    case_sensitive: Option<bool>,
    #[serde(default, rename = "-i")]
    ignore_case: Option<bool>,
    // 正文始终带行号，兼容按 ripgrep 习惯传入的参数。
    #[serde(default, rename = "-n")]
    _line_number: bool,
    #[serde(default)]
    context: Option<usize>,
    #[serde(default, rename = "-C")]
    context_alias: Option<usize>,
    #[serde(default, rename = "-A")]
    after: Option<usize>,
    #[serde(default, rename = "-B")]
    before: Option<usize>,
    #[serde(default)]
    pub(super) output_mode: OutputMode,
    #[serde(default)]
    pub(super) gitignore: Option<bool>,
    #[serde(default)]
    pub(super) skip: Option<f64>,
    #[serde(default, rename = "-F")]
    pub(super) fixed: Option<bool>,
    #[serde(default, rename = "-w")]
    pub(super) word: Option<bool>,
    #[serde(default, rename = "-t")]
    pub(super) types: Option<Many>,
    #[serde(default, rename = "-g")]
    pub(super) globs: Option<Many>,
    // `glob` 是 `-g` 的别名：常见调用方用不带短横线的名字传 override glob。
    #[serde(default)]
    pub(super) glob: Option<Many>,
    #[serde(default)]
    pub(super) max_count: Option<f64>,
}
/// 可重复的 rg 参数：单个字符串或字符串数组。
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum Many {
    One(String),
    Many(Vec<String>),
}
impl Many {
    pub(super) fn into_vec(self) -> Vec<String> {
        match self {
            Self::One(one) => vec![one],
            Self::Many(many) => many,
        }
    }
}
impl Args {
    pub(super) fn settings(&self, defaults: RgSettings) -> Result<RgSettings> {
        if let (Some(context), Some(alias)) = (self.context, self.context_alias) {
            ensure!(context == alias, "context and -C must agree");
        }
        let context = self.context.or(self.context_alias);
        Ok(if self.output_mode == OutputMode::Content {
            RgSettings {
                context_before: self.before.or(context).unwrap_or(defaults.context_before),
                context_after: self.after.or(context).unwrap_or(defaults.context_after),
            }
        } else {
            RgSettings {
                context_before: 0,
                context_after: 0,
            }
        })
    }
    pub(super) fn insensitive(&self) -> Result<bool> {
        if let (Some(case), Some(ignore)) = (self.case_sensitive, self.ignore_case) {
            ensure!(case != ignore, "case and -i have conflicting values");
        }
        Ok(self
            .ignore_case
            .unwrap_or(!self.case_sensitive.unwrap_or(true)))
    }
}
pub(super) fn count(value: Option<f64>, label: &str) -> Result<usize> {
    let value = value.unwrap_or(0.0);
    ensure!(
        value.is_finite() && value >= 0.0 && value <= usize::MAX as f64,
        "{label} must be a non-negative number"
    );
    Ok(value.floor() as usize)
}
pub(super) fn file_types(input: Option<Many>) -> Result<Option<ignore::types::Types>> {
    let Some(input) = input else { return Ok(None) };
    let names = input.into_vec();
    if names.is_empty() {
        return Ok(None);
    }
    let mut builder = ignore::types::TypesBuilder::new();
    builder.add_defaults();
    let known: Vec<String> = builder
        .definitions()
        .into_iter()
        .map(|definition| definition.name().to_owned())
        .collect();
    for name in &names {
        let (negate, bare) = match name.strip_prefix('!') {
            Some(bare) => (true, bare),
            None => (false, name.as_str()),
        };
        ensure!(
            bare == "all" || known.iter().any(|known| known == bare),
            "invalid rg type: {bare}"
        );
        if negate {
            builder.negate(bare);
        } else {
            builder.select(bare);
        }
    }
    Ok(Some(builder.build().map_err(|error| {
        anyhow::anyhow!("invalid rg type: {error}")
    })?))
}
