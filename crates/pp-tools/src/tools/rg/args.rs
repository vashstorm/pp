//! `rg` 的工具 schema、参数结构、宽容归一化与筛选参数构造。
use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use pp_config::config::{EditMode, RgSettings};
use serde::Deserialize;
use serde_json::{Map, Value, json};

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
                "skip": {"type": "integer", "minimum": 0, "description": "Skip first N matching files for pagination."},
                "head_limit": {"type": "integer", "minimum": 0, "description": "Max output lines (`content`) or entries (other modes); 0 = unlimited."}
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
    #[serde(default)]
    pub(super) head_limit: Option<f64>,
    /// 归一化时丢弃的未知参数名，结果里以提示返回给模型。
    #[serde(skip)]
    pub(super) ignored: Vec<String>,
}

/// 规范参数名；不在此列表（也不是别名）的键会被忽略并提示。
const KNOWN_KEYS: &[&str] = &[
    "pattern",
    "path",
    "case",
    "-i",
    "-n",
    "context",
    "-C",
    "-A",
    "-B",
    "output_mode",
    "gitignore",
    "skip",
    "-F",
    "-w",
    "-t",
    "-g",
    "glob",
    "max_count",
    "max_output_tokens",
    "head_limit",
];
/// 模型常按 ripgrep CLI 或其他 Grep 工具的习惯起名：别名 → 规范名。
const KEY_ALIASES: &[(&str, &str)] = &[
    ("A", "-A"),
    ("B", "-B"),
    ("C", "-C"),
    ("i", "-i"),
    ("n", "-n"),
    ("F", "-F"),
    ("w", "-w"),
    ("t", "-t"),
    ("g", "-g"),
    ("type", "-t"),
    ("ignore_case", "-i"),
    ("case_insensitive", "-i"),
    ("fixed_strings", "-F"),
    ("mode", "output_mode"),
    ("offset", "skip"),
    ("max_tokens", "max_output_tokens"),
    ("head", "head_limit"),
    ("limit", "head_limit"),
    ("max_lines", "head_limit"),
    ("max_results", "head_limit"),
];
/// 布尔开关形式的输出模式：`-c` → count，`-l` → files_with_matches。
const MODE_FLAGS: &[(&str, &str)] = &[("-c", "count"), ("-l", "files_with_matches")];
const BOOL_KEYS: &[&str] = &["case", "-i", "-n", "-F", "-w", "gitignore"];
const NUMBER_KEYS: &[&str] = &[
    "context",
    "-C",
    "-A",
    "-B",
    "skip",
    "max_count",
    "max_output_tokens",
    "head_limit",
];

/// 把字符串形式的布尔（`"true"`/`"false"`）还原为 JSON 布尔，其余值原样返回。
fn coerce_bool(value: Value) -> Value {
    match &value {
        Value::String(text) if text.trim().eq_ignore_ascii_case("true") => Value::Bool(true),
        Value::String(text) if text.trim().eq_ignore_ascii_case("false") => Value::Bool(false),
        _ => value,
    }
}
/// 把数字字符串（`"40"`）还原为 JSON 数字，其余值原样返回，交给后续类型校验报错。
fn coerce_number(value: Value) -> Value {
    match &value {
        Value::String(text) => serde_json::from_str::<serde_json::Number>(text.trim())
            .map(Value::Number)
            .unwrap_or(value),
        _ => value,
    }
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
    /// 执行与权限预检共用的唯一解析入口：先归一化模型常见的参数偏差
    /// （null、别名、字符串化的布尔/数字、`-c`/`-l` 模式开关、未知键），再严格反序列化。
    pub(super) fn parse(arguments: &str) -> Result<Self> {
        let input: Value = serde_json::from_str(arguments).context("invalid rg arguments")?;
        let Value::Object(input) = input else {
            bail!("invalid rg arguments: expected a JSON object");
        };
        let mut map = Map::new();
        let mut ignored = Vec::new();
        let mut modes = Vec::new();
        for (key, value) in input {
            if value.is_null() {
                continue;
            }
            if let Some(&(flag, mode)) = MODE_FLAGS.iter().find(|(flag, _)| *flag == key) {
                match coerce_bool(value) {
                    Value::Bool(true) => modes.push((flag, mode)),
                    Value::Bool(false) => {}
                    _ => bail!("invalid rg arguments: `{flag}` must be a boolean"),
                }
                continue;
            }
            let canonical = KEY_ALIASES
                .iter()
                .find(|(alias, _)| *alias == key)
                .map_or(key.as_str(), |(_, canonical)| canonical);
            let Some(&canonical) = KNOWN_KEYS.iter().find(|known| **known == canonical) else {
                ignored.push(key);
                continue;
            };
            let value = if BOOL_KEYS.contains(&canonical) {
                coerce_bool(value)
            } else if NUMBER_KEYS.contains(&canonical) {
                coerce_number(value)
            } else {
                value
            };
            if let Some(previous) = map.get(canonical) {
                ensure!(
                    *previous == value,
                    "invalid rg arguments: `{key}` conflicts with `{canonical}`"
                );
                continue;
            }
            map.insert(canonical.to_owned(), value);
        }
        for (flag, mode) in modes {
            match map.get("output_mode") {
                Some(existing) => ensure!(
                    existing == mode,
                    "invalid rg arguments: `{flag}` conflicts with output_mode"
                ),
                None => {
                    map.insert("output_mode".into(), Value::from(mode));
                }
            }
        }
        let mut args: Self =
            serde_json::from_value(Value::Object(map)).context("invalid rg arguments")?;
        ignored.sort_unstable();
        args.ignored = ignored;
        Ok(args)
    }
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
