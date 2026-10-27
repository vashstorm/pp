//! 本地内容搜索；行为参考 Oh My Pi d4279540（MIT License）。
use super::{hashline, save};
use crate::{
    cancellation::{self, CancellationToken},
    config::{EditMode, GrepSettings},
};
use anyhow::{Context, Result, ensure};
use async_openai::types::chat::FunctionObject;
use globset::{GlobBuilder, GlobMatcher};
use grep_matcher::Matcher;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const FILE_LIMIT: usize = 20;
const MULTI_LIMIT: usize = 20;
const SINGLE_LIMIT: usize = 200;
const TOTAL_LIMIT: usize = 2000;
const MAX_FILE: u64 = 4 * 1024 * 1024;
const MAX_OUTPUT: usize = 50 * 1024;
const MAX_LINE: usize = 512;

pub(super) fn definition() -> FunctionObject {
    FunctionObject { name: "grep".into(), description: Some("Search local files with regex: Rust regex then PCRE2, with literal fallback. path accepts semicolon-separated files, directories, globs and exclusions (!glob); default '.'. File paths may include line ranges such as src/main.rs:20-40. Defaults to case-sensitive, includes hidden files and respects ignore files. skip paginates matching files. Results are bounded and indicate truncation; narrow the path or pattern for more. Hashline file headings contain path and TAG: use [path#TAG] for edit; only complete returned lines are editable. No shell or subprocess.".into()), parameters: Some(json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"case":{"type":"boolean"},"gitignore":{"type":"boolean"},"skip":{"type":["number","null"]}},"required":["pattern"],"additionalProperties":false})), ..Default::default() }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default, rename = "case")]
    case_sensitive: Option<bool>,
    #[serde(default)]
    gitignore: Option<bool>,
    #[serde(default)]
    skip: Option<f64>,
}
struct Target {
    path: PathBuf,
    display: String,
    ranges: Vec<(usize, usize)>,
    explicit: bool,
}
#[derive(Clone)]
struct Row {
    number: usize,
    text: String,
    matched: bool,
    truncated: bool,
    column: Option<usize>,
}
struct Found {
    target: Target,
    rows: Vec<Row>,
    matches: usize,
    capped: bool,
    bytes: Option<Arc<str>>,
}

fn add_target(targets: &mut BTreeMap<PathBuf, Target>, mut target: Target) {
    if let Some(existing) = targets.get_mut(&target.path) {
        if existing.ranges.is_empty() || target.ranges.is_empty() {
            existing.ranges.clear();
        } else {
            existing.ranges.append(&mut target.ranges);
        }
        existing.explicit |= target.explicit;
    } else {
        targets.insert(target.path.clone(), target);
    }
}

enum Compiled {
    Rust(grep_regex::RegexMatcher),
    Pcre(grep_pcre2::RegexMatcher),
}
fn compile(pattern: &str, insensitive: bool, multiline: bool) -> Result<Compiled> {
    let sanitized = sanitize_braces(pattern);
    let rust = |p: &str| {
        let mut b = grep_regex::RegexMatcherBuilder::new();
        b.case_insensitive(insensitive).multi_line(multiline);
        b.build(p)
    };
    let pcre = |p: &str| {
        let mut b = grep_pcre2::RegexMatcherBuilder::new();
        b.caseless(insensitive)
            .multi_line(multiline)
            .utf(true)
            .ucp(true)
            .jit_if_available(true);
        b.build(p)
    };
    match rust(&sanitized) {
        Ok(m) => return Ok(Compiled::Rust(m)),
        Err(e) => {
            if let Ok(m) = pcre(&sanitized) {
                return Ok(Compiled::Pcre(m));
            }
            if e.to_string().contains("unclosed group") || e.to_string().contains("unopened group")
            {
                let fixed = escape_parens(&sanitized);
                if let Ok(m) = rust(&fixed) {
                    return Ok(Compiled::Rust(m));
                }
                if let Ok(m) = pcre(&fixed) {
                    return Ok(Compiled::Pcre(m));
                }
            }
        }
    }
    Ok(Compiled::Rust(
        rust(&regex::escape(pattern)).context("invalid grep pattern")?,
    ))
}
fn sanitize_braces(p: &str) -> String {
    let mut out = String::new();
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(c);
            if let Some(n) = chars.next() {
                out.push(n);
                if matches!(n, 'p' | 'P' | 'x' | 'u') && chars.peek() == Some(&'{') {
                    for x in chars.by_ref() {
                        out.push(x);
                        if x == '}' {
                            break;
                        }
                    }
                }
            }
            continue;
        }
        if c == '{' {
            let suffix: String = chars.clone().collect();
            let end = suffix.find('}');
            let valid = end.is_some_and(|i| {
                let q = &suffix[..i];
                let mut parts = q.split(',');
                let a = parts.next().unwrap_or("");
                !a.is_empty()
                    && a.bytes().all(|x| x.is_ascii_digit())
                    && parts
                        .next()
                        .is_none_or(|b| b.bytes().all(|x| x.is_ascii_digit()))
                    && parts.next().is_none()
            });
            if !valid {
                out.push('\\');
            }
        }
        if c == '}' {
            // 有效量词的右括号已经由左括号处复制。
            out.push_str("\\}");
            continue;
        }
        if c == '{' {
            let suffix: String = chars.clone().collect();
            if let Some(i) = suffix.find('}') {
                let q = &suffix[..i];
                let mut parts = q.split(',');
                let a = parts.next().unwrap_or("");
                if !a.is_empty()
                    && a.bytes().all(|x| x.is_ascii_digit())
                    && parts
                        .next()
                        .is_none_or(|b| b.bytes().all(|x| x.is_ascii_digit()))
                    && parts.next().is_none()
                {
                    out.push('{');
                    for _ in 0..=i {
                        if let Some(ch) = chars.next() {
                            out.push(ch);
                        }
                    }
                    continue;
                }
            }
        }
        out.push(c);
    }
    out
}
fn escape_parens(p: &str) -> String {
    let mut out = String::new();
    let mut chars = p.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(c);
            if let Some(n) = chars.next() {
                out.push(n);
            }
        } else {
            if matches!(c, '(' | ')') {
                out.push('\\');
            }
            out.push(c);
        }
    }
    out
}
fn check(token: &CancellationToken, deadline: Instant) -> Result<()> {
    cancellation::check(token)?;
    ensure!(
        Instant::now() < deadline,
        "Grep timed out after 30s; narrow paths or pattern"
    );
    Ok(())
}
fn glob(pattern: &str) -> Result<GlobMatcher> {
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .context("invalid grep glob")?
        .compile_matcher())
}
fn glob_path(path: &str) -> bool {
    path.contains(['*', '?', '[', '{'])
}
fn split_top_level(input: &str, delimiter: u8) -> Vec<&str> {
    let bytes = input.as_bytes();
    let mut result = Vec::new();
    let (mut start, mut index, mut depth) = (0, 0, 0usize);
    while index < bytes.len() {
        match bytes[index] {
            b'\\' if index + 1 < bytes.len() => {
                index += 2;
                continue;
            }
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            c if c == delimiter && depth == 0 => {
                result.push(&input[start..index]);
                start = index + 1;
            }
            _ => {}
        }
        index += 1;
    }
    result.push(&input[start..]);
    result
}
fn reject_symlink_components(path: &Path) -> Result<()> {
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part.as_os_str());
        if let Ok(metadata) = fs::symlink_metadata(&current) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "symlink search paths are not supported"
            );
        }
    }
    Ok(())
}
fn parse_paths(
    cwd: &Path,
    input: &str,
    respect: bool,
    token: &CancellationToken,
    deadline: Instant,
) -> Result<(Vec<Target>, bool, Vec<String>)> {
    let input = if input.trim().is_empty() { "." } else { input };
    let normalized = if cwd.join(input).exists() {
        input.to_owned()
    } else {
        input
            .trim()
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(input.trim())
            .to_owned()
    };
    let input = normalized.as_str();
    ensure!(!input.contains("://"), "grep only supports local paths");
    let semicolon = split_top_level(input, b';');
    let comma = split_top_level(input, b',');
    let pieces: Vec<String> = if cwd.join(input).exists() {
        vec![input.to_owned()]
    } else if semicolon.len() > 1 {
        semicolon.into_iter().map(|s| s.trim().to_owned()).collect()
    } else if comma.len() > 1
        && !input.rsplit_once(':').is_some_and(|(base, selector)| {
            cwd.join(base).exists()
                && !selector.is_empty()
                && selector
                    .bytes()
                    .all(|c| c.is_ascii_digit() || b"-+,Ll.".contains(&c))
        })
        && comma.iter().any(|s| cwd.join(s.trim()).exists())
    {
        comma.into_iter().map(|s| s.trim().to_owned()).collect()
    } else {
        let chunks: Vec<_> = input.split_whitespace().collect();
        if chunks.len() > 1 && chunks.iter().all(|s| cwd.join(s).exists()) {
            chunks.into_iter().map(str::to_owned).collect()
        } else {
            vec![input.to_owned()]
        }
    };
    ensure!(
        pieces.iter().all(|s| !s.is_empty()),
        "Search scope entries must be non-empty paths or globs"
    );
    let mut exclusions = Vec::new();
    let mut entries = Vec::new();
    for s in pieces {
        let s = if cwd.join(&s).exists() {
            s
        } else {
            let stripped = s
                .trim()
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .unwrap_or(s.trim());
            if stripped == "~" || stripped.starts_with("~/") {
                std::env::var_os("HOME")
                    .map(|home| {
                        PathBuf::from(home)
                            .join(stripped.strip_prefix("~/").unwrap_or(""))
                            .to_string_lossy()
                            .into_owned()
                    })
                    .unwrap_or_else(|| stripped.to_owned())
            } else {
                stripped.to_owned()
            }
        };
        if s.starts_with('!') && !cwd.join(&s).exists() {
            exclusions.push(s[1..].to_owned());
        } else {
            entries.push(s);
        }
    }
    if entries.is_empty() {
        entries.push(".".into());
    }
    let one_entry = entries.len() == 1;
    let mut targets = BTreeMap::new();
    let mut missing = Vec::new();
    for entry in entries {
        check(token, deadline)?;
        let (raw, ranges) = if cwd.join(&entry).symlink_metadata().is_ok() {
            (entry.clone(), vec![])
        } else if let Some((p, r)) = entry.rsplit_once(':') {
            if !r.is_empty()
                && r.bytes()
                    .all(|c| c.is_ascii_digit() || b"-+,Ll.".contains(&c))
            {
                let mut ranges = Vec::new();
                let multiple = r.contains(',');
                let number = |value: &str| -> Result<usize> {
                    let value = value.trim_start_matches(['L', 'l']);
                    let n = value.parse::<usize>().context("invalid line range")?;
                    ensure!(n > 0, "line numbers must be positive");
                    Ok(n)
                };
                for segment in r.split(',') {
                    let (a, b) = if let Some((a, b)) = segment.split_once('+') {
                        let a = number(a)?;
                        let n = number(b)?;
                        (a, a.checked_add(n - 1).context("range overflow")?)
                    } else if let Some((a, b)) =
                        segment.split_once("..").or_else(|| segment.split_once('-'))
                    {
                        let a = number(a)?;
                        (a, if b.is_empty() { usize::MAX } else { number(b)? })
                    } else {
                        let a = number(segment)?;
                        (a, if multiple { a } else { usize::MAX })
                    };
                    ensure!(b >= a, "invalid line range");
                    ranges.push((a, b));
                }
                (p.to_owned(), ranges)
            } else {
                (entry.clone(), vec![])
            }
        } else {
            (entry.clone(), vec![])
        };
        let abs = cwd.join(&raw);
        if abs.symlink_metadata().is_ok() {
            reject_symlink_components(&abs)?;
        }
        let existing = abs.symlink_metadata().ok();
        if let Some(meta) = existing {
            ensure!(
                !meta.file_type().is_symlink(),
                "symlink search roots are not supported"
            );
            if meta.is_file() {
                let display = abs
                    .strip_prefix(cwd)
                    .unwrap_or(&abs)
                    .to_string_lossy()
                    .into_owned();
                let abs = fs::canonicalize(&abs)?;
                add_target(
                    &mut targets,
                    Target {
                        path: abs,
                        display,
                        ranges,
                        explicit: true,
                    },
                );
                continue;
            }
            ensure!(meta.is_dir(), "search root must be a file or directory");
            ensure!(
                ranges.is_empty(),
                "line-range selector requires a single file"
            );
        }
        let (root, pat) = if abs.is_dir() {
            (abs.clone(), None)
        } else if glob_path(&raw) {
            ensure!(
                ranges.is_empty(),
                "line-range selector requires a single file"
            );
            let ix = raw.find(['*', '?', '[', '{']).unwrap();
            let prefix = &raw[..ix];
            let slash = prefix.rfind('/');
            let (base, pattern) = slash.map_or((".", raw.as_str()), |i| (&raw[..i], &raw[i + 1..]));
            (
                cwd.join(if base.is_empty() { "/" } else { base }),
                Some(pattern.to_owned()),
            )
        } else {
            missing.push(entry);
            continue;
        };
        if !root.is_dir() {
            missing.push(entry);
            continue;
        }
        reject_symlink_components(&root)?;
        let root = fs::canonicalize(root)?;
        let matcher = pat.as_deref().map(glob).transpose()?;
        let exclude: Vec<_> = exclusions.iter().map(|e| glob(e)).collect::<Result<_>>()?;
        let filter_exclude = exclude.clone();
        let filter_patterns = exclusions.clone();
        let filter_root = root.clone();
        let filter_cwd = cwd.to_owned();
        let mut builder = ignore::WalkBuilder::new(&root);
        builder
            .hidden(false)
            .follow_links(false)
            .git_ignore(respect)
            .git_global(respect)
            .git_exclude(respect)
            .ignore(respect)
            .require_git(false)
            .filter_entry(move |entry| {
                if entry.depth() == 0 {
                    return true;
                }
                if entry.file_name() == ".git" {
                    return false;
                }
                let path = entry.path();
                let rel = path.strip_prefix(&filter_root).unwrap_or(path);
                !filter_exclude
                    .iter()
                    .zip(&filter_patterns)
                    .any(|(matcher, pattern)| {
                        if pattern.contains('/') {
                            path.strip_prefix(&filter_cwd)
                                .unwrap_or(path)
                                .ancestors()
                                .any(|part| matcher.is_match(part))
                        } else {
                            rel.components()
                                .any(|part| matcher.is_match(part.as_os_str()))
                        }
                    })
            })
            .sort_by_file_path(|a, b| a.cmp(b));
        for item in builder.build() {
            check(token, deadline)?;
            let Ok(item) = item else { continue };
            let path = item.path();
            if path.components().any(|c| c.as_os_str() == ".git")
                || !item.file_type().is_some_and(|f| f.is_file())
            {
                continue;
            }
            let rel = path.strip_prefix(&root).unwrap_or(path);
            if let Some(m) = &matcher {
                let plain = !raw.contains('/');
                if !m.is_match(rel) && !(plain && path.file_name().is_some_and(|n| m.is_match(n))) {
                    continue;
                }
            }
            if exclude.iter().zip(&exclusions).any(|(m, e)| {
                if e.contains('/') {
                    path.strip_prefix(cwd)
                        .unwrap_or(path)
                        .ancestors()
                        .any(|candidate| m.is_match(candidate))
                } else {
                    rel.components().any(|c| m.is_match(c.as_os_str()))
                }
            }) {
                continue;
            }
            let display = path
                .strip_prefix(cwd)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            add_target(
                &mut targets,
                Target {
                    path: path.to_owned(),
                    display,
                    ranges: vec![],
                    explicit: false,
                },
            );
        }
    }
    ensure!(
        !targets.is_empty() || missing.is_empty(),
        "Path not found: {}",
        missing.join(", ")
    );
    let single = one_entry
        && targets.len() == 1
        && targets
            .values()
            .next()
            .is_some_and(|target| target.explicit);
    Ok((targets.into_values().collect(), single, missing))
}
struct Collector<'a, M> {
    matcher: &'a M,
    rows: BTreeMap<usize, Row>,
    matches: usize,
    limit: usize,
    ranges: &'a [(usize, usize)],
    token: &'a CancellationToken,
    deadline: Instant,
}
impl<M: Matcher> Collector<'_, M> {
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
        let n = m.line_number().unwrap_or(1) as usize;
        if !self.ranges.is_empty() && !self.ranges.iter().any(|(a, b)| n >= *a && n <= *b) {
            return Ok(true);
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
}
struct CheckedReader<'a> {
    file: File,
    token: &'a CancellationToken,
    deadline: Instant,
}
impl Read for CheckedReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.token.is_cancelled() || Instant::now() >= self.deadline {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "grep interrupted",
            ));
        }
        self.file.read(buf)
    }
}
#[allow(clippy::too_many_arguments)]
fn run_file<M: Matcher>(
    matcher: &M,
    target: Target,
    settings: GrepSettings,
    multiline: bool,
    cap: usize,
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
    ensure!(metadata.is_file(), "grep requires a regular file");
    let mut collector = Collector {
        matcher,
        rows: BTreeMap::new(),
        matches: 0,
        limit: cap,
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
    found.map_err(|e| anyhow::anyhow!("grep search failed: {e}"))?;
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
        matches: matches.min(cap),
        capped: matches > cap,
        bytes,
    }))
}
fn libc_nofollow() -> i32 {
    (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32
}

fn grouped_header(
    path: &str,
    tag: Option<&str>,
    common: usize,
    previous: &mut Vec<String>,
) -> String {
    let parts: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    let dirs = &parts[..parts.len().saturating_sub(1)];
    let mut groups = Vec::new();
    if common > 0 {
        groups.push(dirs[..common].join("/"));
    }
    groups.extend(dirs[common..].iter().map(|part| (*part).to_owned()));
    let shared = groups
        .iter()
        .zip(previous.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut output = String::new();
    for (depth, directory) in groups.iter().enumerate().skip(shared) {
        output.push_str(&format!("{} {directory}/\n", "#".repeat(depth + 1)));
    }
    let name = parts.last().copied().unwrap_or(path);
    output.push_str(&format!(
        "{} {name}{}\n",
        "#".repeat(groups.len() + 1),
        tag.map(|t| format!("#{t}")).unwrap_or_default()
    ));
    *previous = groups;
    output
}

pub(super) fn run(
    cwd: &Path,
    settings: GrepSettings,
    mode: EditMode,
    workspace: &save::Workspace,
    snapshots: &mut hashline::Store,
    arguments: &str,
    token: &CancellationToken,
) -> Result<Value> {
    let args: Args = serde_json::from_str(arguments).context("invalid grep arguments")?;
    ensure!(!args.pattern.trim().is_empty(), "Pattern must not be empty");
    let skip = args.skip.unwrap_or(0.0);
    ensure!(
        skip.is_finite() && skip >= 0.0 && skip <= usize::MAX as f64,
        "Skip must be a non-negative number"
    );
    let skip = skip.floor() as usize;
    let deadline = Instant::now() + Duration::from_secs(30);
    let (targets, single, missing) = parse_paths(
        cwd,
        args.path.as_deref().unwrap_or("."),
        args.gitignore.unwrap_or(true),
        token,
        deadline,
    )?;
    let multiline = args.pattern.contains('\n') || args.pattern.contains("\\n");
    let compiled = compile(
        &args.pattern,
        !args.case_sensitive.unwrap_or(true),
        multiline,
    )?;
    let per_file = if single { SINGLE_LIMIT } else { MULTI_LIMIT };
    let mut found = Vec::new();
    let mut total = 0;
    let mut internal_limit = false;
    let mut warnings = Vec::new();
    for target in targets {
        check(token, deadline)?;
        let large = fs::metadata(&target.path).is_ok_and(|m| m.len() > MAX_FILE);
        if large && (multiline || !target.explicit) {
            warnings.push(format!("{}: searched only the first 4 MiB", target.display));
        }
        let in_page =
            single || (found.len() >= skip && found.len() < skip.saturating_add(FILE_LIMIT));
        let capture_snapshot = mode == EditMode::Hashline && in_page;
        let result = match &compiled {
            Compiled::Rust(m) => run_file(
                m,
                target,
                settings,
                multiline,
                per_file,
                capture_snapshot,
                token,
                deadline,
            ),
            Compiled::Pcre(m) => run_file(
                m,
                target,
                settings,
                multiline,
                per_file,
                capture_snapshot,
                token,
                deadline,
            ),
        };
        match result {
            Ok(Some(mut f)) => {
                total += f.matches;
                if !in_page {
                    f.rows.clear();
                }
                found.push(f);
                if total >= TOTAL_LIMIT {
                    internal_limit = true;
                    break;
                }
            }
            Ok(None) => {}
            Err(e) => {
                check(token, deadline)?;
                warnings.push(format!("Skipped file: {e:#}"));
            }
        }
    }
    let count = found.len();
    let start = if single { 0 } else { skip.min(count) };
    let end = if single {
        count
    } else {
        start.saturating_add(FILE_LIMIT).min(count)
    };
    let page = &found[start..end];
    let file_limit = end < count;
    let mut content = String::new();
    let mut visible_matches = 0;
    let mut line_cut = false;
    let mut byte_cut = false;
    let mut file_details = Vec::new();
    let mut common = page
        .first()
        .map(|file| {
            file.target
                .display
                .split('/')
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    common.pop();
    for file in page.iter().skip(1) {
        let mut dirs = file
            .target
            .display
            .split('/')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>();
        dirs.pop();
        common.truncate(
            common
                .iter()
                .zip(dirs.iter())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    let mut previous_dirs = Vec::new();
    for file in page {
        let key = workspace
            .components(&file.target.path)
            .ok()
            .map(|c| c.into_iter().collect::<PathBuf>());
        let editable = if mode == EditMode::Hashline {
            match (&key, &file.bytes) {
                (Some(k), Some(raw))
                    if !k.to_string_lossy().contains(['[', ']', '\n', '\r'])
                        && !raw.contains('\0')
                        && !raw
                            .as_bytes()
                            .windows(2)
                            .any(|pair| pair[0] == b'\r' && pair[1] != b'\n')
                        && !raw.ends_with('\r') =>
                {
                    workspace
                        .target(&file.target.path, false)
                        .ok()
                        .and_then(|t| t.read_bounded(hashline::MAX_FILE_BYTES).ok())
                        .filter(|b| b.as_slice() == raw.as_bytes())
                        .map(|_| (k.clone(), raw.clone(), hashline::file_hash(raw)))
                }
                _ => None,
            }
        } else {
            None
        };
        let editable =
            editable.filter(|(key, raw, _)| snapshots.record(key.clone(), raw.clone(), Vec::new()));
        let mut header = if single {
            editable
                .as_ref()
                .map(|(k, _, tag)| format!("[{}#{tag}]\n", k.display()))
                .unwrap_or_default()
        } else {
            grouped_header(
                &file.target.display,
                editable.as_ref().map(|(_, _, t)| t.as_str()),
                common.len(),
                &mut previous_dirs,
            )
        };
        if !content.is_empty() {
            header.insert(0, '\n');
        }
        if content.len() + header.len() > MAX_OUTPUT - 2048 {
            byte_cut = true;
            break;
        }
        content.push_str(&header);
        let mut seen = Vec::new();
        let mut last = None;
        for row in &file.rows {
            let sep = if last.is_some_and(|n| row.number > n + 1) {
                "...\n"
            } else {
                ""
            };
            let marker = if row.matched { '*' } else { ' ' };
            let delimiter = if editable.is_some() { ':' } else { '|' };
            let column = row
                .column
                .map(|n| format!(" [col {n}]"))
                .unwrap_or_default();
            let text = format!(
                "{sep}{marker}{}{delimiter}{}{column}\n",
                row.number, row.text
            );
            if content.len() + text.len() > MAX_OUTPUT - 2048 {
                byte_cut = true;
                break;
            }
            content.push_str(&text);
            if !row.truncated {
                seen.push(row.number..row.number + 1);
            }
            if row.matched {
                visible_matches += 1;
            }
            line_cut |= row.truncated;
            last = Some(row.number);
        }
        if let Some((key, raw, _)) = editable {
            let _ = snapshots.record(key, raw, seen);
        }
        file_details.push(json!({"path":file.target.display,"count":file.matches}));
        if byte_cut {
            break;
        }
    }
    let mut notes = Vec::new();
    if file_limit {
        notes.push(format!(
            "Showing files {}-{} of {}{}. Use skip={} for the next page, or narrow paths/pattern.",
            start + 1,
            end,
            count,
            if internal_limit { "+" } else { "" },
            end
        ));
    }
    if internal_limit {
        notes.push("Internal search match limit reached; narrow paths or pattern.".into());
    }
    for f in page.iter().filter(|f| f.capped) {
        notes.push(format!(
            "More than {per_file} matches in {}; narrow the pattern or search one file.",
            f.target.display
        ));
    }
    if !missing.is_empty() {
        notes.push(format!("Skipped missing paths: {}", missing.join(", ")));
    }
    if byte_cut {
        notes.push("Output truncated by byte limit; narrow paths or pattern.".into());
    }
    if line_cut {
        notes.push("Some lines were truncated to 512 bytes.".into());
    }
    notes.extend(warnings.into_iter().take(10));
    if content.is_empty() {
        content = if skip > 0 && count > 0 {
            "No more results".into()
        } else {
            "No matches found".into()
        };
    }
    if !notes.is_empty() {
        content.push_str("\n\n");
        let mut notice = notes.join("\n");
        let remaining = MAX_OUTPUT.saturating_sub(content.len());
        if notice.len() > remaining {
            let mut end = remaining.saturating_sub(24).min(notice.len());
            while !notice.is_char_boundary(end) {
                end -= 1;
            }
            notice.truncate(end);
            notice.push_str("\n... notices truncated");
        }
        content.push_str(&notice);
    }
    Ok(
        json!({"ok":true,"content":content,"details":{"matchCount":visible_matches,"fileCount":file_details.len(),"fileMatches":file_details,"fileLimitReached":file_limit,"perFileLimitReached":page.iter().any(|f|f.capped),"linesTruncated":line_cut,"truncated":file_limit || internal_limit || byte_cut || line_cut || page.iter().any(|f|f.capped),"scopePath":args.path.unwrap_or_else(||".".into())}}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn execute(dir: &Path, args: Value, mode: EditMode, store: &mut hashline::Store) -> Value {
        let canonical = dir.canonicalize().unwrap();
        run(
            &canonical,
            GrepSettings::default(),
            mode,
            &save::Workspace::open(&canonical).unwrap(),
            store,
            &args.to_string(),
            &CancellationToken::new(),
        )
        .unwrap()
    }
    #[test]
    fn grep_native_regex_paths_and_ranges() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("src")).unwrap();
        fs::write(
            d.path().join("src/a.rs"),
            "first\nfetchProvider(\nfoobar\nfooquux\nlast\n",
        )
        .unwrap();
        fs::write(d.path().join("src/b.txt"), "foobar\n").unwrap();
        fs::write(d.path().join("4"), "unrelated\n").unwrap();
        fs::write(d.path().join("txt}"), "unrelated\n").unwrap();
        fs::create_dir(d.path().join("src/nested")).unwrap();
        fs::write(d.path().join("src/nested/deep.rs"), "foobar\n").unwrap();
        let mut store = hashline::Store::default();
        let a = execute(
            d.path(),
            json!({"pattern":"foo(?=bar)","path":"src/*.rs"}),
            EditMode::Replace,
            &mut store,
        );
        assert!(a["content"].as_str().unwrap().contains("*3|foobar"), "{a}");
        assert!(!a["content"].as_str().unwrap().contains("deep.rs"), "{a}");
        let braces = execute(
            d.path(),
            json!({"pattern":"foobar", "path":"src/*.{rs,txt}"}),
            EditMode::Replace,
            &mut store,
        );
        assert_eq!(braces["details"]["fileCount"], 2, "{braces}");
        let recursive = execute(
            d.path(),
            json!({"pattern":"foobar", "path":"src/**/*.rs"}),
            EditMode::Replace,
            &mut store,
        );
        assert!(
            recursive["content"].as_str().unwrap().contains("deep.rs"),
            "{recursive}"
        );
        let b = execute(
            d.path(),
            json!({"pattern":"fetchProvider(","path":"src/a.rs:2-2"}),
            EditMode::Replace,
            &mut store,
        );
        assert!(
            b["content"].as_str().unwrap().contains("*2|fetchProvider("),
            "{b}"
        );
        assert!(!b["content"].as_str().unwrap().contains("foobar"));
        let onward = execute(
            d.path(),
            json!({"pattern":"foo", "path":"src/a.rs:4"}),
            EditMode::Replace,
            &mut store,
        );
        assert!(
            onward["content"].as_str().unwrap().contains("*4|fooquux"),
            "{onward}"
        );
        assert!(
            !onward["content"].as_str().unwrap().contains("foobar"),
            "{onward}"
        );
        let ranged = execute(
            d.path(),
            json!({"pattern":"foo", "path":"src/a.rs:L3..L4"}),
            EditMode::Replace,
            &mut store,
        );
        assert_eq!(ranged["details"]["matchCount"], 2, "{ranged}");
        let disjoint = execute(
            d.path(),
            json!({"pattern":"foo", "path":"src/a.rs:3,4"}),
            EditMode::Replace,
            &mut store,
        );
        assert_eq!(disjoint["details"]["matchCount"], 2, "{disjoint}");
        let c = execute(
            d.path(),
            json!({"pattern":"foo\\nbar", "path":"src/a.rs"}),
            EditMode::Replace,
            &mut store,
        );
        assert_eq!(c["ok"], true);
    }
    #[test]
    fn grep_file_pagination_and_hashline_seen() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..22 {
            fs::write(d.path().join(format!("{i:02}.txt")), "needle\nother\n").unwrap();
        }
        let mut store = hashline::Store::default();
        let page = execute(
            d.path(),
            json!({"pattern":"needle"}),
            EditMode::Replace,
            &mut store,
        );
        assert_eq!(page["details"]["fileCount"], 20, "{page}");
        assert!(page["content"].as_str().unwrap().contains("skip=20"));
        let second = execute(
            d.path(),
            json!({"pattern":"needle","skip":20}),
            EditMode::Replace,
            &mut store,
        );
        assert_eq!(second["details"]["fileCount"], 2);
        let file = execute(
            d.path(),
            json!({"pattern":"needle","path":"00.txt"}),
            EditMode::Hashline,
            &mut store,
        );
        assert!(
            file["content"].as_str().unwrap().contains("[00.txt#"),
            "{file}"
        );
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    fn query(cwd: &Path, args: Value) -> Value {
        let cwd = cwd.canonicalize().unwrap();
        run(
            &cwd,
            GrepSettings::default(),
            EditMode::Replace,
            &save::Workspace::open(&cwd).unwrap(),
            &mut hashline::Store::default(),
            &args.to_string(),
            &CancellationToken::new(),
        )
        .unwrap()
    }
    #[test]
    fn grep_hidden_ignore_exclusions_and_literal_filename() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join(".ignore"), "ignored.txt\n").unwrap();
        fs::write(d.path().join("ignored.txt"), "need\n").unwrap();
        fs::write(d.path().join(".hidden"), "need\n").unwrap();
        fs::write(d.path().join("literal[1].txt"), "need\n").unwrap();
        fs::create_dir(d.path().join("vendor")).unwrap();
        fs::write(d.path().join("vendor/a.txt"), "need\n").unwrap();
        let r = query(d.path(), json!({"pattern":"need","path":".;!vendor"}));
        let text = r["content"].as_str().unwrap();
        assert!(text.contains(".hidden"), "{text}");
        assert!(!text.contains("ignored.txt"));
        assert!(!text.contains("vendor"));
        let exact = query(d.path(), json!({"pattern":"need","path":"literal[1].txt"}));
        assert!(exact["content"].as_str().unwrap().contains("*1|need"));
        let no_ignore = query(d.path(), json!({"pattern":"need","gitignore":false}));
        assert!(
            no_ignore["content"]
                .as_str()
                .unwrap()
                .contains("ignored.txt")
        );
    }
    #[test]
    fn grep_multiline_and_long_line_window() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("file"),
            format!("foo\nbar\n{}needle-end\n", "中".repeat(1000)),
        )
        .unwrap();
        let cross = query(d.path(), json!({"pattern":"foo\\nbar","path":"file"}));
        assert!(
            cross["content"].as_str().unwrap().contains("*1|foo"),
            "{cross}"
        );
        let long = query(d.path(), json!({"pattern":"needle","path":"file"}));
        let text = long["content"].as_str().unwrap();
        assert!(
            text.contains("needle-end") && text.contains("[col 1001]"),
            "{text}"
        );
        assert_eq!(long["details"]["linesTruncated"], true);
    }
    #[test]
    fn grep_symlink_scope_and_pre_cancelled_are_rejected() {
        use std::os::unix::fs::symlink;
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("target"), "needle").unwrap();
        symlink("target", d.path().join("link")).unwrap();
        let cwd = d.path().canonicalize().unwrap();
        let workspace = save::Workspace::open(&cwd).unwrap();
        let call = |path: &str, token: &CancellationToken| {
            run(
                &cwd,
                GrepSettings::default(),
                EditMode::Replace,
                &workspace,
                &mut hashline::Store::default(),
                &json!({"pattern":"needle","path":path}).to_string(),
                token,
            )
        };
        assert!(call("link", &CancellationToken::new()).is_err());
        let t = CancellationToken::new();
        t.cancel();
        assert!(
            call("target", &t)
                .unwrap_err()
                .downcast_ref::<crate::cancellation::Cancelled>()
                .is_some()
        );
    }

    #[test]
    fn grep_large_file_direct_search_streams_but_directory_reports_partial_coverage() {
        let d = tempfile::tempdir().unwrap();
        let mut content = "padding\n".repeat(524_289);
        content.push_str("needle-at-end\n");
        fs::write(d.path().join("large.txt"), content).unwrap();
        let direct = query(
            d.path(),
            json!({"pattern":"needle-at-end", "path":"large.txt"}),
        );
        assert!(
            direct["content"]
                .as_str()
                .unwrap()
                .contains("needle-at-end"),
            "{direct}"
        );
        let directory = query(d.path(), json!({"pattern":"needle-at-end"}));
        let text = directory["content"].as_str().unwrap();
        assert!(
            text.contains("No matches found") && text.contains("first 4 MiB"),
            "{text}"
        );
    }
}
