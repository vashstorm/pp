//! 本地路径搜索，使用 pattern 匹配表达式和可选 path 搜索目录。
use anyhow::{Context, Result, bail, ensure};
use async_openai::types::chat::FunctionObject;
use globset::{GlobBuilder, GlobMatcher};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
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
        description: Some("Find local filesystem paths matching a glob pattern. pattern is required (e.g. '**/*.rs'); path is an optional directory, defaulting to the current working directory. Example: {\"pattern\":\"**/*.rs\",\"path\":\"src\"}. Never put the glob pattern in path. Bare *.rs searches recursively; src/*.rs matches direct children; src/**/*.rs searches recursively. Includes hidden paths and respects ignore rules by default. Results include directories, are ranked by modification time, and are limited to 200 paths; narrow path or pattern when incomplete. No shell or filesystem sandbox.".into()),
        parameters: Some(json!({
            "type":"object",
            "properties":{
                "pattern":{"type":"string","description":"Required glob pattern, e.g. **/*.rs or src/*.rs. One pattern per call; use braces for alternatives."},
                "path":{"type":"string","description":"Directory to search in, absolute or relative to cwd. Defaults to cwd. This is a literal directory, not a glob pattern."},
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
fn add(entries: &mut BTreeMap<String, Entry>, item: Entry, limit: usize, more: &mut bool) {
    entries.insert(item.path.clone(), item);
    if entries.len() > limit {
        *more = true;
        if let Some(worst) = entries
            .values()
            .max_by(|a, b| rank(a, b))
            .map(|e| e.path.clone())
        {
            entries.remove(&worst);
        }
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

pub(super) fn run(cwd: &Path, arguments: &str, token: &CancellationToken) -> Result<Value> {
    run_with_timeout(cwd, arguments, token, TIMEOUT)
}
fn run_with_timeout(
    cwd: &Path,
    arguments: &str,
    token: &CancellationToken,
    timeout: Duration,
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
    search(cwd, args, target, token, requested, limit, || {
        Instant::now() >= deadline
    })
}

fn search(
    cwd: &Path,
    args: Args,
    target: Target,
    token: &CancellationToken,
    requested: f64,
    limit: usize,
    mut expired: impl FnMut() -> bool,
) -> Result<Value> {
    let mut entries = BTreeMap::new();
    let mut warnings = Vec::new();
    let mut timed_out = false;
    let mut more = false;
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
                        && (allow_node_modules || entry.file_name() != "node_modules"))
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
            add(
                &mut entries,
                Entry {
                    path: display(cwd, &root.join(relative), dir),
                    mtime,
                },
                limit,
                &mut more,
            );
        }
    }
    let mut matches: Vec<_> = entries.into_values().collect();
    matches.sort_by(rank);
    more |= matches.len() == limit;
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
        let timeout = run_with_timeout(
            root,
            r#"{"pattern":"**/*"}"#,
            &CancellationToken::new(),
            Duration::ZERO,
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
        let result = search(
            root,
            args,
            parse_target(root, "**/*"),
            &CancellationToken::new(),
            200.0,
            200,
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
}
