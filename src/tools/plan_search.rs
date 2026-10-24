//! In-process Plan-mode search tools.
//!
//! `plan_search` matches file contents by literal, case-sensitive substring
//! and `plan_find` matches root-relative file paths. Both run entirely inside
//! the process: no `rg`/`grep`/`fd`/`find` argv, no shell, no `.pp-cache`.
//! Traversal reuses the `ignore` crate with the same project-ignore policy as
//! the TUI file scan, and file contents are streamed line by line with line
//! buffers bounded by `read_max_bytes`.

use crate::cancellation::CancellationToken;
use crate::config::ToolLimits;
use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{
    borrow::Cow,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use super::{OUTPUT_LIMIT, resolve_path};

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 500;
const MAX_WARNINGS: usize = 10;
const WARNING_BYTES: usize = 8192;
const READ_BUFFER_BYTES: usize = 64 * 1024;

pub(crate) fn run(
    cwd: &Path,
    limits: &ToolLimits,
    name: &str,
    arguments: &str,
    cancellation: &CancellationToken,
) -> Result<Value> {
    match name {
        "plan_search" => search(cwd, limits, arguments, cancellation),
        "plan_find" => find(cwd, limits, arguments, cancellation),
        _ => bail!("unknown plan search tool: {name}"),
    }
}

pub(crate) fn definitions() -> Vec<FunctionObject> {
    let schema = |query: Value, required: &[&str]| {
        let mut properties = Map::new();
        properties.insert("query".into(), query);
        properties.insert(
            "path".into(),
            json!({"type":"string","description":"File or directory to search; relative to startup cwd unless absolute. Defaults to '.'."}),
        );
        properties.insert(
            "hidden".into(),
            json!({"type":"boolean","description":"Include hidden files and directories. Defaults to false."}),
        );
        properties.insert(
            "no_ignore".into(),
            json!({"type":"boolean","description":"Ignore .gitignore/.ignore files. Defaults to false."}),
        );
        properties.insert(
            "offset".into(),
            json!({"type":"integer","minimum":0,"description":"0-based result offset for pagination."}),
        );
        properties.insert(
            "limit".into(),
            json!({"type":"integer","minimum":1,"maximum":500,"description":"Maximum results per page. Defaults to 200."}),
        );
        let mut schema = Map::new();
        schema.insert("type".into(), json!("object"));
        schema.insert("properties".into(), Value::Object(properties));
        schema.insert("required".into(), json!(required));
        schema.insert("additionalProperties".into(), Value::Bool(false));
        Some(Value::Object(schema))
    };
    vec![
        FunctionObject {
            name: "plan_search".into(),
            description: Some(
                "Search file contents for a literal, case-sensitive substring (no regex, no shell, no subprocess). Each match is {path, line, text} with a 1-based line number and lossy UTF-8 text; path is relative to the search root. Files containing NUL bytes or lines longer than the configured read_max_bytes are skipped and reported in warnings. Results are paginated with offset/limit; next_offset is null when no results remain and truncated marks a page cut off by the output limit. .gitignore/.ignore files are respected by default; .git is always skipped."
                    .into(),
            ),
            parameters: schema(
                json!({"type":"string","minLength":1,"description":"Literal substring to match, compared case-sensitively; must not be empty."}),
                &["query"],
            ),
            ..Default::default()
        },
        FunctionObject {
            name: "plan_find".into(),
            description: Some(
                "List regular files whose path relative to the search root contains a literal, case-sensitive substring (no regex, no shell, no subprocess). An empty query enumerates all files. Directories, symlinks and special files are never returned. Uses the same path/hidden/no_ignore/offset/limit parameters, pagination and warnings behavior as plan_search."
                    .into(),
            ),
            parameters: schema(
                json!({"type":"string","description":"Literal substring of the root-relative path; empty lists every file."}),
                &[],
            ),
            ..Default::default()
        },
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    no_ignore: bool,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindArgs {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    no_ignore: bool,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    DEFAULT_LIMIT
}

/// Controls whether the match callback wants more results from this file.
#[derive(PartialEq, Eq)]
enum Flow {
    Continue,
    Stop,
}

/// Outcome of streaming one candidate file.
enum FileFlow {
    Exhausted,
    Skipped(String),
    Unavailable(String),
    Stop,
}

/// Warnings capped at `MAX_WARNINGS` with the overflow counted separately.
/// `bytes` is the serialized size contribution for the running output budget.
#[derive(Default)]
struct Warnings {
    kept: Vec<Value>,
    omitted: usize,
    bytes: usize,
}

impl Warnings {
    fn push(&mut self, path: &str, reason: &str) {
        if self.kept.len() >= MAX_WARNINGS {
            self.omitted += 1;
            return;
        }
        let bounded = |text: &str| text.chars().take(256).collect::<String>();
        let warning = json!({"path": bounded(path), "reason": bounded(reason)});
        let bytes = serialized_len(&warning) + 1;
        if self.bytes + bytes > WARNING_BYTES {
            self.omitted += 1;
            return;
        }
        self.bytes += bytes;
        self.kept.push(warning);
    }
}

/// Shared pagination and output budget for plan_search and plan_find: every
/// candidate entry is admitted, omitted with a warning, or cuts the page.
struct Pager {
    offset: usize,
    limit: usize,
    used: usize,
    index: usize,
    returned: usize,
    truncated: bool,
    next_offset: Option<usize>,
}

enum Admit {
    /// The entry counts toward this page; the caller pushes it.
    Taken,
    /// The entry was skipped (before `offset`) or omitted with a warning.
    Dropped,
    /// The page or the output budget is full; stop scanning.
    Full,
}

impl Pager {
    fn new(key: &str, offset: usize, limit: usize) -> Self {
        Self {
            offset,
            limit,
            used: skeleton(key) + WARNING_BYTES,
            index: 0,
            returned: 0,
            truncated: false,
            next_offset: None,
        }
    }

    /// Admit one candidate entry; `rel` is used for the omission warning.
    fn admit(&mut self, warnings: &mut Warnings, rel: &str, entry: &Value) -> Admit {
        if self.index < self.offset {
            self.index += 1;
            return Admit::Dropped;
        }
        if self.returned == self.limit {
            self.truncated = true;
            self.next_offset = Some(self.index);
            return Admit::Full;
        }
        let size = serialized_len(entry);
        if size > OUTPUT_LIMIT
            || (self.used + size + 1 > OUTPUT_LIMIT
                && self.returned == 0
                && self.index == self.offset)
        {
            // A single result over the output limit is never returned
            // half; when it cannot fit an otherwise empty page, drop
            // it so pagination always advances.
            warnings.push(rel, "result omitted: exceeds output limit");
            self.index += 1;
            return Admit::Dropped;
        }
        if self.used + size + 1 > OUTPUT_LIMIT {
            self.truncated = true;
            self.next_offset = Some(self.index);
            return Admit::Full;
        }
        self.used += size + 1;
        self.returned += 1;
        self.index += 1;
        Admit::Taken
    }

    /// Rolls the pager back to a file-scan snapshot after the file's earlier
    /// matches were invalidated by a late per-file error.
    fn rewind(&mut self, snapshot: (usize, usize, usize)) {
        self.index = snapshot.0;
        self.returned = snapshot.1;
        self.used = snapshot.2;
        self.truncated = false;
        self.next_offset = None;
    }
}

fn search(
    cwd: &Path,
    limits: &ToolLimits,
    arguments: &str,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let args: SearchArgs =
        serde_json::from_str(arguments).context("invalid plan_search arguments")?;
    ensure!(!args.query.is_empty(), "query must not be empty");
    ensure!(
        args.limit > 0 && args.limit <= MAX_LIMIT,
        "limit must be between 1 and {MAX_LIMIT}"
    );
    let deadline = Instant::now() + Duration::from_secs(limits.bash_timeout_seconds);
    let (root, is_dir) = resolve_root(cwd, args.path.as_deref())?;
    let (files, mut warnings) = collect_files(
        &root,
        is_dir,
        args.hidden,
        args.no_ignore,
        deadline,
        cancellation,
    )?;
    let mut pager = Pager::new("matches", args.offset, args.limit);
    let mut matches: Vec<Value> = Vec::new();
    for file in &files {
        let rel = relative(&root, file);
        let before = (pager.index, pager.returned, pager.used, matches.len());
        let flow = scan_file(
            file,
            &args.query,
            limits.read_max_bytes,
            deadline,
            cancellation,
            &mut |line_no: usize, text: &str| -> Result<Flow> {
                let entry = json!({"path": rel, "line": line_no, "text": text});
                match pager.admit(&mut warnings, &rel, &entry) {
                    Admit::Taken => matches.push(entry),
                    Admit::Dropped => {}
                    Admit::Full => return Ok(Flow::Stop),
                }
                Ok(Flow::Continue)
            },
        )?;
        match flow {
            FileFlow::Exhausted => {}
            flow @ (FileFlow::Skipped(_) | FileFlow::Unavailable(_)) => {
                let reason = match flow {
                    FileFlow::Unavailable(reason) if !is_dir => bail!("{reason}"),
                    FileFlow::Skipped(reason) | FileFlow::Unavailable(reason) => reason,
                    _ => unreachable!(),
                };
                matches.truncate(before.3);
                pager.rewind((before.0, before.1, before.2));
                warnings.push(&rel, &reason);
            }
            FileFlow::Stop => break,
        }
    }
    Ok(assemble(
        "matches",
        matches,
        pager.next_offset,
        pager.truncated,
        warnings,
    ))
}

fn find(
    cwd: &Path,
    limits: &ToolLimits,
    arguments: &str,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let args: FindArgs = serde_json::from_str(arguments).context("invalid plan_find arguments")?;
    ensure!(
        args.limit > 0 && args.limit <= MAX_LIMIT,
        "limit must be between 1 and {MAX_LIMIT}"
    );
    let query = args.query.unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(limits.bash_timeout_seconds);
    let (root, is_dir) = resolve_root(cwd, args.path.as_deref())?;
    let (files, mut warnings) = collect_files(
        &root,
        is_dir,
        args.hidden,
        args.no_ignore,
        deadline,
        cancellation,
    )?;
    let mut pager = Pager::new("paths", args.offset, args.limit);
    let mut paths: Vec<Value> = Vec::new();
    for file in &files {
        let rel = relative(&root, file);
        // An empty query matches every path.
        if !rel.contains(query.as_str()) {
            continue;
        }
        let entry = Value::String(rel.clone());
        match pager.admit(&mut warnings, &rel, &entry) {
            Admit::Taken => paths.push(entry),
            Admit::Dropped => {}
            Admit::Full => break,
        }
    }
    Ok(assemble(
        "paths",
        paths,
        pager.next_offset,
        pager.truncated,
        warnings,
    ))
}

fn resolve_root(cwd: &Path, path_arg: Option<&str>) -> Result<(PathBuf, bool)> {
    let root = match path_arg {
        Some(path) => resolve_path(cwd, path)?,
        None => cwd.to_path_buf(),
    };
    let metadata = fs::symlink_metadata(&root)
        .with_context(|| format!("search root not found: {}", root.display()))?;
    ensure!(
        metadata.is_file() || metadata.is_dir(),
        "search root must be a regular file or directory: {}",
        root.display()
    );
    if metadata.is_file() {
        OpenOptions::new()
            .read(true)
            .custom_flags(
                (rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::NOFOLLOW).bits() as i32,
            )
            .open(&root)
            .with_context(|| format!("search root is not readable: {}", root.display()))?;
    }
    if metadata.is_dir() {
        // Fail fast when the root itself cannot be listed; problems with
        // individual entries inside stay per-entry warnings.
        fs::read_dir(&root)
            .with_context(|| format!("search root is not readable: {}", root.display()))?;
    }
    Ok((root, metadata.is_dir()))
}

fn collect_files(
    root: &Path,
    is_dir: bool,
    hidden: bool,
    no_ignore: bool,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<(Vec<PathBuf>, Warnings)> {
    if !is_dir {
        return Ok((vec![root.to_path_buf()], Warnings::default()));
    }
    let walker = ignore::WalkBuilder::new(root)
        .hidden(!hidden)
        .ignore(!no_ignore)
        .git_ignore(!no_ignore)
        .git_exclude(!no_ignore)
        .parents(false)
        .git_global(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != ".git")
        .build();
    let mut files = Vec::new();
    let mut warnings = Warnings::default();
    for entry in walker {
        checkpoint(deadline, cancellation)?;
        match entry {
            Ok(entry) => {
                if entry.depth() == 0 {
                    continue;
                }
                let path = entry.path().to_path_buf();
                match entry.file_type() {
                    Some(file_type) if file_type.is_file() => files.push(path),
                    Some(file_type) if file_type.is_dir() => {}
                    Some(_) => {
                        warnings.push(&relative(root, &path), "skipped: special file");
                    }
                    None => {
                        warnings.push(&relative(root, &path), "skipped: unknown file type");
                    }
                }
            }
            Err(error) => {
                let path = error_path(&error)
                    .map(|path| relative(root, path))
                    .unwrap_or_else(|| root.display().to_string());
                warnings.push(&path, &format!("{error}"));
            }
        }
    }
    files.sort();
    Ok((files, warnings))
}

/// Streams one file line by line without holding more than one line (bounded
/// by `max_line_bytes`) plus one read buffer in memory.
fn scan_file(
    path: &Path,
    query: &str,
    max_line_bytes: usize,
    deadline: Instant,
    cancellation: &CancellationToken,
    on_match: &mut dyn FnMut(usize, &str) -> Result<Flow>,
) -> Result<FileFlow> {
    // Open non-blocking so a FIFO swapped in after the walk cannot hang the
    // search; regular files are unaffected.
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags((rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::NOFOLLOW).bits() as i32)
        .open(path)
    {
        Ok(file) => file,
        Err(error) => return Ok(FileFlow::Unavailable(format!("skipped: {error}"))),
    };
    let is_regular = file
        .metadata()
        .map(|metadata| metadata.is_file())
        .unwrap_or(false);
    if !is_regular {
        return Ok(FileFlow::Skipped("skipped: not a regular file".into()));
    }
    let mut reader = BufReader::with_capacity(READ_BUFFER_BYTES, file);
    let mut line: Vec<u8> = Vec::new();
    let mut line_no = 0usize;
    let mut stopped = false;
    loop {
        checkpoint(deadline, cancellation)?;
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Ok(FileFlow::Unavailable(format!("skipped: {error}"))),
        };
        if chunk.is_empty() {
            if !line.is_empty() {
                line_no += 1;
                if !stopped && emit_line(&line, query, line_no, on_match)? == Flow::Stop {
                    stopped = true;
                }
            }
            return Ok(if stopped {
                FileFlow::Stop
            } else {
                FileFlow::Exhausted
            });
        }
        match chunk.iter().position(|&byte| byte == 0 || byte == b'\n') {
            Some(at) if chunk[at] == 0 => {
                return Ok(FileFlow::Skipped("skipped: NUL byte (binary file)".into()));
            }
            Some(at) => {
                if line.len().saturating_add(at) > max_line_bytes {
                    return Ok(FileFlow::Skipped(format!(
                        "skipped: line exceeds read_max_bytes ({max_line_bytes})"
                    )));
                }
                line.extend_from_slice(&chunk[..at]);
                line_no += 1;
                if !stopped && emit_line(&line, query, line_no, on_match)? == Flow::Stop {
                    stopped = true;
                }
                line.clear();
                reader.consume(at + 1);
            }
            None => {
                if line.len() + chunk.len() > max_line_bytes {
                    return Ok(FileFlow::Skipped(format!(
                        "skipped: line exceeds read_max_bytes ({max_line_bytes})"
                    )));
                }
                let consumed = chunk.len();
                line.extend_from_slice(chunk);
                reader.consume(consumed);
            }
        }
    }
}

fn emit_line(
    line: &[u8],
    query: &str,
    line_no: usize,
    on_match: &mut dyn FnMut(usize, &str) -> Result<Flow>,
) -> Result<Flow> {
    match match_text(line, query) {
        Some(text) => on_match(line_no, &text),
        None => Ok(Flow::Continue),
    }
}

/// Literal case-sensitive substring match over the line bytes; text is decoded
/// lossily like read when the line is not valid UTF-8.
fn match_text<'a>(line: &'a [u8], query: &str) -> Option<Cow<'a, str>> {
    match std::str::from_utf8(line) {
        Ok(text) => text.contains(query).then_some(Cow::Borrowed(text)),
        Err(_) => {
            let lossy = String::from_utf8_lossy(line);
            lossy.contains(query).then_some(lossy)
        }
    }
}

/// Extracts the entry path from a walk error, when the variant carries one.
fn error_path(error: &ignore::Error) -> Option<&Path> {
    match error {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithLineNumber { err, .. } | ignore::Error::WithDepth { err, .. } => {
            error_path(err)
        }
        ignore::Error::Partial(errors) => errors.iter().find_map(error_path),
        _ => None,
    }
}

fn relative(root: &Path, path: &Path) -> String {
    if path == root {
        // A file root is its own relative root; show the file name instead.
        return path.file_name().map_or_else(
            || path.to_string_lossy().into_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
    }
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn checkpoint(deadline: Instant, cancellation: &CancellationToken) -> Result<()> {
    crate::cancellation::check(cancellation)?;
    if Instant::now() >= deadline {
        bail!("Plan search timed out.");
    }
    Ok(())
}

fn serialized_len(value: &Value) -> usize {
    // Serializing an owned Value into an infallible writer cannot fail.
    crate::context::serialized_len(value).expect("Value serialization is infallible")
}

/// Worst-case wrapper size (20-digit next_offset/warnings_omitted) so the
/// running budget never undercounts the serialized result.
fn skeleton(key: &str) -> usize {
    format!(
        r#"{{"ok":true,"{key}":[],"next_offset":18446744073709551615,"truncated":false,"warnings":[],"warnings_omitted":18446744073709551615}}"#
    )
    .len()
}

fn assemble(
    key: &str,
    entries: Vec<Value>,
    next_offset: Option<usize>,
    truncated: bool,
    warnings: Warnings,
) -> Value {
    let mut result = Map::new();
    result.insert("ok".into(), Value::Bool(true));
    result.insert(key.into(), Value::Array(entries));
    result.insert("next_offset".into(), json!(next_offset));
    result.insert("truncated".into(), Value::Bool(truncated));
    result.insert("warnings".into(), Value::Array(warnings.kept));
    if warnings.omitted > 0 {
        result.insert("warnings_omitted".into(), json!(warnings.omitted));
    }
    Value::Object(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancellation::Cancelled;
    use std::process::Command;

    fn plan_limits(read_max_bytes: usize, bash_timeout_seconds: u64) -> ToolLimits {
        ToolLimits {
            read_max_bytes,
            read_max_lines: 1,
            bash_timeout_seconds,
        }
    }

    fn plan_run(
        name: &str,
        cwd: &Path,
        args: Value,
        limits: &ToolLimits,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        run(cwd, limits, name, &args.to_string(), cancellation)
    }

    fn plan_match_paths(result: &Value) -> Vec<&str> {
        if let Some(paths) = result["paths"].as_array() {
            paths.iter().map(|path| path.as_str().unwrap()).collect()
        } else {
            result["matches"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["path"].as_str().unwrap())
                .collect()
        }
    }

    #[test]
    fn plan_search_discards_early_matches_when_later_bytes_invalidate_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut binary = b"needle\n".repeat(12_000);
        binary.push(0);
        fs::write(dir.path().join("a.bin"), binary).unwrap();
        fs::write(
            dir.path().join("b.long"),
            "needle\nneedle\n01234567890123456789",
        )
        .unwrap();
        fs::write(dir.path().join("c.txt"), "needle\n").unwrap();
        let result = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"needle","limit":1}),
            &plan_limits(10, 60),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(
            result["matches"],
            json!([{"path":"c.txt","line":1,"text":"needle"}])
        );
        assert_eq!(result["next_offset"], Value::Null);
        assert_eq!(result["warnings"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn plan_search_reports_exact_paths_lines_and_is_case_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn main() {}\nneedle here\nthird\n",
        )
        .unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(
            dir.path().join("nested/b.txt"),
            "no match\nneedle twice\nneedle\n",
        )
        .unwrap();
        let token = CancellationToken::new();
        let result = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"needle"}),
            &plan_limits(4096, 60),
            &token,
        )
        .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["truncated"], false);
        assert_eq!(result["next_offset"], Value::Null);
        let matches = result["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 3);
        assert_eq!(matches[0]["path"], "a.rs");
        assert_eq!(matches[0]["line"], 2);
        assert_eq!(matches[0]["text"], "needle here");
        assert_eq!(matches[1]["path"], "nested/b.txt");
        assert_eq!(matches[1]["line"], 2);
        assert_eq!(matches[1]["text"], "needle twice");
        assert_eq!(matches[2]["path"], "nested/b.txt");
        assert_eq!(matches[2]["line"], 3);
        // Case-sensitive: no match for the uppercase query.
        let upper = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"NEEDLE"}),
            &plan_limits(4096, 60),
            &token,
        )
        .unwrap();
        assert!(upper["matches"].as_array().unwrap().is_empty());
        assert_eq!(upper["warnings"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn plan_search_pagination_covers_every_result_without_overlap() {
        let dir = tempfile::tempdir().unwrap();
        let mut content = String::new();
        for _ in 0..5 {
            content.push_str("hit\n");
        }
        fs::write(dir.path().join("hits.txt"), &content).unwrap();
        let token = CancellationToken::new();
        let mut offset = 0usize;
        let mut lines = Vec::new();
        loop {
            let result = plan_run(
                "plan_search",
                dir.path(),
                json!({"query":"hit","limit":2,"offset":offset}),
                &plan_limits(4096, 60),
                &token,
            )
            .unwrap();
            let matches = result["matches"].as_array().unwrap();
            for entry in matches {
                lines.push(entry["line"].as_u64().unwrap());
            }
            if result["truncated"] == true {
                assert_eq!(matches.len(), 2);
                offset = result["next_offset"].as_u64().unwrap() as usize;
            } else {
                assert_eq!(result["next_offset"], Value::Null);
                break;
            }
        }
        assert_eq!(lines, vec![1, 2, 3, 4, 5]);
        // An offset past the end yields a clean empty page.
        let past_end = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"hit","offset":999}),
            &plan_limits(4096, 60),
            &token,
        )
        .unwrap();
        assert!(past_end["matches"].as_array().unwrap().is_empty());
        assert_eq!(past_end["next_offset"], Value::Null);
        assert_eq!(past_end["truncated"], false);
    }

    #[test]
    fn plan_find_lists_files_matching_relative_path_substring() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("agent.rs"), "x\n").unwrap();
        fs::write(dir.path().join("main.rs"), "x\n").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/agent_helper.rs"), "x\n").unwrap();
        fs::create_dir(dir.path().join("agent")).unwrap();
        fs::write(dir.path().join("agent/inner.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink("agent.rs", dir.path().join("link.rs")).unwrap();
        let token = CancellationToken::new();
        let result = plan_run(
            "plan_find",
            dir.path(),
            json!({"query":"agent"}),
            &plan_limits(4096, 60),
            &token,
        )
        .unwrap();
        // The directory "agent" itself is not a result; its inner file is.
        // The symlink is excluded; regular files only.
        let mut actual = plan_match_paths(&result);
        actual.sort_unstable();
        let mut expected = vec!["agent/inner.txt", "agent.rs", "sub/agent_helper.rs"];
        expected.sort_unstable();
        assert_eq!(actual, expected);
        // An empty query enumerates every regular file.
        let all = plan_run(
            "plan_find",
            dir.path(),
            json!({"query":""}),
            &plan_limits(4096, 60),
            &token,
        )
        .unwrap();
        let mut actual_all = plan_match_paths(&all);
        actual_all.sort_unstable();
        let mut expected_all = vec![
            "agent/inner.txt",
            "agent.rs",
            "main.rs",
            "sub/agent_helper.rs",
        ];
        expected_all.sort_unstable();
        assert_eq!(actual_all, expected_all);
    }

    #[test]
    fn plan_find_pagination_is_deterministic() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["f1.txt", "f2.txt", "f3.txt", "f4.txt", "f5.txt"] {
            fs::write(dir.path().join(name), "x\n").unwrap();
        }
        let token = CancellationToken::new();
        let mut offset = 0usize;
        let mut seen = Vec::new();
        loop {
            let result = plan_run(
                "plan_find",
                dir.path(),
                json!({"query":"txt","limit":2,"offset":offset}),
                &plan_limits(4096, 60),
                &token,
            )
            .unwrap();
            let paths = result["paths"].as_array().unwrap();
            for path in paths {
                seen.push(path.as_str().unwrap().to_owned());
            }
            if result["truncated"] == true {
                assert_eq!(paths.len(), 2);
                offset = result["next_offset"].as_u64().unwrap() as usize;
            } else {
                assert_eq!(result["next_offset"], Value::Null);
                break;
            }
        }
        // Pages cover the sorted traversal exactly once, in order.
        assert_eq!(seen, vec!["f1.txt", "f2.txt", "f3.txt", "f4.txt", "f5.txt"]);
    }

    #[test]
    fn plan_search_respects_ignore_files_hidden_and_git_entries() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(dir.path().join("ignored.txt"), "needle\n").unwrap();
        fs::write(dir.path().join(".hidden"), "needle\n").unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        fs::write(dir.path().join(".git/config"), "needle\n").unwrap();
        fs::write(dir.path().join("normal.txt"), "needle\n").unwrap();
        let token = CancellationToken::new();
        let paths = |extra: Value| {
            let mut args = json!({"query":"needle"});
            let object = args.as_object_mut().unwrap();
            for (key, value) in extra.as_object().unwrap() {
                object.insert(key.clone(), value.clone());
            }
            let result = plan_run(
                "plan_search",
                dir.path(),
                args,
                &plan_limits(4096, 60),
                &token,
            )
            .unwrap();
            plan_match_paths(&result)
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<String>>()
        };
        assert_eq!(paths(json!({})), vec!["normal.txt"]);
        assert_eq!(paths(json!({"hidden":true})), vec![".hidden", "normal.txt"]);
        assert_eq!(
            paths(json!({"no_ignore":true})),
            vec!["ignored.txt", "normal.txt"]
        );
        // .git is always skipped, even with both expansion flags set.
        assert_eq!(
            paths(json!({"hidden":true,"no_ignore":true})),
            vec![".hidden", "ignored.txt", "normal.txt"]
        );
    }

    #[test]
    fn plan_search_skips_binary_files_and_counts_warning_overflow() {
        let dir = tempfile::tempdir().unwrap();
        for index in 0..12 {
            fs::write(dir.path().join(format!("bin{index:02}.dat")), b"\x00needle").unwrap();
        }
        let result = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"needle"}),
            &plan_limits(4096, 60),
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(result["matches"].as_array().unwrap().is_empty());
        let warnings = result["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 10);
        assert_eq!(result["warnings_omitted"], 2);
        for warning in warnings {
            assert!(warning["path"].as_str().unwrap().starts_with("bin"));
            assert!(warning["reason"].as_str().unwrap().contains("NUL"));
        }
    }

    #[test]
    fn plan_search_skips_files_with_lines_over_read_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("long.txt"),
            format!("{}\nneedle\n", "x".repeat(100)),
        )
        .unwrap();
        fs::write(dir.path().join("short.txt"), "needle\n").unwrap();
        let result = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"needle"}),
            &plan_limits(32, 60),
            &CancellationToken::new(),
        )
        .unwrap();
        // The whole file is skipped once one line overflows the buffer bound.
        assert_eq!(plan_match_paths(&result), vec!["short.txt"]);
        let warnings = result["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["path"], "long.txt");
        assert!(
            warnings[0]["reason"]
                .as_str()
                .unwrap()
                .contains("read_max_bytes")
        );
    }

    #[test]
    fn plan_search_skips_special_files_and_rejects_special_roots() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("pipe");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        fs::write(dir.path().join("text.txt"), "needle\n").unwrap();
        let result = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"needle"}),
            &plan_limits(4096, 60),
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(plan_match_paths(&result), vec!["text.txt"]);
        let warnings = result["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["path"], "pipe");
        // A FIFO root is an explicit error and must not block on open.
        let error = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"needle","path":"pipe"}),
            &plan_limits(4096, 60),
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("regular file or directory"));
    }

    #[test]
    fn plan_search_rejects_missing_blank_and_oversized_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let token = CancellationToken::new();
        let limits = plan_limits(4096, 60);
        for (args, expected) in [
            (
                json!({"query":"x","path":"missing"}),
                "search root not found",
            ),
            (json!({"query":"x","path":"  "}), "path must not be blank"),
            (json!({"query":""}), "query must not be empty"),
            (
                json!({"query":"x","limit":0}),
                "limit must be between 1 and 500",
            ),
            (
                json!({"query":"x","limit":501}),
                "limit must be between 1 and 500",
            ),
            (json!({"query":"x","regex":true}), "unknown field"),
        ] {
            let error = plan_run("plan_search", dir.path(), args, &limits, &token).unwrap_err();
            let message = format!("{error:#}");
            assert!(
                message.contains(expected),
                "expected {expected} in {message}"
            );
        }
    }

    #[test]
    fn plan_search_scans_explicit_file_roots_by_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "needle\nother\nneedle two\n").unwrap();
        fs::write(dir.path().join("b.txt"), "needle\n").unwrap();
        let result = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"needle","path":"a.txt"}),
            &plan_limits(4096, 60),
            &CancellationToken::new(),
        )
        .unwrap();
        let matches = result["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0]["path"], "a.txt");
        assert_eq!(matches[0]["line"], 1);
        assert_eq!(matches[1]["path"], "a.txt");
        assert_eq!(matches[1]["line"], 3);
    }

    #[test]
    fn plan_search_decodes_invalid_utf8_lines_lossily() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("raw.txt"), b"\xffneedle\n").unwrap();
        let result = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"needle"}),
            &plan_limits(4096, 60),
            &CancellationToken::new(),
        )
        .unwrap();
        let matches = result["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0]["line"], 1);
        assert!(matches[0]["text"].as_str().unwrap().contains("needle"));
    }

    #[test]
    fn plan_search_output_limit_pages_without_splitting_entries() {
        let dir = tempfile::tempdir().unwrap();
        let line = "x".repeat(1000);
        let mut content = String::new();
        for _ in 0..100 {
            content.push_str(&line);
            content.push('\n');
        }
        fs::write(dir.path().join("big.txt"), &content).unwrap();
        let token = CancellationToken::new();
        let mut offset = 0usize;
        let mut lines = Vec::new();
        let mut pages = 0;
        loop {
            let result = plan_run(
                "plan_search",
                dir.path(),
                json!({"query":"xxxx","offset":offset}),
                &plan_limits(8192, 60),
                &token,
            )
            .unwrap();
            assert!(serde_json::to_string(&result).unwrap().len() <= OUTPUT_LIMIT);
            let matches = result["matches"].as_array().unwrap();
            for entry in matches {
                // No half entries: path, line and text are all complete.
                assert_eq!(entry["path"], "big.txt");
                assert_eq!(entry["text"].as_str().unwrap().len(), 1000);
                lines.push(entry["line"].as_u64().unwrap());
            }
            if result["truncated"] == true {
                assert!(!matches.is_empty());
                offset = result["next_offset"].as_u64().unwrap() as usize;
            } else {
                assert_eq!(result["next_offset"], Value::Null);
                break;
            }
            pages += 1;
            assert!(pages < 100, "pagination did not terminate");
        }
        assert_eq!(lines, (1..=100).collect::<Vec<u64>>());
    }

    #[test]
    fn plan_search_omits_single_results_over_output_limit_with_warning() {
        let dir = tempfile::tempdir().unwrap();
        // The first line matches but its serialized result alone exceeds the
        // output limit; later smaller results in the same file still return.
        fs::write(
            dir.path().join("huge.txt"),
            format!("{}needle\nneedle small\n", "y".repeat(70_000)),
        )
        .unwrap();
        fs::write(dir.path().join("small.txt"), "needle\n").unwrap();
        let result = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"needle"}),
            &plan_limits(200_000, 60),
            &CancellationToken::new(),
        )
        .unwrap();
        let matches = result["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0]["path"], "huge.txt");
        assert_eq!(matches[0]["line"], 2);
        assert_eq!(matches[1]["path"], "small.txt");
        assert_eq!(result["truncated"], false);
        let warnings = result["warnings"].as_array().unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0]["path"], "huge.txt");
        assert!(
            warnings[0]["reason"]
                .as_str()
                .unwrap()
                .contains("output limit")
        );
    }

    #[test]
    fn plan_search_reports_cancelled_tokens_as_cancelled_errors() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let error = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"needle"}),
            &plan_limits(4096, 60),
            &token,
        )
        .unwrap_err();
        assert!(error.chain().any(|cause| cause.is::<Cancelled>()));
    }

    #[test]
    fn plan_search_times_out_when_the_configured_budget_is_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        let error = plan_run(
            "plan_search",
            dir.path(),
            json!({"query":"needle"}),
            &plan_limits(4096, 0),
            &CancellationToken::new(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Plan search timed out."));
    }
}
