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
pub(super) fn parse_paths(
    cwd: &Path,
    input: &str,
    respect: bool,
    globs: &[String],
    types: Option<&ignore::types::Types>,
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
