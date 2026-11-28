use std::{
    io,
    path::{Component, Path, PathBuf},
};

const MAX_SYMLINK_DEPTH: usize = 16;

// The test-only `canonicalize` counter is thread-local so parallel cases in the same process cannot disturb the assertions.
#[cfg(test)]
thread_local! {
    static CANONICALIZE_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    #[cfg(test)]
    CANONICALIZE_CALLS.with(|calls| calls.set(calls.get() + 1));
    std::fs::canonicalize(path)
}

#[cfg(test)]
pub(crate) fn canonicalize_calls() -> usize {
    CANONICALIZE_CALLS.with(|calls| calls.get())
}

#[cfg(test)]
pub(crate) fn reset_canonicalize_calls() {
    CANONICALIZE_CALLS.with(|calls| calls.set(0));
}

/// Normalizes a path: collapses symlinks through `canonicalize` where possible (e.g. `/tmp` → `/private/tmp`),
/// folds the not-yet-existing tail lexically, and follows a dangling symlink's target to prevent bypass.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    normalize_with_depth(path, 0)
}

/// Compares path prefixes under platform case semantics: on macOS's default case-insensitive APFS each
/// normal component is case-folded so case variants cannot bypass `protect`, the lexical fast path or `deny_read`.
pub(crate) fn path_starts_with(path: &Path, base: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        let mut p_comps = path.components();
        let mut b_comps = base.components();
        loop {
            match b_comps.next() {
                None => return true,
                Some(b) => {
                    let Some(p) = p_comps.next() else {
                        return false;
                    };
                    if !component_eq_macos(p, b) {
                        return false;
                    }
                }
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        path.starts_with(base)
    }
}

#[cfg(target_os = "macos")]
fn component_eq_macos(left: Component<'_>, right: Component<'_>) -> bool {
    match (left, right) {
        (Component::Normal(a), Component::Normal(b)) => {
            if a == b {
                return true;
            }
            let (Some(sa), Some(sb)) = (a.to_str(), b.to_str()) else {
                return false;
            };
            if sa.is_ascii() && sb.is_ascii() {
                sa.eq_ignore_ascii_case(sb)
            } else {
                sa.to_lowercase() == sb.to_lowercase()
            }
        }
        _ => left == right,
    }
}

pub(crate) fn paths_overlap(left: &Path, right: &Path) -> bool {
    path_starts_with(left, right) || path_starts_with(right, left)
}

pub(crate) fn path_eq_platform(left: &Path, right: &Path) -> bool {
    path_starts_with(left, right) && path_starts_with(right, left)
}

pub(crate) fn lexically_normal_abs(target: &Path) -> bool {
    let mut components = target.components();
    if !matches!(components.next(), Some(Component::RootDir)) {
        return false;
    }
    !components.any(|component| !matches!(component, Component::Normal(_)))
}

#[cfg(test)]
pub(crate) fn lexically_outside_deny(target: &Path, deny_read: &[PathBuf]) -> bool {
    if !lexically_normal_abs(target) {
        return false;
    }
    !deny_read.iter().any(|denied| paths_overlap(target, denied))
}

/// Deny roots are already normalized when the policy is built; on macOS `/tmp`, `/var` and `/etc` are
/// symlinks into `/private/...`, so the lexical fast path must treat the unnormalized system alias as
/// "possibly matching" too, otherwise `/tmp/x` would bypass the deny root normalized to `/private/tmp/x`.
pub(crate) fn lexical_aliases(root: &Path) -> Vec<PathBuf> {
    let Ok(rest) = root.strip_prefix("/private") else {
        return Vec::new();
    };
    let Some(Component::Normal(first)) = rest.components().next() else {
        return Vec::new();
    };
    if !matches!(first.to_str(), Some("tmp" | "var" | "etc")) {
        return Vec::new();
    }
    vec![Path::new("/").join(rest)]
}

fn normalize_with_depth(path: &Path, depth: usize) -> PathBuf {
    if let Ok(canon) = canonicalize(path) {
        return canon;
    }

    let mut acc = PathBuf::new();
    let mut broken = false;
    let components: Vec<_> = path.components().collect();

    for (idx, comp) in components.iter().enumerate() {
        match comp {
            Component::Prefix(prefix) => acc.push(prefix.as_os_str()),
            Component::RootDir => acc.push("/"),
            Component::CurDir => {}
            Component::ParentDir => {
                if !broken {
                    acc.push("..");
                    if let Ok(canon) = canonicalize(&acc) {
                        acc = canon;
                    } else {
                        acc.pop();
                        acc.pop();
                        broken = true;
                    }
                } else {
                    acc.pop();
                    if let Ok(canon) = canonicalize(&acc) {
                        acc = canon;
                        broken = false;
                    }
                }
            }
            Component::Normal(seg) => {
                acc.push(seg);
                if !broken {
                    if let Ok(canon) = canonicalize(&acc) {
                        acc = canon;
                    } else if depth < MAX_SYMLINK_DEPTH
                        && std::fs::symlink_metadata(&acc)
                            .is_ok_and(|meta| meta.file_type().is_symlink())
                        && let Ok(target) = std::fs::read_link(&acc)
                    {
                        let base = acc.parent().unwrap_or_else(|| Path::new("/"));
                        let mut resolved = if target.is_absolute() {
                            target
                        } else {
                            base.join(target)
                        };
                        for rem in &components[idx + 1..] {
                            resolved.push(rem.as_os_str());
                        }
                        return normalize_with_depth(&resolved, depth + 1);
                    } else {
                        broken = true;
                    }
                }
            }
        }
    }

    acc
}

pub(crate) fn expand_tilde(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    if raw == "~" {
        return home.map(Path::to_path_buf);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home.map(|h| h.join(rest));
    }
    Some(PathBuf::from(raw))
}

pub(crate) fn is_safe_device_write(path: &Path) -> bool {
    let raw = path.to_string_lossy();
    matches!(
        raw.as_ref(),
        "/dev/null" | "/dev/tty" | "/dev/zero" | "/dev/dtracehelper"
    ) || raw.starts_with("/dev/fd/")
        || raw.starts_with("/dev/tty")
        || raw.starts_with("/dev/pty")
}

/// Matches macOS `confstr(_CS_DARWIN_USER_TEMP_DIR / CACHE_DIR)` paths:
/// `/private/var/folders/<2>/<hash>/{T,C}(/.*)?`
pub(crate) fn is_macos_var_folder_temp(path: &Path) -> bool {
    fn next_normal<'a>(it: &mut std::path::Components<'a>) -> Option<&'a std::ffi::OsStr> {
        match it.next() {
            Some(Component::Normal(s)) => Some(s),
            _ => None,
        }
    }
    let mut comps = path.components();
    let Some(Component::RootDir) = comps.next() else {
        return false;
    };
    next_normal(&mut comps).is_some_and(|s| s == "private")
        && next_normal(&mut comps).is_some_and(|s| s == "var")
        && next_normal(&mut comps).is_some_and(|s| s == "folders")
        && next_normal(&mut comps).is_some_and(|s| !s.is_empty())
        && next_normal(&mut comps).is_some_and(|s| !s.is_empty())
        && next_normal(&mut comps).is_some_and(|s| s == "T" || s == "C")
}

pub(crate) fn check_read_path(
    target: &Path,
    deny_read: &[PathBuf],
    allow_read: &[PathBuf],
    home_deny_root: Option<&Path>,
    home_allow_read: &[PathBuf],
) -> io::Result<()> {
    if allow_read
        .iter()
        .any(|allowed| path_starts_with(target, allowed))
    {
        return Ok(());
    }
    if deny_read
        .iter()
        .any(|denied| path_starts_with(target, denied))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "blocked by sandbox policy (credential or protected path read denied): {}",
                target.display()
            ),
        ));
    }
    if let Some(home) = home_deny_root
        && path_starts_with(target, home)
        && !home_allow_read
            .iter()
            .any(|allowed| path_starts_with(target, allowed))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "blocked by sandbox policy (read outside workspace home boundary): {}",
                target.display()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn is_read_denied_path(
    target: &Path,
    deny_read: &[PathBuf],
    allow_read: &[PathBuf],
    home_deny_root: Option<&Path>,
    home_allow_read: &[PathBuf],
) -> bool {
    if allow_read
        .iter()
        .any(|allowed| path_starts_with(target, allowed))
    {
        return false;
    }
    if deny_read
        .iter()
        .any(|denied| path_starts_with(target, denied))
    {
        return true;
    }
    if let Some(home) = home_deny_root
        && path_starts_with(target, home)
        && !home_allow_read
            .iter()
            .any(|allowed| path_starts_with(target, allowed))
    {
        return true;
    }
    false
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn check_write_path(
    target: &Path,
    mode: crate::EffectiveMode,
    cwd: &Path,
    writable: &[PathBuf],
    allow_write_files: &[PathBuf],
    protect: &[PathBuf],
    immutable_protect: &[PathBuf],
    deny_read: &[PathBuf],
    allow_read: &[PathBuf],
) -> io::Result<()> {
    if is_safe_device_write(target) {
        return Ok(());
    }
    if immutable_protect
        .iter()
        .any(|prot| path_starts_with(target, prot))
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "blocked by sandbox policy (immutable protected path is read-only): {}",
                target.display()
            ),
        ));
    }
    if is_read_denied_path(target, deny_read, allow_read, None, &[]) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "blocked by sandbox policy (credential or protected path write denied): {}",
                target.display()
            ),
        ));
    }
    if allow_write_files
        .iter()
        .any(|allowed| path_eq_platform(target, allowed))
    {
        return Ok(());
    }
    if protect.iter().any(|prot| path_starts_with(target, prot)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "blocked by sandbox policy (protected path is read-only): {}",
                target.display()
            ),
        ));
    }
    // In ReadOnly mode, even when cwd itself sits under /tmp or /var/folders/.../T,
    // only the explicitly listed sub-roots (e.g. .pp-cache, this run's scratch) are writable; the rest of cwd is read-only.
    if mode == crate::EffectiveMode::ReadOnly && path_starts_with(target, cwd) {
        if writable.iter().any(|root| {
            path_starts_with(root, cwd)
                && !path_eq_platform(root, cwd)
                && path_starts_with(target, root)
        }) {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "blocked by sandbox policy (read-only mode forbids writing in workspace): {}",
                target.display()
            ),
        ));
    }
    if is_macos_var_folder_temp(target)
        || writable.iter().any(|root| path_starts_with(target, root))
    {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "blocked by sandbox policy (write outside writable roots): {}",
            target.display()
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_write_guards_and_normalize_path() {
        {
            let deny = vec![
                PathBuf::from("/Users/u/.ssh"),
                PathBuf::from("/Users/u/.pp/agent"),
            ];
            let allow = vec![
                PathBuf::from("/Users/u/.pp/agent/skills"),
                PathBuf::from("/Users/u/.ssh/known_hosts"),
            ];

            let cases = [
                ("/Users/u/.ssh/id_ed25519", true),
                ("/Users/u/.ssh_backup/key", false),
                ("/Users/u/.ssh/known_hosts", false),
                ("/Users/u/.pp/agent/models.yml", true),
                ("/Users/u/.pp/agent/skills/demo/SKILL.md", false),
                ("/Users/u/project/src/main.rs", false),
            ];

            for (path, expect_denied) in cases {
                let res = check_read_path(Path::new(path), &deny, &allow, None, &[]);
                assert_eq!(res.is_err(), expect_denied, "path={path}");
            }
        }
        {
            let writable = vec![PathBuf::from("/work/repo"), PathBuf::from("/private/tmp")];
            let protect = vec![
                PathBuf::from("/work/repo/.git/hooks"),
                PathBuf::from("/work/repo/.pp"),
            ];

            let cases = [
                ("/work/repo/src/lib.rs", true),
                ("/work/repo/.pp-cache/x", true),
                ("/work/repo/.pp/hooks.yml", false),
                ("/work/repo/.git/hooks/pre-commit", false),
                ("/private/tmp/out.log", true),
                ("/private/var/folders/ab/cdef/T/dsym", true),
                ("/dev/null", true),
                ("/Users/u/.zshrc", false),
            ];

            for (path, expect_ok) in cases {
                let res = check_write_path(
                    Path::new(path),
                    crate::EffectiveMode::WorkspaceWrite,
                    Path::new("/work/repo"),
                    &writable,
                    &[],
                    &protect,
                    &[],
                    &[],
                    &[],
                );
                assert_eq!(res.is_ok(), expect_ok, "path={path} -> {res:?}");
            }
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let outside = root.join("outside_secret");

            let link = root.join("dangling_link");
            #[cfg(unix)]
            std::os::unix::fs::symlink(&outside, &link).unwrap();

            assert_eq!(normalize_path(&link), outside);
            assert_eq!(
                normalize_path(&root.join("missing/../sub/file.txt")),
                root.join("sub/file.txt")
            );
        }
    }

    #[test]
    fn lexical_fast_path_aliases_and_denied_write_guard() {
        {
            let deny = vec![PathBuf::from("/work/repo/.ssh")];
            let cases = [
                ("/work/repo/src/main.rs", true),
                ("/work/repo/.ssh_backup/key", true),
                ("/work/repo/.ssh", false),
                ("/work/repo/.ssh/id_rsa", false),
                ("/work/repo", false),
                ("/work", false),
                ("work/repo/src", false),
                ("/work/repo/../repo/src", false),
                // `.` is normalized away by `Path::components`, so safety is unaffected
                ("/work/./repo/src", true),
            ];
            for (path, outside) in cases {
                assert_eq!(
                    lexically_outside_deny(Path::new(path), &deny),
                    outside,
                    "path={path}"
                );
            }
            assert!(lexically_outside_deny(Path::new("/work/repo/src"), &[]));
        }
        {
            assert_eq!(
                lexical_aliases(Path::new("/private/tmp/secret")),
                vec![PathBuf::from("/tmp/secret")]
            );
            assert_eq!(
                lexical_aliases(Path::new("/private/var/folders/x/T")),
                vec![PathBuf::from("/var/folders/x/T")]
            );
            assert!(lexical_aliases(Path::new("/Users/u/.ssh")).is_empty());
            assert!(lexical_aliases(Path::new("/private/tmpx/secret")).is_empty());
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let secret = root.join("secret");
            let allowed = secret.join("public");
            assert!(
                check_write_path(
                    &secret.join("key"),
                    crate::EffectiveMode::WorkspaceWrite,
                    &root,
                    std::slice::from_ref(&root),
                    &[],
                    &[],
                    &[],
                    std::slice::from_ref(&secret),
                    std::slice::from_ref(&allowed),
                )
                .is_err()
            );
            assert!(
                check_write_path(
                    &allowed.join("note"),
                    crate::EffectiveMode::WorkspaceWrite,
                    &root,
                    std::slice::from_ref(&root),
                    &[],
                    &[],
                    &[],
                    std::slice::from_ref(&secret),
                    std::slice::from_ref(&allowed),
                )
                .is_ok()
            );
        }
    }
}
