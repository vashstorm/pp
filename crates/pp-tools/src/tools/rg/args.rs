//! `rg` 的工具 schema、参数结构与筛选参数构造。
use anyhow::{Result, ensure};
use async_openai::types::chat::FunctionObject;
use pp_config::config::{EditMode, RgSettings};
use serde::Deserialize;
use serde_json::json;

pub(in crate::tools) fn definition(edit_mode: EditMode, sandbox_active: bool) -> FunctionObject {
    let content_mode = match edit_mode {
        EditMode::Hashline => "`content` (line numbers & Hashline `[path#TAG]`)",
        EditMode::Replace => "`content` (line numbers)",
    };
    let sandbox_note = if sandbox_active {
        " Protected credential paths are excluded by sandbox policy."
    } else {
        ""
    };
    let description = format!(
        "In-process ripgrep search over local files or one `artifact://<id>` URI (Rust regex, PCRE2, literal fallback). Case-sensitive by default; includes hidden files and respects ignore files. `output_mode`: {content_mode}, `files_with_matches`, `count`.{sandbox_note}"
    );
    FunctionObject {
        name: "rg".into(),
        description: Some(description),
        parameters: Some(json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regex (or literal with `-F`)."},
                "path": {"type": "string", "description": "`;`-separated files/dirs/globs/`!exclusions`, `file:20-40`, or `artifact://<id>` (default `.`)."},
                "-i": {"type": "boolean", "description": "Case-insensitive search."},
                "-C": {"type": "integer", "minimum": 0, "description": "Context lines before and after each match."},
                "-A": {"type": "integer", "minimum": 0, "description": "Context lines after each match."},
                "-B": {"type": "integer", "minimum": 0, "description": "Context lines before each match."},
                "-F": {"type": "boolean", "description": "Treat pattern as a literal string."},
                "-w": {"type": "boolean", "description": "Match whole words only."},
                "-t": {"type": ["string", "array"], "items": {"type": "string"}, "description": "Filter by ripgrep file type(s), e.g. `rust`."},
                "glob": {"type": ["string", "array"], "items": {"type": "string"}, "description": "Glob override(s) relative to cwd (overrides ignore files)."},
                "max_count": {"type": "integer", "minimum": 0, "description": "Max matches per file (0 = unlimited)."},
                "max_output_tokens": {"type": "integer", "minimum": 1, "description": "Soft token budget for content output."},
                "output_mode": {
                    "type": "string",
                    "enum": ["content", "files_with_matches", "count"],
                    "description": "Result format (default `content`)."
                },
                "gitignore": {"type": "boolean", "description": "Respect ignore files (default true)."},
                "skip": {"type": "integer", "minimum": 0, "description": "Skip first N matching files for pagination."}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })),
        ..Default::default()
    }
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
    #[serde(default)]
    pub(super) max_output_tokens: Option<u64>,
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
