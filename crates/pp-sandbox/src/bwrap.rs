use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};

use crate::{EffectiveMode, ProbeResult};

#[derive(Clone, Copy)]
pub(crate) struct BwrapSpec<'a> {
    pub mode: EffectiveMode,
    pub cwd: &'a PathBuf,
    pub writable: &'a [PathBuf],
    pub allow_write_files: &'a [PathBuf],
    pub protect: &'a [PathBuf],
    pub immutable_protect: &'a [PathBuf],
    pub home_deny_root: Option<&'a PathBuf>,
    pub home_allow_read: &'a [PathBuf],
    pub deny_read: &'a [PathBuf],
    pub allow_read: &'a [PathBuf],
    pub docker_sockets: &'a [PathBuf],
    pub network: bool,
}

fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    meta.is_file() && (meta.permissions().mode() & 0o111 != 0)
}

pub(crate) fn locate_bwrap() -> Result<PathBuf> {
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let candidate = dir.join("bwrap");
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }
    for fallback in ["/usr/bin/bwrap", "/usr/local/bin/bwrap"] {
        let candidate = Path::new(fallback);
        if is_executable_file(candidate) {
            return Ok(candidate.to_path_buf());
        }
    }
    anyhow::bail!(
        "bubblewrap (bwrap) was not found in PATH or /usr/bin/bwrap; install bubblewrap (e.g. apt install bubblewrap, dnf install bubblewrap, or pacman -S bubblewrap)"
    )
}

pub(crate) fn probe_platform(network: bool) -> ProbeResult {
    use std::process::{Command, Stdio};

    let bin = match locate_bwrap() {
        Ok(path) => path,
        Err(err) => {
            return ProbeResult::Unavailable {
                reason: err.to_string(),
            };
        }
    };

    let (true_bin, true_args): (&str, &[&str]) = if is_executable_file(Path::new("/usr/bin/true")) {
        ("/usr/bin/true", &[])
    } else if is_executable_file(Path::new("/bin/true")) {
        ("/bin/true", &[])
    } else {
        ("/bin/sh", &["-c", "true"])
    };

    let mut cmd = Command::new(&bin);
    cmd.args(["--die-with-parent", "--cap-drop", "ALL"]);
    if !network {
        cmd.arg("--unshare-net");
    }
    cmd.args([
        "--ro-bind",
        "/",
        "/",
        "--dev",
        "/dev",
        "--perms",
        "0000",
        "--tmpfs",
        "/tmp",
        "--remount-ro",
        "/tmp",
        "--",
        true_bin,
    ])
    .args(true_args)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped());

    let output = match cmd.output() {
        Ok(out) => out,
        Err(err) => {
            return ProbeResult::Unavailable {
                reason: format!("failed to execute {}: {err}", bin.display()),
            };
        }
    };

    if output.status.success() {
        return ProbeResult::Available;
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let lower = stderr.to_ascii_lowercase();
    let reason = if lower.contains("creating new namespace failed")
        || lower.contains("operation not permitted")
        || lower.contains("permission denied")
        || lower.contains("no permissions to create new namespace")
        || lower.contains("uid map")
    {
        format!(
            "unprivileged user namespaces are unavailable for bwrap ({stderr}); check kernel.unprivileged_userns_clone, user.max_user_namespaces, or AppArmor restrict_unprivileged_userns"
        )
    } else if lower.contains("unknown option") || lower.contains("unrecognized option") {
        format!(
            "bwrap binary at {} is too old or unsupported ({stderr}); bubblewrap >= 0.5 is required",
            bin.display()
        )
    } else if stderr.is_empty() {
        format!("{} exited with {}", bin.display(), output.status)
    } else {
        stderr
    };
    ProbeResult::Unavailable { reason }
}

pub(crate) fn build_wrapper_prefix(spec: &BwrapSpec<'_>) -> Result<(String, Vec<OsString>)> {
    let bin = locate_bwrap().context("locating bubblewrap binary")?;
    let (args, _notes) = compile_args(spec)?;
    Ok((bin.to_string_lossy().into_owned(), args))
}

pub(crate) fn policy_notes(spec: &BwrapSpec<'_>) -> Vec<String> {
    compile_args(spec)
        .map(|(_, notes)| notes)
        .unwrap_or_default()
}

pub(crate) fn compile_args(spec: &BwrapSpec<'_>) -> Result<(Vec<OsString>, Vec<String>)> {
    let mut args = Vec::<OsString>::new();
    let mut notes = Vec::<String>::new();

    ensure_valid_path(spec.cwd)?;
    for path in spec
        .writable
        .iter()
        .chain(spec.allow_write_files)
        .chain(spec.protect)
        .chain(spec.immutable_protect)
        .chain(spec.home_deny_root)
        .chain(spec.home_allow_read)
        .chain(spec.deny_read)
        .chain(spec.allow_read)
        .chain(spec.docker_sockets)
    {
        ensure_valid_path(path)?;
    }

    args.push(OsString::from("--die-with-parent"));
    args.push(OsString::from("--cap-drop"));
    args.push(OsString::from("ALL"));
    if !spec.network {
        args.push(OsString::from("--unshare-net"));
    }

    args.push(OsString::from("--ro-bind"));
    args.push(OsString::from("/"));
    args.push(OsString::from("/"));
    args.push(OsString::from("--dev"));
    args.push(OsString::from("/dev"));

    let mut home_tmpfs: Option<PathBuf> = None;
    if let Some(home) = spec.home_deny_root
        && !spec.home_allow_read.iter().any(|allowed| allowed == home)
        && !home.starts_with(spec.cwd)
        && !spec.deny_read.iter().any(|denied| home.starts_with(denied))
        && let Ok(meta) = std::fs::symlink_metadata(home)
        && !meta.file_type().is_symlink()
        && meta.is_dir()
    {
        args.push(OsString::from("--perms"));
        args.push(OsString::from("0111"));
        args.push(OsString::from("--tmpfs"));
        args.push(home.as_os_str().to_os_string());
        home_tmpfs = Some(home.clone());
        let home_allows: Vec<PathBuf> = sort_by_depth_unique(spec.home_allow_read)
            .into_iter()
            .filter(|allowed| allowed.starts_with(home) && allowed != home)
            .collect();
        for path in home_allows {
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if !meta.file_type().is_symlink() => {
                    push_ro_bind_try(&mut args, &path);
                }
                _ => {}
            }
        }
    }

    let writable_sorted = sort_by_depth_unique(spec.writable);
    match spec.mode {
        EffectiveMode::WorkspaceWrite => {
            for path in &writable_sorted {
                push_bind_try(&mut args, path);
            }
        }
        EffectiveMode::ReadOnly => {
            for path in &writable_sorted {
                if !path.starts_with(spec.cwd) {
                    push_bind_try(&mut args, path);
                }
            }
            if spec.cwd != Path::new("/")
                && std::fs::metadata(spec.cwd).is_ok()
                && writable_sorted.iter().any(|w| spec.cwd.starts_with(w))
            {
                push_ro_bind(&mut args, spec.cwd);
            }
            for path in &writable_sorted {
                if path.starts_with(spec.cwd) && path != spec.cwd {
                    push_bind_try(&mut args, path);
                }
            }
        }
        EffectiveMode::Off => {}
    }

    let protect_sorted = sort_by_depth_unique(spec.protect);
    for path in &protect_sorted {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                notes.push(format!(
                    "skipped symlink in protect (cannot bind mount symlink): {}",
                    path.display()
                ));
            }
            Ok(_) => push_ro_bind(&mut args, path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                notes.push(format!(
                    "skipped inaccessible protect path {}: {err}",
                    path.display()
                ));
            }
        }
    }

    for path in sort_by_depth_unique(spec.allow_write_files) {
        if let Ok(meta) = std::fs::symlink_metadata(&path)
            && !meta.file_type().is_symlink()
        {
            push_bind_try(&mut args, &path);
        } else if let Some(parent) = path.parent()
            && let Ok(meta) = std::fs::symlink_metadata(parent)
            && !meta.file_type().is_symlink()
            && meta.is_dir()
        {
            push_bind_try(&mut args, parent);
        }
    }

    for path in sort_by_depth_unique(spec.immutable_protect) {
        if let Ok(meta) = std::fs::symlink_metadata(&path)
            && !meta.file_type().is_symlink()
        {
            push_ro_bind(&mut args, &path);
        }
    }

    let denies_sorted = sort_by_depth_unique(spec.deny_read);
    let allows_sorted = sort_by_depth_unique(spec.allow_read);

    let mut active_denies: Vec<(PathBuf, bool)> = Vec::new();
    for d in &denies_sorted {
        if allows_sorted.iter().any(|a| d.starts_with(a)) {
            continue;
        }
        if spec.cwd.starts_with(d) {
            notes.push(format!(
                "skipped denyRead ancestor of working directory: {}",
                d.display()
            ));
            continue;
        }
        let covered_by_outer = active_denies.iter().any(|(outer, _)| {
            d.starts_with(outer)
                && !allows_sorted
                    .iter()
                    .any(|a| a.starts_with(outer) && d.starts_with(a))
        });
        if covered_by_outer {
            continue;
        }
        let Ok(sym_meta) = std::fs::symlink_metadata(d) else {
            continue;
        };
        if sym_meta.file_type().is_symlink() {
            continue;
        }
        active_denies.push((d.clone(), sym_meta.is_dir()));
    }

    let active_allows: Vec<PathBuf> = allows_sorted
        .into_iter()
        .filter(|a| {
            active_denies
                .iter()
                .any(|(d, _)| a.starts_with(d) && a != d)
                && std::fs::metadata(a).is_ok()
        })
        .collect();

    enum MaskEvent {
        Deny { path: PathBuf, is_dir: bool },
        Allow { path: PathBuf },
    }

    impl MaskEvent {
        fn path(&self) -> &Path {
            match self {
                Self::Deny { path, .. } | Self::Allow { path } => path,
            }
        }
    }

    let mut events: Vec<MaskEvent> = active_denies
        .iter()
        .map(|(path, is_dir)| MaskEvent::Deny {
            path: path.clone(),
            is_dir: *is_dir,
        })
        .chain(
            active_allows
                .iter()
                .map(|path| MaskEvent::Allow { path: path.clone() }),
        )
        .collect();
    events.sort_by(|a, b| {
        a.path()
            .components()
            .count()
            .cmp(&b.path().components().count())
            .then_with(|| a.path().cmp(b.path()))
    });

    let mut tmpfs_dirs = Vec::<PathBuf>::new();
    for event in events {
        match event {
            MaskEvent::Deny { path, is_dir: true } => {
                let has_child_allow = active_allows
                    .iter()
                    .any(|a| a.starts_with(&path) && a != &path);
                let perms = if has_child_allow { "0111" } else { "0000" };
                args.push(OsString::from("--perms"));
                args.push(OsString::from(perms));
                args.push(OsString::from("--tmpfs"));
                args.push(path.as_os_str().to_os_string());
                tmpfs_dirs.push(path);
            }
            MaskEvent::Deny {
                path,
                is_dir: false,
            } => {
                push_ro_bind_src(&mut args, Path::new("/dev/null"), &path);
            }
            MaskEvent::Allow { path } => {
                let keep_writable = spec.mode == EffectiveMode::WorkspaceWrite
                    && writable_sorted.iter().any(|w| path.starts_with(w))
                    && !protect_sorted
                        .iter()
                        .chain(spec.immutable_protect)
                        .any(|p| path.starts_with(p));
                if keep_writable {
                    push_bind_try(&mut args, &path);
                    for p in &protect_sorted {
                        if p.starts_with(&path)
                            && p != &path
                            && std::fs::symlink_metadata(p)
                                .is_ok_and(|meta| !meta.file_type().is_symlink())
                        {
                            push_ro_bind(&mut args, p);
                        }
                    }
                } else {
                    push_ro_bind_try(&mut args, &path);
                }
            }
        }
    }

    // 必须在所有子挂载（allowRead）建立完成后再 remount-ro，否则 bwrap 无法在只读 tmpfs 中创建挂载点。
    for dir in tmpfs_dirs.into_iter().rev() {
        args.push(OsString::from("--remount-ro"));
        args.push(dir.as_os_str().to_os_string());
    }
    if let Some(home) = home_tmpfs {
        args.push(OsString::from("--remount-ro"));
        args.push(home.as_os_str().to_os_string());
    }

    for sock in sort_by_depth_unique(spec.docker_sockets) {
        if let Ok(meta) = std::fs::symlink_metadata(&sock)
            && !meta.is_dir()
            && !meta.file_type().is_symlink()
        {
            push_ro_bind_src(&mut args, Path::new("/dev/null"), &sock);
        }
    }

    args.push(OsString::from("--"));
    Ok((args, notes))
}

fn push_bind_try(args: &mut Vec<OsString>, path: &Path) {
    args.push(OsString::from("--bind-try"));
    args.push(path.as_os_str().to_os_string());
    args.push(path.as_os_str().to_os_string());
}

fn push_ro_bind(args: &mut Vec<OsString>, path: &Path) {
    push_ro_bind_src(args, path, path);
}

fn push_ro_bind_try(args: &mut Vec<OsString>, path: &Path) {
    args.push(OsString::from("--ro-bind-try"));
    args.push(path.as_os_str().to_os_string());
    args.push(path.as_os_str().to_os_string());
}

fn push_ro_bind_src(args: &mut Vec<OsString>, src: &Path, dest: &Path) {
    args.push(OsString::from("--ro-bind"));
    args.push(src.as_os_str().to_os_string());
    args.push(dest.as_os_str().to_os_string());
}

fn sort_by_depth_unique(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = paths.to_vec();
    out.sort_by(|a, b| {
        a.components()
            .count()
            .cmp(&b.components().count())
            .then_with(|| a.cmp(b))
    });
    out.dedup();
    out
}

fn ensure_valid_path(path: &Path) -> Result<()> {
    let s = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("sandbox path is not valid UTF-8: {}", path.display()))?;
    ensure!(!s.contains('\0'), "sandbox path must not contain NUL");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_to_strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn compile_args_ordering_special_chars_and_deny_ancestor_notes() {
        {
            let dir = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let cwd = root.join("ws \"odd\"\nname");
            let pp = cwd.join(".pp");
            let cache = cwd.join(".pp-cache");
            let missing_prot = cwd.join(".git/hooks");
            let deny_dir = root.join("home/.ssh");
            let allow_file = deny_dir.join("known_hosts");
            let deny_plain_dir = root.join("home/.aws");
            let deny_file = root.join("home/.netrc");
            let docker_sock = root.join("docker.sock");
            std::fs::create_dir_all(&pp).unwrap();
            std::fs::create_dir_all(&cache).unwrap();
            std::fs::create_dir_all(&deny_dir).unwrap();
            std::fs::create_dir_all(&deny_plain_dir).unwrap();
            std::fs::write(&allow_file, b"ok").unwrap();
            std::fs::write(&deny_file, b"secret").unwrap();
            std::fs::write(&docker_sock, b"").unwrap();

            let writable = vec![cache.clone(), cwd.clone(), root.clone()];
            let protect = vec![pp.clone(), missing_prot];
            let deny_read = vec![deny_dir.clone(), deny_plain_dir.clone(), deny_file.clone()];
            let allow_read = vec![allow_file.clone(), root.join("home/.ssh/missing")];
            let sockets = vec![docker_sock.clone(), root.join("missing.sock")];

            let spec = BwrapSpec {
                mode: EffectiveMode::WorkspaceWrite,
                cwd: &cwd,
                writable: &writable,
                allow_write_files: &[],
                protect: &protect,
                immutable_protect: &[],
                home_deny_root: None,
                home_allow_read: &[],
                deny_read: &deny_read,
                allow_read: &allow_read,
                docker_sockets: &sockets,
                network: false,
            };
            let (args, notes) = compile_args(&spec).unwrap();
            assert!(notes.is_empty(), "{notes:?}");
            let strs = args_to_strings(&args);

            assert_eq!(strs.last().map(String::as_str), Some("--"));
            assert!(strs.contains(&"--unshare-net".to_owned()));
            assert!(strs.contains(&"--die-with-parent".to_owned()));

            // 可写根浅到深且不丢失含引号/换行的完整路径。
            let root_pos = strs
                .iter()
                .position(|s| s == &root.to_string_lossy())
                .unwrap();
            let cwd_pos = strs
                .iter()
                .position(|s| s == &cwd.to_string_lossy())
                .unwrap();
            let prot_pos = strs
                .iter()
                .position(|s| s == &pp.to_string_lossy())
                .unwrap();
            assert!(root_pos < cwd_pos, "{strs:?}");
            assert!(cwd_pos < prot_pos, "{strs:?}");
            assert!(!strs.iter().any(|s| s.ends_with(".git/hooks")));

            // 含 allow_read 的 .ssh 用 0111，不含例外的 .aws 用 0000。
            let ssh_pos = strs
                .iter()
                .position(|s| s == &deny_dir.to_string_lossy())
                .unwrap();
            assert_eq!(strs[ssh_pos - 2], "0111", "{strs:?}");
            let aws_pos = strs
                .iter()
                .position(|s| s == &deny_plain_dir.to_string_lossy())
                .unwrap();
            assert_eq!(strs[aws_pos - 2], "0000", "{strs:?}");

            // known_hosts 绑定在 .ssh tmpfs 之后、--remount-ro .ssh 之前。
            let allow_pos = strs
                .iter()
                .position(|s| s == &allow_file.to_string_lossy())
                .unwrap();
            let remount_pos = strs
                .windows(2)
                .position(|w| w[0] == "--remount-ro" && w[1] == deny_dir.to_string_lossy())
                .unwrap();
            assert!(ssh_pos < allow_pos && allow_pos < remount_pos, "{strs:?}");

            // deny_file 与 docker_sock 覆盖 /dev/null。
            assert!(strs.windows(3).any(|w| w[0] == "--ro-bind"
                && w[1] == "/dev/null"
                && w[2] == deny_file.to_string_lossy()));
            assert!(strs.windows(3).any(|w| w[0] == "--ro-bind"
                && w[1] == "/dev/null"
                && w[2] == docker_sock.to_string_lossy()));

            // ReadOnly 模式下，落在可写 root 下的 cwd 被 --ro-bind 覆盖，子根 .pp-cache 恢复 --bind-try。
            let ro_writable = vec![root.clone(), cache.clone()];
            let ro_spec = BwrapSpec {
                mode: EffectiveMode::ReadOnly,
                cwd: &cwd,
                writable: &ro_writable,
                allow_write_files: &[],
                protect: &[],
                immutable_protect: &[],
                home_deny_root: None,
                home_allow_read: &[],
                deny_read: &[],
                allow_read: &[],
                docker_sockets: &[],
                network: true,
            };
            let (ro_args, _) = compile_args(&ro_spec).unwrap();
            let ro_strs = args_to_strings(&ro_args);
            assert!(!ro_strs.contains(&"--unshare-net".to_owned()));
            let ro_cwd_pos = ro_strs
                .windows(3)
                .position(|w| w[0] == "--ro-bind" && w[1] == cwd.to_string_lossy())
                .unwrap();
            let ro_cache_pos = ro_strs
                .windows(3)
                .position(|w| w[0] == "--bind-try" && w[1] == cache.to_string_lossy())
                .unwrap();
            assert!(ro_cwd_pos < ro_cache_pos, "{ro_strs:?}");
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            let cwd = root.join("sub");
            std::fs::create_dir_all(&cwd).unwrap();

            let deny = vec![root.clone()];
            let spec = BwrapSpec {
                mode: EffectiveMode::WorkspaceWrite,
                cwd: &cwd,
                writable: std::slice::from_ref(&cwd),
                allow_write_files: &[],
                protect: &[],
                immutable_protect: &[],
                home_deny_root: None,
                home_allow_read: &[],
                deny_read: &deny,
                allow_read: &[],
                docker_sockets: &[],
                network: true,
            };
            let (args, notes) = compile_args(&spec).unwrap();
            let strs = args_to_strings(&args);
            assert!(!strs.contains(&"--tmpfs".to_owned()));
            assert!(
                notes
                    .iter()
                    .any(|n| n.contains("skipped denyRead ancestor of working directory")),
                "{notes:?}"
            );
        }
    }

    #[test]
    fn bwrap_enforces_write_protect_read_deny_and_network_isolation() {
        if !matches!(probe_platform(false), ProbeResult::Available) {
            eprintln!(
                "skipping bwrap execution test: bwrap/userns unavailable in test environment"
            );
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let workspace = root.join("ws");
        let sub = workspace.join("sub");
        let outside = root.join("outside");
        let protected = workspace.join(".pp");
        let cache = workspace.join(".pp-cache");
        let secret_dir = root.join("home/.ssh");
        let secret_file = root.join("home/.netrc");
        let allow_file = secret_dir.join("known_hosts");

        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&protected).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(&secret_dir).unwrap();
        std::fs::write(secret_dir.join("id_ed25519"), b"top-secret").unwrap();
        std::fs::write(&allow_file, b"known-ok").unwrap();
        std::fs::write(&secret_file, b"netrc-secret").unwrap();

        // 故意不把 /tmp 加入 writable，以验证同在 tempdir 下的 outside 目录确实只读。
        let writable = vec![workspace.clone(), cache.clone()];
        let protect = vec![protected.clone()];
        let deny_read = vec![secret_dir.clone(), secret_file.clone()];
        let allow_read = vec![allow_file.clone()];

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let host_port = listener.local_addr().unwrap().port();

        let spec = BwrapSpec {
            mode: EffectiveMode::WorkspaceWrite,
            cwd: &workspace,
            writable: &writable,
            allow_write_files: &[],
            protect: &protect,
            immutable_protect: &[],
            home_deny_root: None,
            home_allow_read: &[],
            deny_read: &deny_read,
            allow_read: &allow_read,
            docker_sockets: &[],
            network: false,
        };
        let (bin, mut args) = build_wrapper_prefix(&spec).unwrap();
        let script = format!(
            "echo ok > {ws}/ok.txt && \
             mv {ws}/ok.txt {ws}/sub/moved.txt && \
             (cd sub && test \"$(pwd)\" = {sub_dir}) && \
             (! echo bad > {out}/blocked.txt 2>/dev/null) && \
             (! echo bad > /etc/pp-sbx-blocked 2>/dev/null) && \
             (! echo bad > {prot}/hooks.yml 2>/dev/null) && \
             (! ls {sec} >/dev/null 2>&1) && \
             (! test -e {sec}/id_ed25519) && \
             test \"$(cat {allow})\" = 'known-ok' && \
             test -z \"$(cat {netrc})\" && \
             /usr/bin/python3 -c \"import socket; s=socket.socket(); s.settimeout(1.0); rc=s.connect_ex(('127.0.0.1', {port})); s.close(); exit(0 if rc != 0 else 1)\" && \
             echo err >&2",
            ws = workspace.display(),
            sub_dir = sub.display(),
            out = outside.display(),
            prot = protected.display(),
            sec = secret_dir.display(),
            allow = allow_file.display(),
            netrc = secret_file.display(),
            port = host_port,
        );
        args.push(OsString::from("/bin/sh"));
        args.push(OsString::from("-c"));
        args.push(OsString::from(script));

        let status = std::process::Command::new(&bin)
            .args(&args)
            .current_dir(&workspace)
            .status()
            .unwrap();
        assert!(status.success(), "bwrap workspace-write failed: {status}");
        assert_eq!(
            std::fs::read_to_string(sub.join("moved.txt")).unwrap(),
            "ok\n"
        );
        assert!(!outside.join("blocked.txt").exists());
        assert!(!protected.join("hooks.yml").exists());

        // ReadOnly 模式：workspace 不可写（即使 workspace 位于 /tmp 树下）；.pp-cache 可写；network=true 可连通宿主端口。
        let ro_spec = BwrapSpec {
            mode: EffectiveMode::ReadOnly,
            cwd: &workspace,
            writable: &[cache.clone(), PathBuf::from("/tmp")],
            allow_write_files: &[],
            protect: &protect,
            immutable_protect: &[],
            home_deny_root: None,
            home_allow_read: &[],
            deny_read: &[],
            allow_read: &[],
            docker_sockets: &[],
            network: true,
        };
        let (ro_bin, mut ro_args) = build_wrapper_prefix(&ro_spec).unwrap();
        let ro_script = format!(
            "(! echo bad > {ws}/ro_blocked.txt 2>/dev/null) && \
             echo cached > {cache}/ok.txt && \
             /usr/bin/python3 -c \"import socket; s=socket.socket(); s.settimeout(1.0); rc=s.connect_ex(('127.0.0.1', {port})); s.close(); exit(0 if rc == 0 else 1)\"",
            ws = workspace.display(),
            cache = cache.display(),
            port = host_port,
        );
        ro_args.push(OsString::from("/bin/sh"));
        ro_args.push(OsString::from("-c"));
        ro_args.push(OsString::from(ro_script));
        let ro_status = std::process::Command::new(&ro_bin)
            .args(&ro_args)
            .current_dir(&workspace)
            .status()
            .unwrap();
        assert!(ro_status.success(), "bwrap read-only failed: {ro_status}");
        assert!(!workspace.join("ro_blocked.txt").exists());
        assert_eq!(
            std::fs::read_to_string(cache.join("ok.txt")).unwrap(),
            "cached\n"
        );
    }
}
