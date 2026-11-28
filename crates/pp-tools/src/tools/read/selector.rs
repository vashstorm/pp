use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Range {
    Lines(usize, Option<usize>),
    Tail(usize),
}

#[derive(Debug, Default)]
pub(in crate::tools) struct Target {
    pub(super) source: String,
    ranges: Vec<Range>,
    pub(super) raw: bool,
    pub(super) explicit: bool,
}

fn positive(value: &str) -> Result<usize> {
    let value = value.strip_prefix('L').unwrap_or(value);
    let n = value.parse::<usize>().ok().filter(|n| *n > 0);
    n.context("read line numbers and counts must be positive integers")
}

fn range(value: &str, raw: bool, multiple: bool) -> Result<Range> {
    if let Some(tail) = value.strip_prefix('-') {
        return Ok(Range::Tail(positive(tail)?));
    }
    if let Some((start, count)) = value.split_once('+') {
        let start = positive(start)?;
        let end = start
            .checked_add(positive(count)? - 1)
            .context("read range overflow")?;
        return Ok(Range::Lines(start, Some(end)));
    }
    if let Some((start, end)) = value.split_once('-').or_else(|| value.split_once("..")) {
        let start = positive(start)?;
        let end = if end.is_empty() {
            None
        } else {
            Some(positive(end)?)
        };
        ensure!(
            end.is_none_or(|end| end >= start),
            "read range end precedes start"
        );
        return Ok(Range::Lines(start, end));
    }
    let start = positive(value)?;
    Ok(Range::Lines(start, (raw || multiple).then_some(start)))
}

fn looks_like_range(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || b"L-+.,:".contains(&b))
}

fn url_parts(source: &str) -> (&str, &str) {
    if source.starts_with("http://") || source.starts_with("https://") {
        source
            .find(['?', '#'])
            .map_or((source, ""), |i| source.split_at(i))
    } else {
        (source, "")
    }
}

fn suffixed(source: &str, suffix: &str) -> String {
    let (head, rest) = url_parts(source);
    format!("{head}{suffix}{rest}")
}

impl Target {
    pub(super) fn literal(source: String) -> Self {
        Self {
            source,
            ..Self::default()
        }
    }

    pub(in crate::tools) fn is_url(&self) -> bool {
        self.source.starts_with("http://") || self.source.starts_with("https://")
    }

    pub(in crate::tools) fn local_path(&self, cwd: &Path) -> Option<PathBuf> {
        let resource = self.is_url()
            || [
                "skill://",
                "artifact://",
                super::super::LEGACY_PIPE_URI,
                "agent://",
            ]
            .iter()
            .any(|prefix| self.source.starts_with(prefix));
        if resource {
            return None;
        }
        Some(expand_local_path(&self.source, cwd))
    }

    pub(super) fn validate_image(&self) -> Result<()> {
        ensure!(
            self.ranges.is_empty() && !self.raw,
            "image reads do not accept selectors"
        );
        Ok(())
    }

    pub(in crate::tools) fn parse(path: &str, cwd: &Path) -> Result<Self> {
        ensure!(!path.trim().is_empty(), "path must not be blank");
        let web = path.starts_with("http://") || path.starts_with("https://");
        // On POSIX a real file name containing a colon wins, so a valid path tail is never stripped by mistake.
        if !web
            && !path.starts_with("skill://")
            && !path.starts_with("artifact://")
            && !path.starts_with(super::super::LEGACY_PIPE_URI)
            && !path.starts_with("agent://")
            && cwd.join(path).symlink_metadata().is_ok()
        {
            return Ok(Self {
                source: path.into(),
                ..Self::default()
            });
        }
        let (head, rest) = url_parts(path);
        let mut source = head;
        let mut raw = false;
        if let Some(prefix) = source.strip_suffix(":raw") {
            source = prefix;
            raw = true;
        }
        let mut selected = None;
        if let Some((prefix, suffix)) = source.rsplit_once(':')
            && looks_like_range(suffix)
            && (!web
                || prefix
                    .split_once("://")
                    .is_some_and(|(_, v)| v.contains('/')))
        {
            selected = Some(suffix);
            source = prefix;
        }
        if let Some(prefix) = source.strip_suffix(":raw") {
            source = prefix;
            raw = true;
        }
        let ranges = match selected {
            Some(value) => {
                let chunks: Vec<_> = value.split(',').collect();
                ensure!(chunks.len() <= 128, "too many read ranges");
                chunks
                    .iter()
                    .map(|v| range(v.trim_start_matches(':'), raw, chunks.len() > 1))
                    .collect::<Result<_>>()?
            }
            None => Vec::new(),
        };
        ensure!(!source.is_empty(), "read source must not be empty");
        Ok(Self {
            source: format!("{source}{rest}"),
            ranges,
            raw,
            explicit: selected.is_some() || raw,
        })
    }

    pub(super) fn resolved_ranges(
        &self,
        total: impl FnOnce() -> Result<usize>,
    ) -> Result<Vec<(usize, usize)>> {
        let lines = if self.ranges.iter().any(|v| matches!(v, Range::Tail(_))) {
            total()?
        } else {
            0
        };
        let mut ranges: Vec<_> = self
            .ranges
            .iter()
            .filter_map(|r| match *r {
                Range::Lines(start, end) => Some((start, end.unwrap_or(usize::MAX))),
                Range::Tail(count) if lines > 0 => {
                    Some((lines.saturating_sub(count).saturating_add(1), lines))
                }
                Range::Tail(_) => None,
            })
            .collect();
        if self.ranges.is_empty() {
            ranges.push((1, usize::MAX));
        }
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (start, end) in ranges {
            if let Some(last) = merged.last_mut()
                && start <= last.1.saturating_add(1)
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        }
        Ok(merged)
    }

    pub(super) fn dedup_selector(&self) -> String {
        if self.ranges.is_empty() {
            String::new()
        } else {
            format!("{:?}", self.ranges)
        }
    }

    pub(super) fn next_path(&self, ranges: &[(usize, usize)]) -> String {
        let selection = ranges
            .iter()
            .map(|(start, end)| {
                if *end == usize::MAX {
                    format!("{start}-")
                } else {
                    format!("{start}-{end}")
                }
            })
            .collect::<Vec<_>>()
            .join(",");
        suffixed(
            &self.source,
            &format!(":{selection}{}", if self.raw { ":raw" } else { "" }),
        )
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    ranges: Option<String>,
}

fn coerce_positive_int(value: Value) -> Value {
    match &value {
        Value::String(text) => serde_json::from_str::<serde_json::Number>(text.trim())
            .map(Value::Number)
            .unwrap_or(value),
        _ => value,
    }
}

/// Parse and normalize `read` tool arguments (shared by the permission pre-check and the actual read):
/// accepts the `offset`/`limit`/`ranges` the model habitually sends (plus the `path_ranges` alias, including numeric strings and `null`)
/// and folds them into a uniform `path:<selector>` form; other unknown fields, conflicts and non-positive integers are still rejected strictly.
pub(in crate::tools) fn parse_args(arguments: &str, cwd: &Path) -> Result<String> {
    let input: Value = serde_json::from_str(arguments).context("invalid read arguments")?;
    let Value::Object(input) = input else {
        bail!("invalid read arguments: expected a JSON object");
    };
    let mut map = Map::new();
    for (key, value) in input {
        if value.is_null() {
            continue;
        }
        let canonical = match key.as_str() {
            "path_ranges" => "ranges",
            other => other,
        };
        let value = if matches!(canonical, "offset" | "limit") {
            coerce_positive_int(value)
        } else {
            value
        };
        if let Some(previous) = map.get(canonical) {
            ensure!(
                *previous == value,
                "invalid read arguments: `{key}` conflicts with `{canonical}`"
            );
            continue;
        }
        map.insert(canonical.to_owned(), value);
    }
    let args: RawReadArgs =
        serde_json::from_value(Value::Object(map)).context("invalid read arguments")?;
    if let Some(offset) = args.offset {
        ensure!(offset > 0, "invalid read arguments: offset must be >= 1");
    }
    if let Some(limit) = args.limit {
        ensure!(limit > 0, "invalid read arguments: limit must be >= 1");
    }
    if args.offset.is_none() && args.limit.is_none() && args.ranges.is_none() {
        return Ok(args.path);
    }
    ensure!(
        args.ranges.is_none() || (args.offset.is_none() && args.limit.is_none()),
        "invalid read arguments: cannot combine ranges with offset/limit"
    );
    let base = Target::parse(&args.path, cwd)?;
    ensure!(
        base.ranges.is_empty(),
        "invalid read arguments: path already contains a line selector; omit offset/limit/ranges"
    );
    let selector = if let Some(ranges) = args.ranges {
        let trimmed = ranges.trim().trim_start_matches(':');
        ensure!(
            !trimmed.is_empty(),
            "invalid read arguments: ranges must not be empty"
        );
        trimmed.to_owned()
    } else {
        match (args.offset, args.limit) {
            (Some(offset), Some(limit)) => format!("{offset}+{limit}"),
            (Some(offset), None) => offset.to_string(),
            (None, Some(limit)) => format!("1+{limit}"),
            (None, None) => unreachable!(),
        }
    };
    let raw_suffix = if base.raw { ":raw" } else { "" };
    let normalized = suffixed(&base.source, &format!(":{selector}{raw_suffix}"));
    // Validate the folded selector immediately, so an invalid `ranges` cannot slip through the pre-check as a literal file name.
    let parsed = Target::parse(&normalized, cwd).context("invalid read arguments")?;
    ensure!(
        !parsed.ranges.is_empty(),
        "invalid read arguments: invalid line range selector"
    );
    Ok(normalized)
}

fn expand_local_path(raw: &str, cwd: &Path) -> PathBuf {
    if !cwd.join(raw).exists()
        && let Some(home) = std::env::var_os("HOME").filter(|v| !v.is_empty())
    {
        if raw == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = raw.strip_prefix("~/") {
            return PathBuf::from(home).join(rest);
        }
    }
    cwd.join(raw)
}
