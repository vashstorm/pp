//! 本地路径搜索，使用 pattern 匹配表达式和可选 path 搜索目录。
use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use globset::{GlobBuilder, GlobMatcher};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

const MAX_RESULTS: usize = 200;
const MAX_BYTES: usize = 50 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn definition() -> FunctionObject {
    FunctionObject {
        name: "glob".into(),
        description: Some("Find local paths by glob: required pattern (e.g. '**/*.rs'), optional directory path (default cwd). Bare *.rs recurses; src/*.rs matches direct children. Includes hidden paths and directories; respects ignore rules. Ranked by mtime; at most 200 paths and 50 KiB. Narrow the search if incomplete. No filesystem sandbox.".into()),
        parameters: Some(json!({
            "type":"object",
            "properties":{
                "pattern":{"type":"string","description":"Glob pattern, e.g. **/*.rs."},
                "path":{"type":"string","description":"Literal search directory, not a glob; defaults to cwd."},
                "hidden":{"type":"boolean"},
                "gitignore":{"type":"boolean"},
                "limit":{"type":"number"}
            },
            "required":["pattern"],
            "additionalProperties":false
        })),
        ..Default::default()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    pattern: String,
    path: Option<String>,
    hidden: Option<bool>,
    gitignore: Option<bool>,
    limit: Option<f64>,
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
struct Target {
    root: PathBuf,
    pattern: String,
}

fn parse_target(cwd: &Path, input: &str) -> Target {
    let normalized = input.replace('\\', "/");
    let segments: Vec<_> = normalized.split('/').collect();
    let first = segments
        .iter()
        .position(|part| part.contains(['*', '?', '[', '{']))
        .unwrap_or(segments.len().saturating_sub(1));
    let (base, pattern) = if !normalized.contains('/') {
        (".".to_owned(), format!("**/{normalized}"))
    } else {
        let mut base = segments[..first].join("/");
        if base.is_empty() {
            base.push_str(if normalized.starts_with('/') {
                "/"
            } else {
                "."
            });
        }
        (base, segments[first..].join("/"))
    };
    Target {
        root: normalize(&cwd.join(base)),
        pattern,
    }
}

fn compile(pattern: &str) -> Result<GlobMatcher> {
    let mut pattern = pattern.to_owned();
    let open = pattern.chars().filter(|c| *c == '{').count();
    let close = pattern.chars().filter(|c| *c == '}').count();
    pattern.extend(std::iter::repeat_n('}', open.saturating_sub(close)));
    Ok(GlobBuilder::new(&pattern)
        .literal_separator(true)
        .build()
        .context("invalid glob pattern")?
        .compile_matcher())
}

#[derive(Clone)]
struct Entry {
    path: String,
    mtime: SystemTime,
}
fn rank(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    b.mtime.cmp(&a.mtime).then_with(|| a.path.cmp(&b.path))
}
fn display(cwd: &Path, path: &Path, dir: bool) -> String {
    let path = path.strip_prefix(cwd).unwrap_or(path);
    let mut text = path.to_string_lossy().into_owned();
    if dir && !text.ends_with('/') {
        text.push('/');
    }
    text
}
/// 有界候选缓冲：保留 `rank` 最高的至多 `limit` 项。
///
/// 候选按遍历顺序追加，缓冲超过 `2 × limit` 时才整体按 `rank` 排序并截断，
/// 每个候选的摊销成本为 O(1)（旧实现每多出一个候选就要扫一遍已选集合找最差项，
/// 也就是 O(limit)）。由于路径互不相同，`rank` 在候选集合上是全序，
/// top-limit 与淘汰顺序无关，因此选出集合与逐次淘汰最差项的实现逐位相同。
///
/// 不在“已选满 limit”时提前结束遍历：`rank` 只依据 mtime 与路径，与遍历顺序、
/// 条目深度无关（readdir 顺序也不是路径序），任何基于层级或已访问前缀的提前
/// 终止都可能丢掉尚未访问但更新（rank 更高）的候选。保持既有语义：不超时、
/// 不取消时结果恒为全部候选里 rank 最高的 `limit` 项。
struct Results {
    entries: Vec<Entry>,
    limit: usize,
    more: bool,
}
impl Results {
    fn new(limit: usize) -> Self {
        Self {
            entries: Vec::with_capacity(limit.saturating_add(1)),
            limit,
            more: false,
        }
    }
    fn push(&mut self, item: Entry) {
        self.entries.push(item);
        if self.entries.len() > self.limit.saturating_mul(2) {
            self.reduce();
        }
    }
    /// 按 `rank` 排序（最靠前的最优）并截断到 `limit`，丢弃即置 `more`。
    fn reduce(&mut self) {
        self.entries.sort_by(rank);
        if self.entries.len() > self.limit {
            self.more = true;
            self.entries.truncate(self.limit);
        }
    }
    fn finish(mut self) -> (Vec<Entry>, bool) {
        self.reduce();
        let more = self.more || self.entries.len() == self.limit;
        (self.entries, more)
    }
}

#[derive(Default)]
struct Tree {
    files: Vec<String>,
    dirs: Vec<(String, Tree)>,
}
impl Tree {
    fn add(&mut self, path: &str) {
        let dir = path.ends_with('/');
        let path = path.strip_suffix('/').unwrap_or(path);
        let parts: Vec<_> = path.split('/').collect();
        let count = if dir {
            parts.len()
        } else {
            parts.len().saturating_sub(1)
        };
        let mut node = self;
        for part in &parts[..count] {
            let index = node
                .dirs
                .iter()
                .position(|(name, _)| name == part)
                .unwrap_or_else(|| {
                    node.dirs.push(((*part).to_owned(), Tree::default()));
                    node.dirs.len() - 1
                });
            node = &mut node.dirs[index].1;
        }
        if !dir && let Some(name) = parts.last() {
            node.files.push((*name).to_owned());
        }
    }
    fn render(&self, depth: usize, out: &mut Vec<String>) {
        out.extend(self.files.iter().cloned());
        for (name, child) in &self.dirs {
            let mut parts = vec![name.as_str()];
            let mut next = child;
            while next.files.is_empty() && next.dirs.len() == 1 {
                let (name, child) = &next.dirs[0];
                parts.push(name);
                next = child;
            }
            out.push(format!("{} {}/", "#".repeat(depth + 1), parts.join("/")));
            next.render(depth + 1, out);
        }
    }
}
fn grouped(items: &[Entry]) -> String {
    let mut tree = Tree::default();
    for item in items {
        tree.add(&item.path);
    }
    let mut lines = Vec::new();
    tree.render(0, &mut lines);
    lines.join("\n")
}

/// 遍历剪枝：普通 entry 走词法快路径；symlink 与无法取得类型的 entry 取规范化
/// 结果，避免链接指向拒绝根时被词法前缀误放行（`follow_links(false)` 已保证
/// 不跨越目录链接）。
fn sandbox_denied_entry(sandbox: &pp_sandbox::Sandbox, entry: &ignore::DirEntry) -> bool {
    match entry.file_type() {
        Some(kind) if !kind.is_symlink() => sandbox.is_read_denied(entry.path()),
        _ => sandbox.is_read_denied_resolved(entry.path()),
    }
}

pub(super) fn run_with_sandbox(
    cwd: &Path,
    arguments: &str,
    token: &CancellationToken,
    sandbox: &std::sync::Arc<pp_sandbox::Sandbox>,
) -> Result<Value> {
    run_with_timeout(cwd, arguments, token, TIMEOUT, sandbox)
}
#[cfg(test)]
fn run(cwd: &Path, arguments: &str, token: &CancellationToken) -> Result<Value> {
    run_with_sandbox(
        cwd,
        arguments,
        token,
        &std::sync::Arc::new(pp_sandbox::Sandbox::disabled()),
    )
}
fn run_with_timeout(
    cwd: &Path,
    arguments: &str,
    token: &CancellationToken,
    timeout: Duration,
    sandbox: &std::sync::Arc<pp_sandbox::Sandbox>,
) -> Result<Value> {
    cancellation::check(token)?;
    let args: Args = serde_json::from_str(arguments).context("invalid glob arguments")?;
    let requested = args.limit.unwrap_or(MAX_RESULTS as f64);
    ensure!(
        requested.is_finite() && requested > 0.0,
        "limit must be a positive number"
    );
    let limit = requested.floor().clamp(1.0, MAX_RESULTS as f64) as usize;
    ensure!(
        !args.pattern.trim().is_empty() && !args.pattern.contains('\0'),
        "pattern must be non-empty and contain no NUL bytes"
    );
    compile(&args.pattern)?;
    let path = args.path.as_deref().unwrap_or(".");
    ensure!(
        !path.trim().is_empty() && !path.contains('\0'),
        "path must be a non-empty directory and contain no NUL bytes; put the glob in pattern"
    );
    let directory = normalize(&cwd.join(path));
    ensure!(
        directory.is_dir(),
        "path must be an existing directory; put the glob in pattern"
    );
    let target = parse_target(&directory, &args.pattern);
    let deadline = Instant::now() + timeout;
    search(cwd, args, target, token, requested, limit, sandbox, || {
        Instant::now() >= deadline
    })
}

#[allow(clippy::too_many_arguments)]
fn search(
    cwd: &Path,
    args: Args,
    target: Target,
    token: &CancellationToken,
    requested: f64,
    limit: usize,
    sandbox: &std::sync::Arc<pp_sandbox::Sandbox>,
    mut expired: impl FnMut() -> bool,
) -> Result<Value> {
    let mut results = Results::new(limit);
    let mut warnings = Vec::new();
    let mut timed_out = false;
    let root = target.root;
    if root == Path::new("/") {
        bail!("Searching from root directory '/' is not allowed");
    }
    let metadata = match fs::metadata(&root) {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("cannot access glob search path"),
    };
    if let Some(metadata) = metadata {
        ensure!(metadata.is_dir(), "glob search path is not a directory");
        let matcher = compile(&target.pattern)?;
        let walk_root = fs::canonicalize(&root).context("cannot resolve glob search path")?;
        sandbox.check_read(&walk_root)?;
        let filter_sandbox = std::sync::Arc::clone(sandbox);
        let in_repo = walk_root
            .ancestors()
            .any(|ancestor| ancestor.join(".git").exists());
        let depth = if target.pattern.contains("**") || target.pattern.contains('{') {
            None
        } else {
            Some(
                target
                    .pattern
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .count()
                    .max(1),
            )
        };
        let allow_node_modules = target.pattern.contains("node_modules")
            || root.components().any(|c| c.as_os_str() == "node_modules");
        let respect = args.gitignore.unwrap_or(true);
        let mut builder = ignore::WalkBuilder::new(&walk_root);
        builder
            .hidden(!args.hidden.unwrap_or(true))
            .follow_links(false)
            .ignore(respect)
            .git_ignore(respect)
            .git_exclude(respect)
            .git_global(respect)
            .require_git(false)
            .parents(in_repo)
            .max_depth(depth)
            .filter_entry(move |entry| {
                entry.depth() == 0
                    || (entry.file_name() != ".git"
                        && (allow_node_modules || entry.file_name() != "node_modules")
                        && !sandbox_denied_entry(&filter_sandbox, entry))
            });
        for result in builder.build() {
            cancellation::check(token)?;
            if expired() {
                timed_out = true;
                break;
            }
            let entry = match result {
                Ok(entry) => entry,
                Err(error) => {
                    if warnings.len() < 10 {
                        warnings.push(format!("Skipped entry: {error}"));
                    }
                    continue;
                }
            };
            if entry.depth() == 0 {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&walk_root)
                .unwrap_or(entry.path());
            if !matcher.is_match(relative) {
                continue;
            }
            let dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            results.push(Entry {
                path: display(cwd, &root.join(relative), dir),
                mtime,
            });
        }
    }
    let (matches, more) = results.finish();
    let mut content = if matches.is_empty() && !timed_out {
        "No files found matching pattern".to_owned()
    } else {
        grouped(&matches)
    };
    let mut truncated = more || timed_out;
    if content.len() > MAX_BYTES {
        let mut end = MAX_BYTES;
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
        truncated = true;
        warnings.push("Output truncated at 50 KiB".to_owned());
    }
    if more {
        warnings.push(format!(
            "{limit} results limit reached; narrow the search scope"
        ));
    }
    if timed_out {
        warnings.push("glob timed out after 5 seconds; scan is incomplete and is not proof of absence; search a deeper directory".into());
    }
    if requested > MAX_RESULTS as f64 {
        warnings.push(format!(
            "Requested limit {requested} clamped to the max of {MAX_RESULTS}"
        ));
    }
    Ok(
        json!({"ok":true,"content":content,"truncated":truncated,"timed_out":timed_out,"warnings":warnings}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pp_config::config::{SandboxMode, SandboxSettings};
    use pp_sandbox::{ProbeResult, Sandbox};
    use std::os::unix::fs::symlink;

    fn call(root: &Path, args: Value) -> Value {
        run(root, &args.to_string(), &CancellationToken::new()).unwrap()
    }
    fn text(result: &Value) -> &str {
        result["content"].as_str().unwrap()
    }
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("src/nested")).unwrap();
        fs::write(dir.path().join("root.rs"), "").unwrap();
        fs::write(dir.path().join("src/a.rs"), "").unwrap();
        fs::write(dir.path().join("src/nested/b.rs"), "").unwrap();
        fs::write(dir.path().join("src/nested/c.txt"), "").unwrap();
        dir
    }

    #[test]
    fn glob_patterns_scope_and_exact_paths() {
        let dir = fixture();
        let root = dir.path();
        let bare = call(root, json!({"pattern":"*.rs"}));
        for name in ["root.rs", "a.rs", "b.rs"] {
            assert!(text(&bare).contains(name), "{bare}");
        }
        let direct = call(root, json!({"pattern":"src/*.rs"}));
        assert!(text(&direct).contains("a.rs"));
        assert!(!text(&direct).contains("b.rs"));
        assert!(text(&call(root, json!({"pattern":"src/**/*.rs"}))).contains("b.rs"));
        assert!(text(&call(root, json!({"pattern":"src/nested/{b.rs,c.txt"}))).contains("c.txt"));
        assert_eq!(text(&call(root, json!({"pattern":"root.rs"}))), "root.rs");
        assert!(text(&call(root, json!({"pattern":"**/*","path":"src"}))).contains("# nested/"));
        assert!(run(root, r#"{"pattern":"/*.rs"}"#, &CancellationToken::new()).is_err());
    }

    #[test]
    fn glob_pattern_and_literal_directory_are_separate() {
        let dir = fixture();
        let root = dir.path();
        let scoped = call(root, json!({"pattern":"**/*.rs","path":"src"}));
        assert!(text(&scoped).contains("a.rs"));
        assert!(text(&scoped).contains("b.rs"));
        assert!(!text(&scoped).contains("root.rs"));
        assert!(!text(&scoped).contains("c.txt"));
        let direct = call(root, json!({"pattern":"*.rs","path":root.join("src")}));
        assert_eq!(direct["content"], scoped["content"]);
        fs::create_dir_all(root.join("folder with spaces;commas,[]")).unwrap();
        fs::write(root.join("folder with spaces;commas,[]/a,b.txt"), "").unwrap();
        let literal = call(
            root,
            json!({"pattern":"*.txt","path":"folder with spaces;commas,[]"}),
        );
        assert!(text(&literal).contains("a,b.txt"));
        for pattern in ["missing.rs", "missing/*.rs"] {
            assert_eq!(
                text(&call(root, json!({"pattern":pattern}))),
                "No files found matching pattern"
            );
        }
    }

    #[test]
    fn glob_hidden_ignore_and_node_modules() {
        let dir = fixture();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
        fs::write(root.join("ignored.rs"), "").unwrap();
        fs::write(root.join(".hidden.rs"), "").unwrap();
        fs::create_dir_all(root.join("node_modules/lib")).unwrap();
        fs::write(root.join("node_modules/lib/dep.rs"), "").unwrap();
        fs::create_dir(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "").unwrap();
        let result = call(root, json!({"pattern":"*.rs"}));
        assert!(text(&result).contains(".hidden.rs"));
        assert!(!text(&result).contains("ignored.rs"));
        assert!(!text(&result).contains("dep.rs"));
        let result = call(
            root,
            json!({"pattern":"*.rs","hidden":false,"gitignore":false}),
        );
        assert!(!text(&result).contains(".hidden.rs"));
        assert!(text(&result).contains("ignored.rs"));
        let result = call(
            root,
            json!({"pattern":"node_modules/**/*.rs","gitignore":false}),
        );
        assert!(text(&result).contains("dep.rs"), "{result}");
        assert!(!text(&call(root, json!({"pattern":"**/*","gitignore":false}))).contains("config"));
    }

    #[test]
    fn glob_symlinks_are_listed_but_not_followed() {
        let dir = fixture();
        let root = dir.path();
        symlink("src", root.join("link")).unwrap();
        symlink("root.rs", root.join("file-link")).unwrap();
        symlink("missing", root.join("broken")).unwrap();
        let result = call(root, json!({"pattern":"**/*"}));
        assert!(text(&result).contains("link"), "{result}");
        assert!(text(&result).contains("broken"), "{result}");
        assert!(!text(&result).contains("# link/"), "{result}");
        let explicit = call(root, json!({"pattern":"link/*.rs"}));
        assert!(text(&explicit).contains("a.rs"), "{explicit}");
        assert_eq!(
            text(&call(root, json!({"pattern":"file-link"}))),
            "file-link"
        );
    }

    #[test]
    fn glob_limit_cancel_timeout_and_validation() {
        let dir = fixture();
        let root = dir.path();
        let result = call(root, json!({"pattern":"*.rs","limit":1}));
        assert_eq!(result["truncated"], true);
        assert!(result["warnings"].to_string().contains("limit reached"));
        assert!(
            call(root, json!({"pattern":"root.rs","limit":201}))["warnings"]
                .to_string()
                .contains("clamped")
        );
        for args in [
            json!({"pattern":""}),
            json!({"pattern":"["}),
            json!({"pattern":"**/*","limit":0}),
            json!({}),
            json!({"path":"src"}),
            json!({"pattern":"**/*","path":"src/*.rs"}),
            json!({"pattern":"**/*","path":"root.rs"}),
            json!({"pattern":"**/*","path":"missing"}),
            json!({"pattern":"**/*","path":""}),
            json!({"pattern":"**/*","path":"\0"}),
            json!({"pattern":"\0"}),
            json!({"extra":1}),
        ] {
            assert!(
                run(root, &args.to_string(), &CancellationToken::new()).is_err(),
                "{args}"
            );
        }
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(
            run(root, "{}", &cancelled)
                .unwrap_err()
                .is::<pp_core::cancellation::Cancelled>()
        );
        let disabled = std::sync::Arc::new(pp_sandbox::Sandbox::disabled());
        let timeout = run_with_timeout(
            root,
            r#"{"pattern":"**/*"}"#,
            &CancellationToken::new(),
            Duration::ZERO,
            &disabled,
        )
        .unwrap();
        assert_eq!(timeout["timed_out"], true);
        assert!(!text(&timeout).contains("No files found"));
    }

    #[test]
    fn glob_rank_and_partial_timeout_are_deterministic() {
        let dir = fixture();
        let root = dir.path();
        for (name, seconds) in [("root.rs", 30), ("src/a.rs", 20), ("src/nested/b.rs", 10)] {
            let file = fs::File::options()
                .write(true)
                .open(root.join(name))
                .unwrap();
            file.set_times(
                fs::FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
            )
            .unwrap();
        }
        let result = call(root, json!({"pattern":"*.rs","limit":2}));
        assert!(text(&result).contains("root.rs"));
        assert!(text(&result).contains("a.rs"));
        assert!(!text(&result).contains("b.rs"));
        let args: Args = serde_json::from_value(json!({"pattern":"**/*"})).unwrap();
        let mut checkpoints = 0;
        let disabled = std::sync::Arc::new(pp_sandbox::Sandbox::disabled());
        let result = search(
            root,
            args,
            parse_target(root, "**/*"),
            &CancellationToken::new(),
            200.0,
            200,
            &disabled,
            || {
                checkpoints += 1;
                checkpoints > 2
            },
        )
        .unwrap();
        assert_eq!(result["timed_out"], true);
        assert!(!text(&result).is_empty());
        assert!(!text(&result).contains("No files found"));
    }

    /// 与被替换的逐次淘汰最差项实现逐位对照：候选集合、limit 与遍历顺序全部变化。
    fn legacy_selection(candidates: &[Entry], limit: usize) -> (Vec<String>, bool) {
        let mut entries: std::collections::BTreeMap<String, Entry> =
            std::collections::BTreeMap::new();
        let mut more = false;
        for item in candidates {
            entries.insert(item.path.clone(), item.clone());
            if entries.len() > limit {
                more = true;
                let worst = entries
                    .values()
                    .max_by(|a, b| rank(a, b))
                    .map(|entry| entry.path.clone());
                if let Some(worst) = worst {
                    entries.remove(&worst);
                }
            }
        }
        let mut kept: Vec<_> = entries.into_values().collect();
        kept.sort_by(rank);
        let more = more || kept.len() == limit;
        (kept.into_iter().map(|entry| entry.path).collect(), more)
    }

    fn selection(candidates: &[Entry], limit: usize) -> (Vec<String>, bool) {
        let mut results = Results::new(limit);
        for item in candidates {
            results.push(item.clone());
        }
        let (kept, more) = results.finish();
        (kept.into_iter().map(|entry| entry.path).collect(), more)
    }

    /// 确定性洗牌（xorshift64），不用额外依赖；只用于打乱遍历到达顺序。
    fn shuffled(items: &[Entry], seed: u64) -> Vec<Entry> {
        let mut out = items.to_vec();
        let mut state = seed | 1;
        for index in (1..out.len()).rev() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            out.swap(index, (state % (index as u64 + 1)) as usize);
        }
        out
    }

    #[test]
    fn glob_limit_selection_matches_legacy_eviction() {
        // 路径互不相同，mtime 有刻意重复的平局项（按路径升序进入前列）。
        let pool: Vec<Entry> = [30_u64, 10, 20, 20, 10, 30, 5]
            .iter()
            .enumerate()
            .map(|(index, seconds)| Entry {
                path: format!("d{}/f{index}.rs", index % 3),
                mtime: SystemTime::UNIX_EPOCH + Duration::from_secs(*seconds),
            })
            .collect();
        assert_eq!(
            selection(&pool, 2),
            (vec!["d0/f0.rs".to_owned(), "d2/f5.rs".to_owned()], true)
        );
        assert_eq!(
            selection(&pool, 3),
            (
                vec![
                    "d0/f0.rs".to_owned(),
                    "d2/f5.rs".to_owned(),
                    "d0/f3.rs".to_owned()
                ],
                true
            )
        );
        for limit in [1, 2, 3, 5, 7, 8] {
            let expected = selection(&pool, limit);
            assert_eq!(expected, legacy_selection(&pool, limit), "limit={limit}");
            for seed in [1, 2, 3, 4, 7, 11, 23] {
                let order = shuffled(&pool, seed);
                assert_eq!(
                    selection(&order, limit),
                    expected,
                    "limit={limit} seed={seed}"
                );
                assert_eq!(
                    legacy_selection(&order, limit),
                    expected,
                    "limit={limit} seed={seed}"
                );
            }
        }
    }

    #[test]
    fn glob_limit_keeps_newest_files_and_flags_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // 创建顺序与 mtime 顺序相反，其中 c.rs/d.rs 同 mtime（平局按路径升序）。
        for (name, seconds) in [
            ("e.rs", 10_u64),
            ("d.rs", 20),
            ("c.rs", 20),
            ("b.rs", 40),
            ("a.rs", 50),
        ] {
            fs::write(root.join(name), "").unwrap();
            fs::File::options()
                .write(true)
                .open(root.join(name))
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
                )
                .unwrap();
        }
        let result = call(root, json!({"pattern":"*.rs","limit":3}));
        assert_eq!(text(&result), "a.rs\nb.rs\nc.rs");
        assert_eq!(result["truncated"], true);
        assert_eq!(result["timed_out"], false);
        assert!(result["warnings"].to_string().contains("limit reached"));
        let full = call(root, json!({"pattern":"*.rs","limit":6}));
        assert_eq!(full["truncated"], false);
        assert_eq!(text(&full), "a.rs\nb.rs\nc.rs\nd.rs\ne.rs");
    }

    #[test]
    fn glob_output_is_bounded_on_utf8_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("deep")).unwrap();
        for i in 0..200 {
            let name = format!("{i:03}{}", "界".repeat(84));
            fs::write(dir.path().join("deep").join(name), "").unwrap();
        }
        let result = call(dir.path(), json!({"pattern":"deep/*"}));
        assert!(text(&result).len() <= MAX_BYTES);
        assert!(text(&result).contains('界'));
        assert_eq!(result["truncated"], true);
        assert!(result["warnings"].to_string().contains("50 KiB"));
    }

    #[test]
    fn glob_sandbox_hides_entries_linked_into_deny_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let home = root.join("home");
        fs::create_dir_all(home.join(".ssh")).unwrap();
        fs::write(home.join(".ssh/id_rsa"), "").unwrap();
        fs::write(home.join("public.rs"), "").unwrap();
        fs::write(root.join("keep.rs"), "").unwrap();
        symlink(home.join(".ssh"), root.join("secret-link")).unwrap();
        symlink(home.join(".ssh/missing"), root.join("dangling")).unwrap();

        let sandbox = std::sync::Arc::new(Sandbox::with_probe_for_test(
            SandboxSettings {
                mode: SandboxMode::WorkspaceWrite,
                ..Default::default()
            },
            root,
            Some(&home),
            None,
            ProbeResult::Available,
        ));
        let result = run_with_sandbox(
            root,
            r#"{"pattern":"**/*"}"#,
            &CancellationToken::new(),
            &sandbox,
        )
        .unwrap();
        let listing = text(&result);
        assert!(listing.contains("keep.rs"), "{result}");
        assert!(listing.contains("public.rs"), "{result}");
        assert!(!listing.contains("secret-link"), "{result}");
        assert!(!listing.contains("dangling"), "{result}");
        assert!(!listing.contains("id_rsa"), "{result}");
    }
}
