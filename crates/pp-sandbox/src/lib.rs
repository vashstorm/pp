#[cfg(target_os = "linux")]
mod bwrap;
mod guard;
#[cfg(target_os = "macos")]
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

/// 当前平台支持的沙盒后端名称。
pub fn platform_backend() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "seatbelt"
    }
    #[cfg(target_os = "linux")]
    {
        "bubblewrap"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "unsupported"
    }
}

fn platform_probe(network: bool) -> ProbeResult {
    #[cfg(target_os = "macos")]
    {
        let _ = network;
        seatbelt::probe_platform()
    }
    #[cfg(target_os = "linux")]
    {
        bwrap::probe_platform(network)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = network;
        ProbeResult::Unavailable {
            reason: "sandbox is currently only supported on macOS and Linux".to_owned(),
        }
    }
}

/// 沙盒当前状态快照，供 `/sandbox`、降级提示和诊断使用。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxStatus {
    pub configured: SandboxMode,
    pub effective: EffectiveMode,
    pub degraded_reason: Option<String>,
    pub backend: &'static str,
    pub network: bool,
    pub writable_summary: Vec<String>,
    pub notes: Vec<String>,
}

impl SandboxStatus {
    pub fn summary(&self) -> String {
        let net_desc = if self.network {
            "on"
        } else if self.backend == "bubblewrap" {
            "off (isolated network namespace)"
        } else {
            "off (loopback only)"
        };
        let mut lines = vec![format!(
            "Sandbox: effective={}, configured={}, backend={}, network={net_desc}",
            self.effective.as_str(),
            self.configured.as_str(),
            self.backend,
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
        for note in &self.notes {
            lines.push(format!("Note: {note}"));
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
    /// 词法快路径的“可能命中”集合：拒绝根本身加系统别名（如 macOS 的
    /// `/tmp` 与 `/private/tmp`），避免未规范化的别名被误判为不相交。
    deny_read_lexical: Vec<PathBuf>,
    allow_read: Vec<PathBuf>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    docker_user_sock: Option<PathBuf>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    docker_sockets: Vec<PathBuf>,
}

impl Policy {
    pub fn mode(&self) -> EffectiveMode {
        self.mode
    }

    pub fn backend(&self) -> &'static str {
        platform_backend()
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

    /// 遍历剪枝判定：绝对且与拒绝根不相交的路径直接放行，其余情况规范化后判定。
    /// 路径已存在的祖先必须已解析（walk 根 `canonicalize` + `follow_links(false)`）；
    /// symlink entry 使用 `is_read_denied_resolved`。
    pub fn is_read_denied(&self, path: &Path) -> bool {
        let joined = self.join_cwd(path);
        if guard::lexically_outside_deny(&joined, &self.deny_read_lexical) {
            return false;
        }
        guard::is_read_denied_path(
            &guard::normalize_path(&joined),
            &self.deny_read,
            &self.allow_read,
        )
    }

    /// 判断以 `canonical_root` 为根（已通过 `check_read` 且 `follow_links(false)`）的子树
    /// 内非 symlink 条目是否可能命中任何读拒绝根。不相交时遍历期可 O(1) 放行常规 entry。
    pub fn can_contain_denied_read(&self, canonical_root: &Path) -> bool {
        self.deny_read_lexical
            .iter()
            .any(|denied| denied.starts_with(canonical_root) || canonical_root.starts_with(denied))
    }

    /// 精确判定：总是先规范化（含 symlink 与 `/tmp` 折叠）。遍历剪枝只对
    /// symlink entry 使用，因为词法快路径无法预知链接目标。
    pub fn is_read_denied_resolved(&self, path: &Path) -> bool {
        let abs = self.absolutize(path);
        guard::is_read_denied_path(&abs, &self.deny_read, &self.allow_read)
    }

    pub fn check_write(&self, path: &Path) -> io::Result<()> {
        let abs = self.absolutize(path);
        guard::check_write_path(&abs, self.mode, &self.cwd, &self.writable, &self.protect)
    }

    pub fn command_wrapper_prefix(&self) -> Result<(String, Vec<OsString>)> {
        #[cfg(target_os = "macos")]
        {
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
        #[cfg(target_os = "linux")]
        {
            let spec = bwrap::BwrapSpec {
                mode: self.mode,
                cwd: &self.cwd,
                writable: &self.writable,
                protect: &self.protect,
                deny_read: &self.deny_read,
                allow_read: &self.allow_read,
                docker_sockets: &self.docker_sockets,
                network: self.network,
            };
            bwrap::build_wrapper_prefix(&spec)
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = (
                &self.docker_user_sock,
                &self.docker_sockets,
                self.mode,
                self.network,
            );
            anyhow::bail!("sandbox wrapper is unsupported on this platform")
        }
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
        output.contains("blocked by sandbox policy")
            || output.contains("Operation not permitted")
            || if cfg!(target_os = "linux") {
                output.contains("Permission denied") || output.contains("Read-only file system")
            } else {
                false
            }
    }

    pub fn denial_metadata(&self) -> DenialMetadata {
        let writable_roots = self
            .writable
            .iter()
            .take(8)
            .map(|p| p.display().to_string())
            .collect();
        let hint = if cfg!(target_os = "linux") {
            "Command hit the Linux bubblewrap sandbox (Permission denied / Read-only file system). Explain the restriction to the user or suggest adjusting tools.sandbox in settings.yml or using /sandbox."
        } else {
            "Command hit the macOS sandbox (Operation not permitted). Explain the restriction to the user or suggest adjusting tools.sandbox in settings.yml or using /sandbox."
        };
        DenialMetadata {
            mode: self.mode.as_str(),
            network: self.network,
            writable_roots,
            hint: hint.to_owned(),
        }
    }

    fn join_cwd<'a>(&self, path: &'a Path) -> std::borrow::Cow<'a, Path> {
        if path.is_absolute() {
            std::borrow::Cow::Borrowed(path)
        } else {
            std::borrow::Cow::Owned(self.cwd.join(path))
        }
    }

    fn absolutize(&self, path: &Path) -> PathBuf {
        guard::normalize_path(&self.join_cwd(path))
    }
}

#[derive(Debug)]
struct SandboxState {
    settings: SandboxSettings,
    cwd: PathBuf,
    home: Option<PathBuf>,
    tmpdir: Option<PathBuf>,
    xdg_cache_home: Option<PathBuf>,
    xdg_runtime_dir: Option<PathBuf>,
    /// 平台探测结果；`None` 表示以 `off` 起步、从未探测过，切到需要沙盒的模式时补测。
    probe: Option<ProbeResult>,
    override_mode: Option<SandboxMode>,
    cached_workspace_write: Option<Arc<Policy>>,
    cached_read_only: Option<Arc<Policy>>,
}

impl SandboxState {
    fn cached_policy(&self, mode: EffectiveMode) -> Option<&Arc<Policy>> {
        match mode {
            EffectiveMode::WorkspaceWrite => self.cached_workspace_write.as_ref(),
            EffectiveMode::ReadOnly => self.cached_read_only.as_ref(),
            EffectiveMode::Off => None,
        }
    }

    fn get_or_build_policy(&mut self, mode: EffectiveMode) -> Arc<Policy> {
        if let Some(policy) = self.cached_policy(mode) {
            return Arc::clone(policy);
        }
        let policy = Arc::new(build_policy(self, mode));
        let slot = match mode {
            EffectiveMode::WorkspaceWrite => &mut self.cached_workspace_write,
            EffectiveMode::ReadOnly => &mut self.cached_read_only,
            EffectiveMode::Off => unreachable!("off mode has no policy"),
        };
        *slot = Some(Arc::clone(&policy));
        policy
    }
    /// 当前生效模式：会话级覆盖优先于 settings 里的值。
    fn configured(&self) -> SandboxMode {
        self.override_mode.unwrap_or(self.settings.mode)
    }

    /// 探测结果的只读投影；`None` 只可能出现在 `off` 状态，而 `off` 分支不消费该值。
    fn probe_or_available(&self) -> ProbeResult {
        match &self.probe {
            Some(probe) => probe.clone(),
            None => {
                debug_assert_eq!(
                    self.configured(),
                    SandboxMode::Off,
                    "未探测的状态只允许出现在 off 模式"
                );
                ProbeResult::Available
            }
        }
    }

    /// 切到需要沙盒的模式后补一次探测；`off` 保持未探测，避免白起一次沙盒探针子进程。
    fn ensure_probe(&mut self) {
        if self.probe.is_none() && self.configured() != SandboxMode::Off {
            self.probe = Some(platform_probe(self.settings.network));
        }
    }
}

/// 进程级沙盒句柄，由 `main::run` 构造并向下传递给 `Bootstrap` / `Tools`。
#[derive(Debug)]
pub struct Sandbox {
    state: RwLock<SandboxState>,
}

impl Sandbox {
    /// `mode: off` 不探测平台能力：关闭模式下不会读取探测结果，也省掉启动期一次
    /// 沙盒探针子进程；之后切到启用模式时才补测。
    pub fn new(settings: SandboxSettings, cwd: &Path) -> Self {
        let network = settings.network;
        Self::build_with_probe(settings, cwd, move || platform_probe(network))
    }

    fn build_with_probe(
        settings: SandboxSettings,
        cwd: &Path,
        probe: impl FnOnce() -> ProbeResult,
    ) -> Self {
        let home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let tmpdir = std::env::var_os("TMPDIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let xdg_cache_home = std::env::var_os("XDG_CACHE_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let xdg_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let probe = (settings.mode != SandboxMode::Off).then(probe);
        Self::with_parts(
            settings,
            cwd,
            home.as_deref(),
            Some(tmpdir.as_path()),
            xdg_cache_home.as_deref(),
            xdg_runtime_dir.as_deref(),
            probe,
        )
    }

    pub fn disabled() -> Self {
        let settings = SandboxSettings {
            mode: SandboxMode::Off,
            ..SandboxSettings::default()
        };
        Self::with_parts(
            settings,
            Path::new("/"),
            None,
            None,
            None,
            None,
            Some(ProbeResult::Available),
        )
    }

    pub fn with_probe_for_test(
        settings: SandboxSettings,
        cwd: &Path,
        home: Option<&Path>,
        tmpdir: Option<&Path>,
        probe: ProbeResult,
    ) -> Self {
        Self::with_parts(settings, cwd, home, tmpdir, None, None, Some(probe))
    }

    fn with_parts(
        settings: SandboxSettings,
        cwd: &Path,
        home: Option<&Path>,
        tmpdir: Option<&Path>,
        xdg_cache_home: Option<&Path>,
        xdg_runtime_dir: Option<&Path>,
        probe: Option<ProbeResult>,
    ) -> Self {
        let norm_cwd = guard::normalize_path(cwd);
        let norm_home = home.map(guard::normalize_path);
        let norm_tmpdir = tmpdir.map(guard::normalize_path);
        let norm_xdg_cache = xdg_cache_home.map(guard::normalize_path);
        let norm_xdg_runtime = xdg_runtime_dir.map(guard::normalize_path);
        Self {
            state: RwLock::new(SandboxState {
                settings,
                cwd: norm_cwd,
                home: norm_home,
                tmpdir: norm_tmpdir,
                xdg_cache_home: norm_xdg_cache,
                xdg_runtime_dir: norm_xdg_runtime,
                probe,
                override_mode: None,
                cached_workspace_write: None,
                cached_read_only: None,
            }),
        }
    }

    pub fn mode_override(&self) -> Option<SandboxMode> {
        self.state
            .read()
            .expect("sandbox state lock poisoned")
            .override_mode
    }

    pub fn set_mode_override(&self, mode: Option<SandboxMode>) {
        let mut state = self.state.write().expect("sandbox state lock poisoned");
        state.override_mode = mode;
        state.ensure_probe();
    }

    pub fn update_settings_mode(&self, mode: SandboxMode) {
        let mut state = self.state.write().expect("sandbox state lock poisoned");
        state.settings.mode = mode;
        state.override_mode = None;
        state.ensure_probe();
    }

    pub fn for_cwd(&self, cwd: &Path) -> Arc<Self> {
        let state = self.state.read().expect("sandbox state lock poisoned");
        Arc::new(Self {
            state: RwLock::new(SandboxState {
                settings: state.settings.clone(),
                cwd: guard::normalize_path(cwd),
                home: state.home.clone(),
                tmpdir: state.tmpdir.clone(),
                xdg_cache_home: state.xdg_cache_home.clone(),
                xdg_runtime_dir: state.xdg_runtime_dir.clone(),
                probe: state.probe.clone(),
                override_mode: state.override_mode,
                cached_workspace_write: None,
                cached_read_only: None,
            }),
        })
    }

    pub fn status(&self) -> SandboxStatus {
        let (configured, probe, network) = {
            let state = self.state.read().expect("sandbox state lock poisoned");
            (
                state.configured(),
                state.probe_or_available(),
                state.settings.network,
            )
        };
        let (effective, degraded_reason) = resolve_effective(configured, &probe);
        let (writable_summary, notes) = match effective {
            EffectiveMode::Off => (Vec::new(), Vec::new()),
            mode => {
                let policy = self.policy_for_effective(mode);
                #[cfg(target_os = "linux")]
                let notes = {
                    let spec = bwrap::BwrapSpec {
                        mode: policy.mode,
                        cwd: &policy.cwd,
                        writable: &policy.writable,
                        protect: &policy.protect,
                        deny_read: &policy.deny_read,
                        allow_read: &policy.allow_read,
                        docker_sockets: &policy.docker_sockets,
                        network: policy.network,
                    };
                    bwrap::policy_notes(&spec)
                };
                #[cfg(not(target_os = "linux"))]
                let notes = Vec::new();
                let roots = policy
                    .writable
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect();
                (roots, notes)
            }
        };
        SandboxStatus {
            configured,
            effective,
            degraded_reason,
            backend: platform_backend(),
            network,
            writable_summary,
            notes,
        }
    }

    fn policy_for_effective(&self, mode: EffectiveMode) -> Arc<Policy> {
        {
            let state = self.state.read().expect("sandbox state lock poisoned");
            if let Some(policy) = state.cached_policy(mode) {
                return Arc::clone(policy);
            }
        }
        let mut state = self.state.write().expect("sandbox state lock poisoned");
        state.get_or_build_policy(mode)
    }

    /// 取得当前生效策略；显式开启但平台不可用时 fail-closed 返回错误。
    pub fn policy(&self) -> Result<Option<Arc<Policy>>> {
        let effective = {
            let state = self.state.read().expect("sandbox state lock poisoned");
            let configured = state.configured();
            if configured == SandboxMode::Off {
                return Ok(None);
            }
            let probe = state.probe_or_available();
            match configured {
                SandboxMode::Off => return Ok(None),
                SandboxMode::Auto => {
                    if !cfg!(any(target_os = "macos", target_os = "linux")) {
                        return Ok(None);
                    }
                    match &probe {
                        ProbeResult::Available => EffectiveMode::WorkspaceWrite,
                        ProbeResult::Unavailable { .. } => return Ok(None),
                    }
                }
                SandboxMode::WorkspaceWrite | SandboxMode::ReadOnly => match &probe {
                    ProbeResult::Available => {
                        if configured == SandboxMode::WorkspaceWrite {
                            EffectiveMode::WorkspaceWrite
                        } else {
                            EffectiveMode::ReadOnly
                        }
                    }
                    ProbeResult::Unavailable { reason } => {
                        anyhow::bail!(
                            "sandbox mode '{}' is enabled but unavailable: {reason}",
                            configured.as_str()
                        );
                    }
                },
            }
        };
        Ok(Some(self.policy_for_effective(effective)))
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

    /// 精确判定：总会先规范化（含 symlink 解析）。遍历剪枝只对 symlink entry
    /// 使用；普通 entry 走 `is_read_denied` 的词法快路径。
    pub fn is_read_denied_resolved(&self, path: &Path) -> bool {
        match self.policy() {
            Ok(Some(policy)) => policy.is_read_denied_resolved(path),
            Ok(None) => false,
            // fail-closed：无法取得策略时与 `is_read_denied` 保持一致
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
            if !cfg!(any(target_os = "macos", target_os = "linux")) {
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
    if cfg!(target_os = "macos") {
        push_unique(&mut writable, PathBuf::from("/private/tmp"));
    } else if cfg!(target_os = "linux") {
        push_unique(&mut writable, guard::normalize_path(Path::new("/tmp")));
        push_unique(&mut writable, guard::normalize_path(Path::new("/var/tmp")));
    }
    push_unique(&mut writable, state.cwd.join(".pp-cache"));

    if mode == EffectiveMode::WorkspaceWrite {
        push_unique(&mut writable, state.cwd.clone());
        if let Some(home) = home {
            for rel in [
                ".cargo/registry",
                ".cargo/git",
                ".cargo/.package-cache",
                ".cache",
                ".npm",
                ".bun/install/cache",
                "go/pkg/mod",
            ] {
                push_unique(&mut writable, guard::normalize_path(&home.join(rel)));
            }
            if cfg!(target_os = "macos") {
                push_unique(
                    &mut writable,
                    guard::normalize_path(&home.join("Library/Caches")),
                );
            }
        }
        if cfg!(target_os = "linux")
            && let Some(xdg_cache) = &state.xdg_cache_home
        {
            push_unique(&mut writable, xdg_cache.clone());
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
            ".claude/.credentials.json",
            ".codex/auth.json",
            ".pp/agent",
        ] {
            push_unique(&mut deny_read, guard::normalize_path(&home.join(rel)));
        }
        if cfg!(target_os = "macos") {
            for rel in ["Library/Keychains", "Library/Cookies"] {
                push_unique(&mut deny_read, guard::normalize_path(&home.join(rel)));
            }
        } else if cfg!(target_os = "linux") {
            push_unique(
                &mut deny_read,
                guard::normalize_path(&home.join(".local/share/keyrings")),
            );
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
    let mut docker_sockets = Vec::new();
    if cfg!(target_os = "linux") {
        push_unique(
            &mut docker_sockets,
            guard::normalize_path(Path::new("/run/docker.sock")),
        );
        push_unique(
            &mut docker_sockets,
            guard::normalize_path(Path::new("/var/run/docker.sock")),
        );
        if let Some(sock) = &docker_user_sock {
            push_unique(&mut docker_sockets, sock.clone());
        }
        if let Some(runtime_dir) = &state.xdg_runtime_dir {
            push_unique(
                &mut docker_sockets,
                guard::normalize_path(&runtime_dir.join("docker.sock")),
            );
        }
    }

    // 词法快路径的“可能命中”集合：拒绝根本身加系统别名（`/tmp` ↔ `/private/tmp`）。
    let mut deny_read_lexical = deny_read.clone();
    for root in &deny_read {
        for alias in guard::lexical_aliases(root) {
            if !deny_read_lexical.contains(&alias) {
                deny_read_lexical.push(alias);
            }
        }
    }

    Policy {
        mode,
        network: state.settings.network,
        cwd: state.cwd.clone(),
        writable,
        protect,
        deny_read,
        deny_read_lexical,
        allow_read,
        docker_user_sock,
        docker_sockets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_mode_skips_the_platform_probe() {
        let dir = tempfile::tempdir().unwrap();

        let mut probes = 0;
        let off = Sandbox::build_with_probe(
            SandboxSettings {
                mode: SandboxMode::Off,
                ..Default::default()
            },
            dir.path(),
            || {
                probes += 1;
                ProbeResult::Unavailable {
                    reason: "off mode must not probe".into(),
                }
            },
        );
        assert_eq!(probes, 0, "mode=off 不得调用平台探测");
        let status = off.status();
        assert_eq!(status.configured, SandboxMode::Off);
        assert_eq!(status.effective, EffectiveMode::Off);
        assert_eq!(status.degraded_reason, None);
        assert!(off.policy().unwrap().is_none());

        // 同一个探测入口在启用模式下必须恰好调用一次：跳过逻辑不能变成探测整体失效。
        let mut probes = 0;
        let enabled = Sandbox::build_with_probe(
            SandboxSettings {
                mode: SandboxMode::WorkspaceWrite,
                ..Default::default()
            },
            dir.path(),
            || {
                probes += 1;
                ProbeResult::Available
            },
        );
        assert_eq!(probes, 1);
        assert_eq!(enabled.status().effective, EffectiveMode::WorkspaceWrite);
    }

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
        if cfg!(any(target_os = "macos", target_os = "linux")) {
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

    #[test]
    fn sandbox_lexical_fast_path_skips_canonicalize_for_disjoint_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let cwd = root.join("repo");
        let home = root.join("home");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_rsa"), "").unwrap();

        let sbx = Sandbox::with_probe_for_test(
            SandboxSettings {
                mode: SandboxMode::WorkspaceWrite,
                ..Default::default()
            },
            &cwd,
            Some(&home),
            Some(&root),
            ProbeResult::Available,
        );
        let policy = sbx.policy().unwrap().unwrap();

        crate::guard::reset_canonicalize_calls();
        assert!(!policy.is_read_denied(&cwd.join("src/main.rs")));
        assert_eq!(
            crate::guard::canonicalize_calls(),
            0,
            "与拒绝根不相交的绝对路径不得触发 canonicalize"
        );

        // 词法上可能命中（拒绝根后代）时必须回退到规范化，且判定仍为拒绝。
        crate::guard::reset_canonicalize_calls();
        assert!(policy.is_read_denied(&home.join(".ssh/id_rsa")));
        assert!(
            crate::guard::canonicalize_calls() > 0,
            "可能命中时必须走 normalize_path"
        );
    }

    #[test]
    fn sandbox_resolved_denial_keeps_symlink_and_tmp_alias_boundaries() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let cwd = root.join("repo");
        let home = root.join("home");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_rsa"), "").unwrap();

        let alias_root = std::env::temp_dir().join("pp-sandbox-lexical-alias");
        let sbx = Sandbox::with_probe_for_test(
            SandboxSettings {
                mode: SandboxMode::WorkspaceWrite,
                deny_read: vec![alias_root.to_string_lossy().into_owned()],
                ..Default::default()
            },
            &cwd,
            Some(&home),
            Some(&root),
            ProbeResult::Available,
        );
        let policy = sbx.policy().unwrap().unwrap();

        let link = cwd.join("secret-link");
        std::os::unix::fs::symlink(home.join(".ssh"), &link).unwrap();
        let dangling = cwd.join("dangling-link");
        std::os::unix::fs::symlink(home.join(".ssh/id_rsa"), &dangling).unwrap();
        let relative = cwd.join("relative-link");
        std::os::unix::fs::symlink("../home/.ssh", &relative).unwrap();
        assert!(policy.is_read_denied_resolved(&link));
        assert!(policy.is_read_denied_resolved(&dangling));
        assert!(policy.is_read_denied_resolved(&relative));
        assert!(!policy.is_read_denied_resolved(&cwd.join("src/main.rs")));

        // 系统别名（macOS `/tmp` ↔ `/private/tmp`、TMPDIR 的 `/var` ↔ `/private/var`）
        // 不得因词法不相交而被放行。
        assert!(policy.is_read_denied(&alias_root.join("id_rsa")));
    }
}
