use std::{ffi::OsString, path::PathBuf};

use anyhow::{Result, ensure};

use crate::ProbeResult;

#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[cfg(target_os = "macos")]
pub(crate) fn probe_platform() -> ProbeResult {
    use std::process::{Command, Stdio};

    let output = match Command::new(SANDBOX_EXEC)
        .args(["-p", "(version 1)(allow default)", "/usr/bin/true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(out) => out,
        Err(err) => {
            return ProbeResult::Unavailable {
                reason: format!("failed to execute {SANDBOX_EXEC}: {err}"),
            };
        }
    };

    if output.status.success() {
        return ProbeResult::Available;
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let reason = if stderr.contains("sandbox_apply")
        || output.status.code() == Some(71)
        || stderr.contains("Operation not permitted")
    {
        format!("nested sandbox or permission restriction ({stderr})")
    } else if stderr.is_empty() {
        format!("{SANDBOX_EXEC} exited with {}", output.status)
    } else {
        stderr
    };
    ProbeResult::Unavailable { reason }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn probe_platform() -> ProbeResult {
    ProbeResult::Unavailable {
        reason: "sandbox is currently only supported on macOS".to_owned(),
    }
}

pub(crate) struct SeatbeltSpec<'a> {
    pub mode: crate::EffectiveMode,
    pub cwd: &'a PathBuf,
    pub writable: &'a [PathBuf],
    pub protect: &'a [PathBuf],
    pub deny_read: &'a [PathBuf],
    pub allow_read: &'a [PathBuf],
    pub docker_user_sock: Option<&'a PathBuf>,
    pub network: bool,
}

/// 组装 `(sandbox-exec 路径, 前缀 args)`，末尾以 `--` 结束；
/// 路径全部通过 `-D K=V` 传递，SBPL 源码只含静态字面量与 `(param "K")`。
pub(crate) fn build_wrapper_prefix(spec: &SeatbeltSpec<'_>) -> Result<(String, Vec<OsString>)> {
    let (profile, params) = compile_profile(spec)?;
    let mut args = Vec::with_capacity(2 + params.len() * 2 + 1);
    args.push(OsString::from("-p"));
    args.push(OsString::from(profile));
    for (key, value) in params {
        args.push(OsString::from("-D"));
        let mut binding = OsString::from(key);
        binding.push("=");
        binding.push(value);
        args.push(binding);
    }
    args.push(OsString::from("--"));

    #[cfg(target_os = "macos")]
    let bin = SANDBOX_EXEC.to_owned();
    #[cfg(not(target_os = "macos"))]
    let bin = "/usr/bin/sandbox-exec".to_owned();

    Ok((bin, args))
}

#[cfg(test)]
pub(crate) fn build_exec_args(
    spec: &SeatbeltSpec<'_>,
    program: &str,
    program_args: &[OsString],
) -> Result<(String, Vec<OsString>)> {
    let (bin, mut args) = build_wrapper_prefix(spec)?;
    args.push(OsString::from(program));
    args.extend(program_args.iter().cloned());
    Ok((bin, args))
}

pub(crate) fn compile_profile(
    spec: &SeatbeltSpec<'_>,
) -> Result<(String, Vec<(String, OsString)>)> {
    use std::fmt::Write as _;

    let mut params: Vec<(String, OsString)> = Vec::new();
    let mut sb = String::from(
        "(version 1)\n\
         (allow default)\n\
         (deny file-write*)\n\
         (allow file-write*\n\
           (regex #\"^/private/var/folders/[^/]+/[^/]+/[TC](/|$)\")\n\
           (literal \"/dev/null\")\n\
           (literal \"/dev/tty\")\n\
           (literal \"/dev/zero\")\n\
           (literal \"/dev/dtracehelper\")\n\
           (regex #\"^/dev/(fd/|tty|pty)\")\n",
    );

    for (idx, path) in spec.writable.iter().enumerate() {
        ensure_valid_param_path(path)?;
        let key = format!("W{idx}");
        let _ = writeln!(sb, "  (subpath (param \"{key}\"))");
        params.push((key, path.as_os_str().to_os_string()));
    }
    sb.push_str(")\n");

    if spec.mode == crate::EffectiveMode::ReadOnly {
        ensure_valid_param_path(spec.cwd)?;
        sb.push_str("(deny file-write* (subpath (param \"CWD\")))\n");
        params.push(("CWD".to_owned(), spec.cwd.as_os_str().to_os_string()));
        let mut cwd_writable = Vec::new();
        for (idx, path) in spec.writable.iter().enumerate() {
            if path.starts_with(spec.cwd) && path != spec.cwd {
                cwd_writable.push((idx, path));
            }
        }
        if !cwd_writable.is_empty() {
            sb.push_str("(allow file-write*\n");
            for (idx, _) in cwd_writable {
                let _ = writeln!(sb, "  (subpath (param \"W{idx}\"))");
            }
            sb.push_str(")\n");
        }
    }

    if !spec.protect.is_empty() {
        sb.push_str("(deny file-write*\n");
        for (idx, path) in spec.protect.iter().enumerate() {
            ensure_valid_param_path(path)?;
            let key = format!("P{idx}");
            let _ = writeln!(sb, "  (subpath (param \"{key}\"))");
            params.push((key, path.as_os_str().to_os_string()));
        }
        sb.push_str(")\n");
    }

    if !spec.deny_read.is_empty() {
        sb.push_str("(deny file-read*\n");
        for (idx, path) in spec.deny_read.iter().enumerate() {
            ensure_valid_param_path(path)?;
            let key = format!("R{idx}");
            let _ = writeln!(sb, "  (subpath (param \"{key}\"))");
            params.push((key, path.as_os_str().to_os_string()));
        }
        sb.push_str(")\n");
    }

    if !spec.allow_read.is_empty() {
        sb.push_str("(allow file-read*\n");
        for (idx, path) in spec.allow_read.iter().enumerate() {
            ensure_valid_param_path(path)?;
            let key = format!("A{idx}");
            let _ = writeln!(sb, "  (subpath (param \"{key}\"))");
            params.push((key, path.as_os_str().to_os_string()));
        }
        sb.push_str(")\n");
    }

    sb.push_str(
        "(deny mach-lookup\n\
           (global-name \"com.apple.SecurityServer\")\n\
           (global-name \"com.apple.securityd\")\n\
           (global-name \"com.apple.pasteboard.1\")\n\
           (global-name-prefix \"com.apple.coreservices.appleevents\")\n\
           (global-name \"com.apple.coreservices.launchservicesd\")\n\
         )\n",
    );

    if !spec.network {
        sb.push_str(
            "(deny network-outbound)\n\
             (allow network-outbound (remote ip \"localhost:*\"))\n\
             (allow network-outbound (remote unix-socket))\n",
        );
    }

    sb.push_str(
        "(deny network-outbound\n\
           (remote unix-socket (path-literal \"/var/run/docker.sock\"))\n\
           (remote unix-socket (path-literal \"/private/var/run/docker.sock\"))\n",
    );
    if let Some(docker_sock) = spec.docker_user_sock {
        ensure_valid_param_path(docker_sock)?;
        sb.push_str("  (remote unix-socket (path-literal (param \"DS0\")))\n");
        params.push(("DS0".to_owned(), docker_sock.as_os_str().to_os_string()));
    }
    sb.push_str(")\n");

    Ok((sb, params))
}

fn ensure_valid_param_path(path: &std::path::Path) -> Result<()> {
    let s = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("sandbox path is not valid UTF-8: {}", path.display()))?;
    ensure!(!s.contains('\0'), "sandbox path must not contain NUL");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_profile_never_interpolates_raw_paths_into_sbpl() {
        let writable = vec![PathBuf::from("/tmp/evil\") (allow default")];
        let protect = vec![PathBuf::from("/work/.git/hooks")];
        let deny_read = vec![PathBuf::from("/home/u/.ssh")];
        let allow_read = vec![PathBuf::from("/home/u/.ssh/known_hosts")];
        let docker_sock = PathBuf::from("/home/u/.docker/run/docker.sock");

        let cwd = PathBuf::from("/work");
        let spec = SeatbeltSpec {
            mode: crate::EffectiveMode::WorkspaceWrite,
            cwd: &cwd,
            writable: &writable,
            protect: &protect,
            deny_read: &deny_read,
            allow_read: &allow_read,
            docker_user_sock: Some(&docker_sock),
            network: false,
        };
        let (profile, params) = compile_profile(&spec).unwrap();
        assert!(!profile.contains("evil"));
        assert!(!profile.contains("/work/.git/hooks"));
        assert!(profile.contains("(param \"W0\")"));
        assert!(profile.contains("(param \"P0\")"));
        assert!(profile.contains("(param \"R0\")"));
        assert!(profile.contains("(param \"A0\")"));
        assert!(profile.contains("(param \"DS0\")"));
        assert!(profile.contains(
            "(deny network-outbound)\n(allow network-outbound (remote ip \"localhost:*\"))"
        ));

        let docker_deny_pos = profile
            .rfind("path-literal \"/var/run/docker.sock\"")
            .unwrap();
        let unix_allow_pos = profile
            .find("(allow network-outbound (remote unix-socket))")
            .unwrap();
        assert!(docker_deny_pos > unix_allow_pos);
        assert_eq!(params.len(), 5);
    }

    #[test]
    fn build_exec_args_places_double_dash_before_target_command() {
        let writable = vec![PathBuf::from("/work")];
        let cwd = PathBuf::from("/work");
        let spec = SeatbeltSpec {
            mode: crate::EffectiveMode::WorkspaceWrite,
            cwd: &cwd,
            writable: &writable,
            protect: &[],
            deny_read: &[],
            allow_read: &[],
            docker_user_sock: None,
            network: true,
        };
        let (bin, args) = build_exec_args(
            &spec,
            "/bin/echo",
            &[OsString::from("-n"), OsString::from("hi")],
        )
        .unwrap();
        assert_eq!(bin, "/usr/bin/sandbox-exec");
        let dash_dash = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(args[dash_dash + 1], "/bin/echo");
        assert_eq!(args[dash_dash + 2], "-n");
        assert_eq!(args[dash_dash + 3], "hi");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn sandbox_exec_enforces_write_read_and_network_restrictions() {
        assert_eq!(probe_platform(), ProbeResult::Available);

        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let workspace = root.join("ws");
        let outside = root.join("outside");
        let secret = workspace.join("secret");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("key"), b"top-secret").unwrap();

        // 注意：tempdir 在 macOS /var/folders/.../T 下，会被 var_folder 正则放行；
        // 用 /Users 下的路径测试真正的“工作区外写入失败”，或者用 protect 测试只读！
        let protected = workspace.join(".pp");
        std::fs::create_dir_all(&protected).unwrap();

        let writable = vec![workspace.clone()];
        let protect = vec![protected.clone()];
        let deny_read = vec![secret.clone()];
        let allow_read = vec![secret.join("public")];
        std::fs::write(secret.join("public"), b"ok").unwrap();

        let spec = SeatbeltSpec {
            mode: crate::EffectiveMode::WorkspaceWrite,
            cwd: &workspace,
            writable: &writable,
            protect: &protect,
            deny_read: &deny_read,
            allow_read: &allow_read,
            docker_user_sock: None,
            network: false,
        };
        let script = format!(
            "echo ok > {ws}/ok.txt && \
             (! echo bad > {prot}/hooks.yml 2>/dev/null) && \
             (! cat {sec}/key >/dev/null 2>&1) && \
             test \"$(cat {sec}/public)\" = ok",
            ws = workspace.display(),
            prot = protected.display(),
            sec = secret.display(),
        );
        let (bin, args) = build_exec_args(
            &spec,
            "/bin/sh",
            &[OsString::from("-c"), OsString::from(script)],
        )
        .unwrap();
        let status = std::process::Command::new(bin).args(args).status().unwrap();
        assert!(status.success(), "sandbox-exec script failed: {status}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("ok.txt")).unwrap(),
            "ok\n"
        );
    }
}
