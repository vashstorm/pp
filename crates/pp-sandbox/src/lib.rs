mod guard;
mod seatbelt;

use std::{
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use anyhow::Result;
use pp_config::config::{SandboxMode, SandboxSettings};

/// 本进程级平台的沙盒探测结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeResult {
    Available,
    Unavailable { reason: String },
}

/// 实际执行层的有效模式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectiveMode {
    WorkspaceWrite,
    ReadOnly,
    Off,
}

impl EffectiveMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceWrite => "workspace-write",
            Self::ReadOnly => "read-only",
            Self::Off => "off",
        }
    }
}

/// 沙盒当前状态快照，供 `/sandbox`、降级提示和诊断使用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxStatus {
    pub configured: SandboxMode,
    pub effective: EffectiveMode,
    pub degraded_reason: Option<String>,
    pub network: bool,
    pub writable_summary: Vec<String>,
}

impl SandboxStatus {
    pub fn summary(&self) -> String {
        let mut lines = vec![format!(
            "Sandbox: effective={}, configured={}, network={}",
            self.effective.as_str(),
            self.configured.as_str(),
            if self.network {
                "on"
            } else {
                "off (loopback only)"
            }
        )];
        if let Some(reason) = &self.degraded_reason {
            lines.push(format!("Warning: degraded to off ({reason})"));
        }
        if !self.writable_summary.is_empty() {
            lines.push(format!(
                "Writable roots: {}",
                self.writable_summary.join(", ")
            ));
        }
        lines.join("\n")
    }
}

/// 进程级沙盒安装槽，由 `main::run` 构造并在 `Bootstrap` / `Agent` 间共享。
#[derive(Clone, Debug, Default)]
pub struct SharedSandbox {
    inner: Arc<std::sync::Mutex<Option<Arc<Sandbox>>>>,
}

impl SharedSandbox {
    pub fn get_or_init(&self, settings: &SandboxSettings, cwd: &Path) -> Arc<Sandbox> {
        let mut slot = self.inner.lock().expect("shared sandbox lock poisoned");
        if let Some(existing) = slot.as_ref() {
            return Arc::clone(existing);
        }
        let sbx = Arc::new(Sandbox::new(settings.clone(), cwd));
        *slot = Some(Arc::clone(&sbx));
        sbx
    }

    pub fn get(&self) -> Option<Arc<Sandbox>> {
        self.inner
            .lock()
            .expect("shared sandbox lock poisoned")
            .clone()
    }
}

/// 命中沙盒拦截时附加给工具结果的提示信息。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DenialMetadata {
    pub mode: &'static str,
    pub network: bool,
    pub writable_roots: Vec<String>,
    pub hint: String,
}

/// 不可变的策略快照：在单次命令或 tool 调度前取出，运行中切模式不影响已启动进程。
#[derive(Clone, Debug)]
pub struct Policy {
    mode: EffectiveMode,
    network: bool,
    cwd: PathBuf,
    writable: Vec<PathBuf>,
    protect: Vec<PathBuf>,
    deny_read: Vec<PathBuf>,
    allow_read: Vec<PathBuf>,
    docker_user_sock: Option<PathBuf>,
}

impl Policy {
    pub fn mode(&self) -> EffectiveMode {
        self.mode
    }

    pub fn network(&self) -> bool {
        self.network
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn writable_roots(&self) -> &[PathBuf] {
        &self.writable
    }

    /// 为单次运行（如 `read-only` 下的本次 scratch 目录）附加额外可写路径。
    pub fn with_extra_writable(&self, path: &Path) -> Arc<Self> {
        let mut next = self.clone();
        let normalized = guard::normalize_path(path);
        if !next.writable.iter().any(|existing| existing == &normalized) {
            next.writable.push(normalized);
        }
        Arc::new(next)
    }

    pub fn check_read(&self, path: &Path) -> io::Result<()> {
        let abs = self.absolutize(path);
        guard::check_read_path(&abs, &self.deny_read, &self.allow_read)
    }

    pub fn is_read_denied(&self, path: &Path) -> bool {
        let abs = self.absolutize(path);
        guard::is_read_denied_path(&abs, &self.deny_read, &self.allow_read)
    }

    pub fn check_write(&self, path: &Path) -> io::Result<()> {
        let abs = self.absolutize(path);
        guard::check_write_path(&abs, self.mode, &self.cwd, &self.writable, &self.protect)
    }

    pub fn command_wrapper_prefix(&self) -> Result<(String, Vec<OsString>)> {
        let spec = seatbelt::SeatbeltSpec {
            mode: self.mode,
            cwd: &self.cwd,
            writable: &self.writable,
            protect: &self.protect,
            deny_read: &self.deny_read,
            allow_read: &self.allow_read,
            docker_user_sock: self.docker_user_sock.as_ref(),
            network: self.network,
        };
        seatbelt::build_wrapper_prefix(&spec)
    }

    pub fn wrap_command(
        &self,
        program: &str,
        args: &[OsString],
    ) -> Result<(String, Vec<OsString>)> {
        let (bin, mut prefix) = self.command_wrapper_prefix()?;
        prefix.push(OsString::from(program));
        prefix.extend(args.iter().cloned());
        Ok((bin, prefix))
    }

    pub fn is_denied_output(output: &str) -> bool {
        output.contains("Operation not permitted") || output.contains("blocked by sandbox policy")
    }

    pub fn denial_metadata(&self) -> DenialMetadata {
        let writable_roots = self
            .writable
            .iter()
            .take(8)
            .map(|p| p.display().to_string())
            .collect();
        DenialMetadata {
            mode: self.mode.as_str(),
            network: self.network,
            writable_roots,
            hint: "Command hit the macOS sandbox (Operation not permitted). Explain the restriction to the user or suggest adjusting tools.sandbox in settings.yml or using /sandbox.".to_owned(),
        }
    }

    fn absolutize(&self, path: &Path) -> PathBuf {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        guard::normalize_path(&joined)
    }
}

#[derive(Debug)]
struct SandboxState {
    settings: SandboxSettings,
    cwd: PathBuf,
    home: Option<PathBuf>,
    tmpdir: Option<PathBuf>,
    probe: ProbeResult,
    override_mode: Option<SandboxMode>,
}

/// 进程级沙盒句柄，由 `main::run` 构造并向下传递给 `Bootstrap` / `Tools`。
#[derive(Debug)]
pub struct Sandbox {
    state: RwLock<SandboxState>,
}

impl Sandbox {
    pub fn new(settings: SandboxSettings, cwd: &Path) -> Self {
        let home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let tmpdir = std::env::var_os("TMPDIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let probe = seatbelt::probe_platform();
        Self::with_parts(
            settings,
            cwd,
            home.as_deref(),
            Some(tmpdir.as_path()),
            probe,
        )
    }

    pub fn disabled() -> Self {
        let settings = SandboxSettings {
            mode: SandboxMode::Off,
            ..SandboxSettings::default()
        };
        Self::with_parts(settings, Path::new("/"), None, None, ProbeResult::Available)
    }

    pub fn with_probe_for_test(
        settings: SandboxSettings,
        cwd: &Path,
        home: Option<&Path>,
        tmpdir: Option<&Path>,
        probe: ProbeResult,
    ) -> Self {
        Self::with_parts(settings, cwd, home, tmpdir, probe)
    }

    fn with_parts(
        settings: SandboxSettings,
        cwd: &Path,
        home: Option<&Path>,
        tmpdir: Option<&Path>,
        probe: ProbeResult,
    ) -> Self {
        let norm_cwd = guard::normalize_path(cwd);
        let norm_home = home.map(guard::normalize_path);
        let norm_tmpdir = tmpdir.map(guard::normalize_path);
        Self {
            state: RwLock::new(SandboxState {
                settings,
                cwd: norm_cwd,
                home: norm_home,
                tmpdir: norm_tmpdir,
                probe,
                override_mode: None,
            }),
        }
    }

    pub fn set_mode_override(&self, mode: Option<SandboxMode>) {
        let mut state = self.state.write().expect("sandbox state lock poisoned");
        state.override_mode = mode;
    }

    pub fn update_settings_mode(&self, mode: SandboxMode) {
        let mut state = self.state.write().expect("sandbox state lock poisoned");
        state.settings.mode = mode;
        state.override_mode = None;
    }

    pub fn for_cwd(&self, cwd: &Path) -> Arc<Self> {
        let state = self.state.read().expect("sandbox state lock poisoned");
        Arc::new(Self {
            state: RwLock::new(SandboxState {
                settings: state.settings.clone(),
                cwd: guard::normalize_path(cwd),
                home: state.home.clone(),
                tmpdir: state.tmpdir.clone(),
                probe: state.probe.clone(),
                override_mode: state.override_mode,
            }),
        })
    }

    pub fn status(&self) -> SandboxStatus {
        let state = self.state.read().expect("sandbox state lock poisoned");
        let configured = state.override_mode.unwrap_or(state.settings.mode);
        let (effective, degraded_reason) = resolve_effective(configured, &state.probe);
        let writable_summary = match effective {
            EffectiveMode::Off => Vec::new(),
            mode => build_policy(&state, mode)
                .writable
                .into_iter()
                .map(|p| p.display().to_string())
                .collect(),
        };
        SandboxStatus {
            configured,
            effective,
            degraded_reason,
            network: state.settings.network,
            writable_summary,
        }
    }

    /// 取得当前生效策略；显式开启但平台不可用时 fail-closed 返回错误。
    pub fn policy(&self) -> Result<Option<Arc<Policy>>> {
        let state = self.state.read().expect("sandbox state lock poisoned");
        let configured = state.override_mode.unwrap_or(state.settings.mode);
        match configured {
            SandboxMode::Off => Ok(None),
            SandboxMode::Auto => {
                if !cfg!(target_os = "macos") {
                    return Ok(None);
                }
                match &state.probe {
                    ProbeResult::Available => Ok(Some(Arc::new(build_policy(
                        &state,
                        EffectiveMode::WorkspaceWrite,
                    )))),
                    ProbeResult::Unavailable { .. } => Ok(None),
                }
            }
            SandboxMode::WorkspaceWrite | SandboxMode::ReadOnly => match &state.probe {
                ProbeResult::Available => {
                    let mode = if configured == SandboxMode::WorkspaceWrite {
                        EffectiveMode::WorkspaceWrite
                    } else {
                        EffectiveMode::ReadOnly
                    };
                    Ok(Some(Arc::new(build_policy(&state, mode))))
                }
                ProbeResult::Unavailable { reason } => {
                    anyhow::bail!(
                        "sandbox mode '{}' is enabled but unavailable: {reason}",
                        configured.as_str()
                    );
                }
            },
        }
    }

    pub fn check_read(&self, path: &Path) -> io::Result<()> {
        let policy = self
            .policy()
            .map_err(|err| io::Error::new(io::ErrorKind::PermissionDenied, err.to_string()))?;
        let Some(policy) = policy else {
            return Ok(());
        };
        policy.check_read(path)
    }

    pub fn is_read_denied(&self, path: &Path) -> bool {
        match self.policy() {
            Ok(Some(policy)) => policy.is_read_denied(path),
            Ok(None) => false,
            // fail-closed：显式开启却不可用时不悄悄暴露敏感目录
            Err(_) => true,
        }
    }
}

fn resolve_effective(
    configured: SandboxMode,
    probe: &ProbeResult,
) -> (EffectiveMode, Option<String>) {
    match configured {
        SandboxMode::Off => (EffectiveMode::Off, None),
        SandboxMode::Auto => {
            if !cfg!(target_os = "macos") {
                return (EffectiveMode::Off, None);
            }
            match probe {
                ProbeResult::Available => (EffectiveMode::WorkspaceWrite, None),
                ProbeResult::Unavailable { reason } => (EffectiveMode::Off, Some(reason.clone())),
            }
        }
        SandboxMode::WorkspaceWrite => match probe {
            ProbeResult::Available => (EffectiveMode::WorkspaceWrite, None),
            ProbeResult::Unavailable { reason } => (EffectiveMode::Off, Some(reason.clone())),
        },
        SandboxMode::ReadOnly => match probe {
            ProbeResult::Available => (EffectiveMode::ReadOnly, None),
            ProbeResult::Unavailable { reason } => (EffectiveMode::Off, Some(reason.clone())),
        },
    }
}

fn build_policy(state: &SandboxState, mode: EffectiveMode) -> Policy {
    let home = state.home.as_deref();
    let mut writable = Vec::new();
    let push_unique = |vec: &mut Vec<PathBuf>, p: PathBuf| {
        if !vec.iter().any(|existing| existing == &p) {
            vec.push(p);
        }
    };

    if let Some(tmpdir) = &state.tmpdir {
        push_unique(&mut writable, tmpdir.clone());
    }
    push_unique(&mut writable, PathBuf::from("/private/tmp"));
    push_unique(&mut writable, state.cwd.join(".pp-cache"));

    if mode == EffectiveMode::WorkspaceWrite {
        push_unique(&mut writable, state.cwd.clone());
        if let Some(home) = home {
            for rel in [
                ".cargo/registry",
                ".cargo/git",
                ".cargo/.package-cache",
                ".cache",
                "Library/Caches",
                ".npm",
                ".bun/install/cache",
                "go/pkg/mod",
            ] {
                push_unique(&mut writable, guard::normalize_path(&home.join(rel)));
            }
        }
        for raw in &state.settings.writable {
            if let Some(path) = guard::expand_tilde(raw, home) {
                push_unique(&mut writable, guard::normalize_path(&path));
            }
        }
    }

    let mut protect = Vec::new();
    for rel in [
        ".git/hooks",
        ".git/config",
        ".pp",
        ".mcp.json",
        ".claude/.mcp.json",
    ] {
        push_unique(&mut protect, guard::normalize_path(&state.cwd.join(rel)));
    }
    for rel in &state.settings.protect {
        push_unique(&mut protect, guard::normalize_path(&state.cwd.join(rel)));
    }

    let mut deny_read = Vec::new();
    if let Some(home) = home {
        for rel in [
            ".ssh",
            ".aws",
            ".gnupg",
            ".azure",
            ".kube",
            ".config/gcloud",
            ".config/gh",
            ".docker/config.json",
            ".netrc",
            ".npmrc",
            ".pypirc",
            ".git-credentials",
            "Library/Keychains",
            "Library/Cookies",
            ".claude/.credentials.json",
            ".codex/auth.json",
            ".pp/agent",
        ] {
            push_unique(&mut deny_read, guard::normalize_path(&home.join(rel)));
        }
    }
    for raw in &state.settings.deny_read {
        if let Some(path) = guard::expand_tilde(raw, home) {
            push_unique(&mut deny_read, guard::normalize_path(&path));
        }
    }

    let mut allow_read = Vec::new();
    if let Some(home) = home {
        for rel in [
            ".pp/agent/python-env",
            ".pp/agent/skills",
            ".pp/agent/commands",
            ".pp/agent/hooks",
            ".pp/agent/cache",
        ] {
            push_unique(&mut allow_read, guard::normalize_path(&home.join(rel)));
        }
    }
    for raw in &state.settings.allow_read {
        if let Some(path) = guard::expand_tilde(raw, home) {
            push_unique(&mut allow_read, guard::normalize_path(&path));
        }
    }

    let docker_user_sock = home.map(|h| guard::normalize_path(&h.join(".docker/run/docker.sock")));

    Policy {
        mode,
        network: state.settings.network,
        cwd: state.cwd.clone(),
        writable,
        protect,
        deny_read,
        allow_read,
        docker_user_sock,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_degradation_differs_between_auto_and_explicit_modes() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let unavailable = ProbeResult::Unavailable {
            reason: "nested sandbox".into(),
        };

        let auto = Sandbox::with_probe_for_test(
            SandboxSettings {
                mode: SandboxMode::Auto,
                ..Default::default()
            },
            dir.path(),
            Some(&home),
            None,
            unavailable.clone(),
        );
        assert!(auto.policy().unwrap().is_none());
        let st = auto.status();
        assert_eq!(st.effective, EffectiveMode::Off);
        if cfg!(target_os = "macos") {
            assert_eq!(st.degraded_reason.as_deref(), Some("nested sandbox"));
        }

        for explicit in [SandboxMode::WorkspaceWrite, SandboxMode::ReadOnly] {
            let sbx = Sandbox::with_probe_for_test(
                SandboxSettings {
                    mode: explicit,
                    ..Default::default()
                },
                dir.path(),
                Some(&home),
                None,
                unavailable.clone(),
            );
            assert!(
                sbx.policy().is_err(),
                "explicit mode {explicit:?} must fail closed"
            );
        }
    }

    #[test]
    fn policy_sets_expected_defaults_for_workspace_write_and_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let cwd = root.join("repo");
        let home = PathBuf::from("/Users/__pp_sandbox_test_home__");
        std::fs::create_dir_all(&cwd).unwrap();

        let sbx = Sandbox::with_probe_for_test(
            SandboxSettings {
                mode: SandboxMode::WorkspaceWrite,
                allow_read: vec!["~/.ssh/known_hosts".into()],
                protect: vec![".envrc".into()],
                ..Default::default()
            },
            &cwd,
            Some(&home),
            Some(Path::new("/tmp")),
            ProbeResult::Available,
        );
        let policy = sbx.policy().unwrap().unwrap();
        assert_eq!(policy.mode(), EffectiveMode::WorkspaceWrite);
        assert!(policy.check_write(&cwd.join("src/a.rs")).is_ok());
        assert!(
            policy
                .check_write(&PathBuf::from("/tmp/sbx_test_file"))
                .is_ok()
        );
        assert!(policy.check_write(&cwd.join(".pp/hooks.yml")).is_err());
        assert!(policy.check_write(&cwd.join(".envrc")).is_err());
        assert!(
            policy
                .check_write(&cwd.join(".git/hooks/pre-commit"))
                .is_err()
        );
        assert!(policy.check_write(&home.join(".zshrc")).is_err());
        assert!(policy.check_read(&home.join(".ssh/id_rsa")).is_err());
        assert!(policy.check_read(&home.join(".ssh/known_hosts")).is_ok());
        assert!(
            policy
                .check_read(&home.join(".pp/agent/models.yml"))
                .is_err()
        );
        assert!(
            policy
                .check_read(&home.join(".pp/agent/skills/x/SKILL.md"))
                .is_ok()
        );

        sbx.set_mode_override(Some(SandboxMode::ReadOnly));
        let ro = sbx.policy().unwrap().unwrap();
        assert_eq!(ro.mode(), EffectiveMode::ReadOnly);
        assert!(ro.check_write(&cwd.join("src/a.rs")).is_err());
        assert!(ro.check_write(&cwd.join(".pp-cache/x")).is_ok());
        let ro_scratch = ro.with_extra_writable(&cwd.join(".pp-tmp-123"));
        assert!(ro_scratch.check_write(&cwd.join(".pp-tmp-123/out")).is_ok());
    }
}
