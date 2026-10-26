use anyhow::{Context, Result, ensure};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Range {
    Lines(usize, Option<usize>),
    Tail(usize),
}

#[derive(Debug, Default)]
pub(super) struct Target {
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
    pub(super) fn is_url(&self) -> bool {
        self.source.starts_with("http://") || self.source.starts_with("https://")
    }

    pub(super) fn validate_image(&self) -> Result<()> {
        ensure!(
            self.ranges.is_empty() && !self.raw,
            "image reads do not accept selectors"
        );
        Ok(())
    }

    pub(super) fn parse(path: &str, cwd: &Path) -> Result<Self> {
        ensure!(!path.trim().is_empty(), "path must not be blank");
        let web = path.starts_with("http://") || path.starts_with("https://");
        // POSIX 中包含冒号的真实文件名优先，不能误删合法路径尾部。
        if !web && cwd.join(path).symlink_metadata().is_ok() {
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
