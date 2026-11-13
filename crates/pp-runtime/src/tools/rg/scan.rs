//! `rg` 单文件搜索：收集器、受限读取与结果折叠。
use anyhow::{Context, Result, ensure};
use grep_matcher::Matcher;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use pp_config::config::RgSettings;
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    sync::Arc,
    time::Instant,
};

use super::args::OutputMode;
use super::paths::Target;
use super::{MAX_FILE, MAX_LINE};

#[derive(Clone)]
pub(super) struct Row {
    pub(super) number: usize,
    pub(super) text: String,
    pub(super) matched: bool,
    pub(super) truncated: bool,
    pub(super) column: Option<usize>,
}
pub(super) struct Found {
    pub(super) target: Target,
    pub(super) rows: Vec<Row>,
    pub(super) matches: usize,
    pub(super) capped: bool,
    pub(super) bytes: Option<Arc<str>>,
}

pub(super) fn check(token: &CancellationToken, deadline: Instant) -> Result<()> {
    cancellation::check(token)?;
    ensure!(
        Instant::now() < deadline,
        "rg timed out after 30s; narrow paths or pattern"
    );
    Ok(())
}

struct Collector<'a, M> {
    matcher: &'a M,
    rows: BTreeMap<usize, Row>,
    matches: usize,
    limit: usize,
    output_mode: OutputMode,
    multiline: bool,
    ranges: &'a [(usize, usize)],
    token: &'a CancellationToken,
    deadline: Instant,
    max_count: Option<usize>,
}
impl<M: Matcher> Collector<'_, M> {
    fn capped(&self) -> bool {
        self.max_count.is_some_and(|max| self.matches >= max)
    }
    fn row(&mut self, bytes: &[u8], number: usize, matched: bool) {
        let raw = String::from_utf8_lossy(bytes);
        let raw = raw.trim_end_matches(['\n', '\r']);
        let mut offset = if matched {
            self.matcher
                .find(raw.as_bytes())
                .ok()
                .flatten()
                .map(|m| m.start())
                .unwrap_or(0)
        } else {
            0
        };
        while offset > 0 && !raw.is_char_boundary(offset) {
            offset -= 1;
        }
        let truncated = raw.len() > MAX_LINE;
        let (text, column) = if truncated {
            let mut start = if matched && offset > MAX_LINE.saturating_sub(3) {
                offset.saturating_sub((MAX_LINE - 6) / 2)
            } else {
                0
            };
            while start > 0 && !raw.is_char_boundary(start) {
                start -= 1;
            }
            let prefix = if start > 0 { "..." } else { "" };
            let mut end = (start + MAX_LINE - prefix.len() - 3).min(raw.len());
            while end > start && !raw.is_char_boundary(end) {
                end -= 1;
            }
            let suffix = if end < raw.len() { "..." } else { "" };
            (
                format!("{prefix}{}{suffix}", &raw[start..end]),
                matched.then(|| raw[..offset.min(raw.len())].chars().count() + 1),
            )
        } else {
            (raw.to_owned(), None)
        };
        self.rows
            .entry(number)
            .and_modify(|r| {
                if matched {
                    r.matched = true;
                    r.text = text.clone();
                    r.truncated = truncated;
                    r.column = column;
                }
            })
            .or_insert(Row {
                number,
                text,
                matched,
                truncated,
                column,
            });
    }
}
impl<M: Matcher> Sink for Collector<'_, M> {
    type Error = io::Error;
    fn matched(&mut self, _: &Searcher, m: &SinkMatch<'_>) -> io::Result<bool> {
        if self.token.is_cancelled() || Instant::now() >= self.deadline {
            return Ok(false);
        }
        // 停在 max_count 之后：上一条匹配的 after-context 已在此前送达。
        if self.capped() {
            return Ok(false);
        }
        let n = m.line_number().unwrap_or(1) as usize;
        if self.multiline && self.output_mode != OutputMode::Content {
            // 多行回调可能合并多个匹配；按每次匹配的起始行判断范围。
            let range = m.bytes_range_in_buffer();
            let mut offset = range.start;
            let mut number = n;
            self.matcher
                .find_iter_at(m.buffer(), range.start, |matched| {
                    if matched.start() > range.end
                        || matched.end() > range.end
                        || self.token.is_cancelled()
                        || Instant::now() >= self.deadline
                    {
                        return false;
                    }
                    number += m.buffer()[offset..matched.start()]
                        .iter()
                        .filter(|b| **b == b'\n')
                        .count();
                    offset = matched.start();
                    if self.ranges.is_empty()
                        || self
                            .ranges
                            .iter()
                            .any(|(a, b)| number >= *a && number <= *b)
                    {
                        self.matches += 1;
                    }
                    !self.capped()
                        && (self.output_mode != OutputMode::FilesWithMatches || self.matches == 0)
                })
                .map_err(|_| io::Error::other("rg matcher failed"))?;
            return Ok(!self.capped()
                && (self.output_mode != OutputMode::FilesWithMatches || self.matches == 0));
        }
        if !self.ranges.is_empty() && !self.ranges.iter().any(|(a, b)| n >= *a && n <= *b) {
            return Ok(true);
        }
        if self.output_mode == OutputMode::FilesWithMatches {
            self.matches = 1;
            return Ok(false);
        }
        if self.output_mode == OutputMode::Count {
            self.matches += m.lines().count();
            if let Some(max) = self.max_count {
                self.matches = self.matches.min(max);
            }
            return Ok(!self.capped());
        }
        self.matches += 1;
        if self.matches > self.limit {
            return Ok(false);
        }
        for (i, line) in m.bytes().split_inclusive(|b| *b == b'\n').enumerate() {
            self.row(line, n + i, true);
        }
        Ok(true)
    }
    fn context(&mut self, _: &Searcher, c: &SinkContext<'_>) -> io::Result<bool> {
        if self.token.is_cancelled() || Instant::now() >= self.deadline {
            return Ok(false);
        }
        let n = c.line_number().unwrap_or(1) as usize;
        if self.ranges.is_empty() || self.ranges.iter().any(|(a, b)| n >= *a && n <= *b) {
            self.row(c.bytes(), n, false);
        }
        Ok(true)
    }
    fn binary_data(&mut self, _: &Searcher, _: u64) -> io::Result<bool> {
        if self.output_mode == OutputMode::Count {
            return Err(io::Error::other("binary file not fully searched"));
        }
        Ok(true)
    }
}
struct CheckedReader<'a> {
    file: File,
    token: &'a CancellationToken,
    deadline: Instant,
}
impl Read for CheckedReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.token.is_cancelled() || Instant::now() >= self.deadline {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "rg interrupted"));
        }
        self.file.read(buf)
    }
}
#[allow(clippy::too_many_arguments)]
pub(super) fn run_file<M: Matcher>(
    matcher: &M,
    target: Target,
    settings: RgSettings,
    multiline: bool,
    cap: usize,
    max_count: Option<usize>,
    output_mode: OutputMode,
    capture_snapshot: bool,
    token: &CancellationToken,
    deadline: Instant,
) -> Result<Option<Found>>
where
    M::Error: std::fmt::Display,
{
    check(token, deadline)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc_nofollow())
        .open(&target.path)
        .with_context(|| format!("opening {}", target.display))?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "rg requires a regular file");
    let mut collector = Collector {
        matcher,
        rows: BTreeMap::new(),
        matches: 0,
        limit: cap,
        max_count,
        output_mode,
        multiline,
        ranges: &target.ranges,
        token,
        deadline,
    };
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .multi_line(multiline)
        .binary_detection(BinaryDetection::quit(b'\0'))
        .before_context(settings.context_before)
        .after_context(settings.context_after)
        .heap_limit(Some(64 * 1024 * 1024))
        .build();
    let mut bytes = None;
    let found = if metadata.len() <= MAX_FILE || !target.explicit || multiline {
        let mut buffer = Vec::new();
        CheckedReader {
            file,
            token,
            deadline,
        }
        .take(MAX_FILE)
        .read_to_end(&mut buffer)?;
        if capture_snapshot
            && metadata.len() <= MAX_FILE
            && let Ok(s) = std::str::from_utf8(&buffer)
        {
            bytes = Some(Arc::<str>::from(s));
        }
        searcher.search_slice(matcher, &buffer, &mut collector)
    } else {
        searcher.search_reader(
            matcher,
            CheckedReader {
                file,
                token,
                deadline,
            },
            &mut collector,
        )
    };
    check(token, deadline)?;
    found.map_err(|e| anyhow::anyhow!("rg search failed: {e}"))?;
    if collector.matches == 0 {
        return Ok(None);
    }
    let matched: Vec<usize> = collector
        .rows
        .values()
        .filter(|r| r.matched)
        .map(|r| r.number)
        .collect();
    let matches = collector.matches;
    let rows = collector
        .rows
        .into_values()
        .filter(|r| {
            matched.iter().any(|n| {
                r.number >= n.saturating_sub(settings.context_before)
                    && r.number <= n.saturating_add(settings.context_after)
            })
        })
        .collect();
    Ok(Some(Found {
        target,
        rows,
        matches: if output_mode == OutputMode::Content {
            matches.min(cap)
        } else {
            matches
        },
        capped: output_mode == OutputMode::Content && matches > cap,
        bytes,
    }))
}
fn libc_nofollow() -> i32 {
    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32
}
