use std::{
    io,
    path::{Component, Path, PathBuf},
};

const MAX_SYMLINK_DEPTH: usize = 16;

// 测试用 `canonicalize` 计数：nextest 每个用例独立进程，线程局部计数让
// “常规 entry 不触发规范化”的断言不受同进程并行用例干扰。
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

/// 规范化路径：尽可能通过 `canonicalize` 折叠符号链接（如 `/tmp` → `/private/tmp`），
/// 对尚不存在的后半段做词法折叠；遇到悬空符号链接时追踪其目标路径以防绕过。
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    normalize_with_depth(path, 0)
}

/// 词法快路径：`target` 已是绝对路径且各分量均可直接比较（无 `..`/前缀；`.` 由
/// `Path::components` 自行归一）时，只要它与所有拒绝根在路径分量层面互不为祖先也
/// 互不为后代，规范化就不可能把它移入拒绝集合，可跳过 `canonicalize`。
///
/// 前提是调用方保证路径已存在的祖先已解析（`rg`/`glob` 的 walk 根已
/// `canonicalize` 且 `follow_links(false)`）；symlink entry 必须改用
/// `Policy::is_read_denied_resolved`。其余情况返回 `false`，调用方必须走
/// `normalize_path` 的完整判定。
pub(crate) fn lexically_outside_deny(target: &Path, deny_read: &[PathBuf]) -> bool {
    let mut components = target.components();
    if !matches!(components.next(), Some(Component::RootDir)) {
        return false;
    }
    if components.any(|component| !matches!(component, Component::Normal(_))) {
        return false;
    }
    !deny_read
        .iter()
        .any(|denied| target.starts_with(denied) || denied.starts_with(target))
}

/// 拒绝根在建策略时已经规范化；macOS 上 `/tmp`、`/var`、`/etc` 是指向
/// `/private/...` 的符号链接，词法快路径需要把未规范化的系统别名同样视为
/// “可能命中”，否则 `/tmp/x` 会绕过规范化为 `/private/tmp/x` 的拒绝根。
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

/// 匹配 macOS `confstr(_CS_DARWIN_USER_TEMP_DIR / CACHE_DIR)` 路径：
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
) -> io::Result<()> {
    if is_read_denied_path(target, deny_read, allow_read) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "blocked by sandbox policy (credential or protected path read denied): {}",
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
) -> bool {
    if allow_read.iter().any(|allowed| target.starts_with(allowed)) {
        return false;
    }
    deny_read.iter().any(|denied| target.starts_with(denied))
}

pub(crate) fn check_write_path(
    target: &Path,
    mode: crate::EffectiveMode,
    cwd: &Path,
    writable: &[PathBuf],
    protect: &[PathBuf],
) -> io::Result<()> {
    if is_safe_device_write(target) {
        return Ok(());
    }
    if protect.iter().any(|prot| target.starts_with(prot)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "blocked by sandbox policy (protected path is read-only): {}",
                target.display()
            ),
        ));
    }
    // 在 ReadOnly 模式下，即使 cwd 自身位于 /tmp 或 /var/folders/.../T 下，
    // 也只允许写入显式列出的子根（如 .pp-cache、本次 scratch），其余 cwd 子路径只读。
    if mode == crate::EffectiveMode::ReadOnly && target.starts_with(cwd) {
        if writable
            .iter()
            .any(|root| root.starts_with(cwd) && root != cwd && target.starts_with(root))
        {
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
    if is_macos_var_folder_temp(target) || writable.iter().any(|root| target.starts_with(root)) {
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
    fn read_guard_respects_allow_override_and_prefix_boundaries() {
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
            let res = check_read_path(Path::new(path), &deny, &allow);
            assert_eq!(res.is_err(), expect_denied, "path={path}");
        }
    }

    #[test]
    fn write_guard_respects_protect_priority_and_safe_devices() {
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
                &protect,
            );
            assert_eq!(res.is_ok(), expect_ok, "path={path} -> {res:?}");
        }
    }

    #[test]
    fn normalize_path_handles_missing_parent_traversal_and_dangling_symlink() {
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

    #[test]
    fn sandbox_lexical_fast_path_only_skips_component_disjoint_paths() {
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
            // `.` 由 `Path::components` 归一，不改变安全性
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

    #[test]
    fn sandbox_lexical_aliases_cover_private_prefix_folding() {
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
}
