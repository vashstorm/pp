//! `rg` 搜索范围解析：路径、glob、排除与行区间选择器。
use anyhow::{Context, Result, ensure};
use globset::{GlobBuilder, GlobMatcher};
use pp_core::cancellation::CancellationToken;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Instant,
};

use super::scan::check;

pub(super) struct Target {
    pub(super) path: PathBuf,
    pub(super) display: String,
    pub(super) ranges: Vec<(usize, usize)>,
    pub(super) explicit: bool,
}

pub(super) fn add_target(targets: &mut BTreeMap<PathBuf, Target>, mut target: Target) {
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

fn glob(pattern: &str) -> Result<GlobMatcher> {
    Ok(GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .context("invalid rg glob")?
        .compile_matcher())
}
fn glob_path(path: &str) -> bool {
    path.contains(['*', '?', '[', '{'])
}

/// 遍历剪枝：普通 entry 在与拒绝根不相交的遍历根下直接放行，否则走词法快路径；
/// symlink 与无法取得类型的 entry 取规范化结果，避免链接指向拒绝根时被词法前缀误放行
/// （`follow_links(false)` 已保证不跨越目录链接）。
fn sandbox_denied_entry(
    policy: Option<&pp_sandbox::Policy>,
    can_contain_denied: bool,
    entry: &ignore::DirEntry,
) -> bool {
    let Some(policy) = policy else {
        return false;
    };
    match entry.file_type() {
        Some(kind) if !kind.is_symlink() => {
            can_contain_denied && policy.is_read_denied(entry.path())
        }
        _ => policy.is_read_denied_resolved(entry.path()),
    }
}

pub(super) struct LazyWalk {
    walk: ignore::Walk,
    root: PathBuf,
    cwd: PathBuf,
    matcher: Option<GlobMatcher>,
    raw_plain: bool,
    exclude: Vec<GlobMatcher>,
    exclusions: Vec<String>,
    token: CancellationToken,
    deadline: Instant,
    aborted: bool,
}

impl Iterator for LazyWalk {
    type Item = Result<Target>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.aborted {
            return None;
        }
        for item in self.walk.by_ref() {
            if let Err(error) = check(&self.token, self.deadline) {
                self.aborted = true;
                return Some(Err(error));
            }
            let Ok(item) = item else { continue };
            if !item.file_type().is_some_and(|f| f.is_file()) {
                continue;
            }
            let path = item.path();
            let rel = path.strip_prefix(&self.root).unwrap_or(path);
            if let Some(m) = &self.matcher
                && !m.is_match(rel)
                && !(self.raw_plain && path.file_name().is_some_and(|n| m.is_match(n)))
            {
                continue;
            }
            if !self.exclude.is_empty()
                && self.exclude.iter().zip(&self.exclusions).any(|(m, e)| {
                    if e.contains('/') {
                        path.strip_prefix(&self.cwd)
                            .unwrap_or(path)
                            .ancestors()
                            .any(|candidate| m.is_match(candidate))
                    } else {
                        rel.components().any(|c| m.is_match(c.as_os_str()))
                    }
                })
            {
                continue;
            }
            let display = path
                .strip_prefix(&self.cwd)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            return Some(Ok(Target {
                path: path.to_owned(),
                display,
                ranges: vec![],
                explicit: false,
            }));
        }
        None
    }
}

pub(super) enum TargetIter {
    Walk(Box<LazyWalk>),
    List(std::vec::IntoIter<Target>),
}

impl Iterator for TargetIter {
    type Item = Result<Target>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Walk(walk) => walk.next(),
            Self::List(list) => list.next().map(Ok),
        }
    }
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
fn fold_macos_system_prefix(path: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        for alias in ["/tmp", "/var", "/etc"] {
            let prefix = Path::new(alias);
            if let Ok(rest) = path.strip_prefix(prefix) {
                let private = Path::new("/private").join(alias.trim_start_matches('/'));
                return if rest.as_os_str().is_empty() {
                    private
                } else {
                    private.join(rest)
                };
            }
        }
    }
    path.to_path_buf()
}

fn reject_symlink_components(path: &Path) -> Result<()> {
    let path = fold_macos_system_prefix(path);
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
fn expand_tilde_arg(cwd: &Path, s: &str) -> String {
    if cwd.join(s).exists() {
        return s.to_owned();
    }
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
}

fn resolve_scope_entries(cwd: &Path, input: &str) -> Result<(Vec<String>, Vec<String>)> {
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
    ensure!(!input.contains("://"), "rg only supports local paths");
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
        let s = expand_tilde_arg(cwd, &s);
        if s.starts_with('!') && !cwd.join(&s).exists() {
            exclusions.push(s[1..].to_owned());
        } else {
            entries.push(s);
        }
    }
    if entries.is_empty() {
        entries.push(".".into());
    }
    Ok((entries, exclusions))
}

fn split_entry_selector(cwd: &Path, entry: &str) -> Result<(String, Vec<(usize, usize)>)> {
    if cwd.join(entry).symlink_metadata().is_ok() {
        return Ok((entry.to_owned(), vec![]));
    }
    if let Some((p, r)) = entry.rsplit_once(':')
        && !r.is_empty()
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
        return Ok((p.to_owned(), ranges));
    }
    Ok((entry.to_owned(), vec![]))
}

/// 预解析 `rg` 参数指向的搜索根路径，供权限门在执行遍历前分类并审批会话/外部路径。
pub(in crate::tools) fn inspect_roots(cwd: &Path, arguments: &str) -> Result<Vec<PathBuf>> {
    let Ok(args) = serde_json::from_str::<super::Args>(arguments) else {
        return Ok(Vec::new());
    };
    if args
        .path
        .as_deref()
        .is_some_and(|p| super::super::read::is_pipe_uri(p) || p.starts_with("artifact://"))
    {
        return Ok(Vec::new());
    }
    let Ok((entries, _)) = resolve_scope_entries(cwd, args.path.as_deref().unwrap_or(".")) else {
        return Ok(Vec::new());
    };
    let mut roots = Vec::new();
    for entry in entries {
        let Ok((raw, _)) = split_entry_selector(cwd, &entry) else {
            continue;
        };
        let abs = cwd.join(&raw);
        let root = if abs.symlink_metadata().is_ok() || !glob_path(&raw) {
            abs
        } else {
            let ix = raw.find(['*', '?', '[', '{']).unwrap();
            let prefix = &raw[..ix];
            let slash = prefix.rfind('/');
            let base = slash.map_or(".", |i| &raw[..i]);
            cwd.join(if base.is_empty() { "/" } else { base })
        };
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    Ok(roots)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn parse_paths(
    cwd: &Path,
    input: &str,
    respect: bool,
    globs: &[String],
    types: Option<&ignore::types::Types>,
    token: &CancellationToken,
    deadline: Instant,
    policy: Option<std::sync::Arc<pp_sandbox::Policy>>,
) -> Result<(TargetIter, bool, Vec<String>)> {
    let (entries, exclusions) = resolve_scope_entries(cwd, input)?;
    let one_entry = entries.len() == 1;
    let mut targets = BTreeMap::new();
    let mut missing = Vec::new();
    let mut lazy_walk = None;
    for entry in entries {
        check(token, deadline)?;
        let (raw, ranges) = split_entry_selector(cwd, &entry)?;
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
                if let Some(policy) = &policy {
                    policy.check_read(&abs)?;
                }
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
        if let Some(policy) = &policy {
            policy.check_read(&root)?;
        }
        let can_contain_denied = policy
            .as_ref()
            .is_some_and(|p| p.can_contain_denied_read(&root));
        let matcher = pat.as_deref().map(glob).transpose()?;
        let exclude: Vec<_> = exclusions.iter().map(|e| glob(e)).collect::<Result<_>>()?;
        let filter_exclude = exclude.clone();
        let filter_patterns = exclusions.clone();
        let filter_root = root.clone();
        let filter_cwd = cwd.to_owned();
        let filter_policy = policy.clone();
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
                if sandbox_denied_entry(filter_policy.as_deref(), can_contain_denied, entry) {
                    return false;
                }
                if filter_exclude.is_empty() {
                    return true;
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
        if let Some(types) = types {
            builder.types(types.clone());
        }
        if !globs.is_empty() {
            let mut overrides = ignore::overrides::OverrideBuilder::new(&root);
            for glob in globs {
                overrides
                    .add(glob)
                    .map_err(|_| anyhow::anyhow!("invalid -g glob: {glob}"))?;
            }
            builder.overrides(
                overrides
                    .build()
                    .map_err(|error| anyhow::anyhow!("invalid -g glob: {error}"))?,
            );
        }
        let stream = LazyWalk {
            walk: builder.build(),
            root,
            cwd: cwd.to_owned(),
            matcher,
            raw_plain: !raw.contains('/'),
            exclude,
            exclusions: exclusions.clone(),
            token: token.clone(),
            deadline,
            aborted: false,
        };
        if one_entry {
            lazy_walk = Some(stream);
            break;
        }
        for target in stream {
            add_target(&mut targets, target?);
        }
    }
    if let Some(walk) = lazy_walk {
        return Ok((TargetIter::Walk(Box::new(walk)), false, missing));
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
    Ok((
        TargetIter::List(targets.into_values().collect::<Vec<_>>().into_iter()),
        single,
        missing,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pp_config::config::{SandboxMode, SandboxSettings};
    use pp_sandbox::{ProbeResult, Sandbox};

    fn denied(entries: &[ignore::DirEntry], sandbox: &Sandbox, name: &str) -> bool {
        let policy = sandbox.policy().unwrap();
        let can_contain = policy
            .as_ref()
            .is_some_and(|p| p.can_contain_denied_read(p.cwd()));
        entries
            .iter()
            .find(|entry| entry.file_name() == name)
            .map(|entry| sandbox_denied_entry(policy.as_deref(), can_contain, entry))
            .unwrap_or_else(|| panic!("missing walk entry {name}"))
    }

    #[test]
    fn rg_sandbox_prunes_symlinks_into_deny_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_rsa"), "secret\n").unwrap();
        std::fs::write(home.join("public.txt"), "public\n").unwrap();
        std::fs::write(root.join("a.rs"), "needle\n").unwrap();
        std::os::unix::fs::symlink(home.join(".ssh"), root.join("secret-link")).unwrap();
        std::os::unix::fs::symlink(home.join(".ssh/id_rsa"), root.join("dangling")).unwrap();

        let sandbox = Sandbox::with_probe_for_test(
            SandboxSettings {
                mode: SandboxMode::WorkspaceWrite,
                ..Default::default()
            },
            &root,
            Some(&home),
            None,
            ProbeResult::Available,
        );
        let entries: Vec<ignore::DirEntry> = ignore::WalkBuilder::new(&root)
            .hidden(false)
            .follow_links(false)
            .require_git(false)
            .build()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.depth() > 0)
            .collect();

        // 普通 entry 不得被词法误杀；拒绝根祖先目录仍可继续遍历。
        assert!(!denied(&entries, &sandbox, "a.rs"));
        assert!(!denied(&entries, &sandbox, "public.txt"));
        assert!(!denied(&entries, &sandbox, "home"));
        // symlink 与悬空链接必须取规范化结果并仍被拒绝。
        let secret_link = entries
            .iter()
            .find(|entry| entry.file_name() == "secret-link")
            .unwrap();
        assert!(
            secret_link
                .file_type()
                .is_some_and(|kind| kind.is_symlink()),
            "fixture 必须是 symlink entry"
        );
        assert!(denied(&entries, &sandbox, "secret-link"));
        assert!(denied(&entries, &sandbox, "dangling"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn rg_allows_macos_tmp_system_symlink_prefix() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let file = dir.path().join("sample.txt");
        fs::write(&file, "needle\n").unwrap();
        let raw_tmp_path = Path::new("/tmp").join(dir.path().file_name().unwrap());
        let (targets, _, missing) = parse_paths(
            dir.path(),
            raw_tmp_path.to_str().unwrap(),
            true,
            &[],
            None,
            &CancellationToken::new(),
            Instant::now() + std::time::Duration::from_secs(5),
            None,
        )
        .unwrap();
        assert!(missing.is_empty());
        assert_eq!(targets.count(), 1);
    }
}
