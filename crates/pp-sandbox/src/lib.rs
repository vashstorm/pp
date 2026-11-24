#[cfg(target_os = "linux")]
mod bwrap;
mod guard;
mod policy;
#[cfg(target_os = "macos")]
mod seatbelt;

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use anyhow::Result;
use pp_config::config::{SandboxMode, SandboxSettings};

pub use policy::{DenialMetadata, Policy, SandboxOverlay, WriteAccessCheck};

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
    pub fn warning(&self) -> Option<String> {
        self.degraded_reason.as_ref().map(|reason| {
            if self.configured == SandboxMode::Auto {
                format!("Sandbox degraded to off: {reason}")
            } else {
                format!("Sandbox unavailable; commands requiring it are blocked: {reason}")
            }
        })
    }

    pub fn summary(&self) -> String {
        let unavailable = self.degraded_reason.is_some()
            && matches!(
                self.configured,
                SandboxMode::WorkspaceWrite | SandboxMode::ReadOnly
            );
        let mode = if unavailable {
            "unavailable"
        } else {
            self.effective.as_str()
        };
        let net_desc = if unavailable {
            "unavailable"
        } else if self.effective == EffectiveMode::Off {
            "unrestricted"
        } else if self.network {
            "on"
        } else if self.backend == "bubblewrap" {
            "off (isolated network namespace)"
        } else {
            "off (loopback only)"
        };
        let mut lines = vec![format!(
            "Sandbox: effective={mode}, configured={}, backend={}, network={net_desc}",
            self.configured.as_str(),
            self.backend,
        )];
        if let Some(warning) = self.warning() {
            lines.push(format!("Warning: {warning}"));
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
        self.get_or_init_with_override(settings, cwd, None)
    }

    pub fn get_or_init_with_override(
        &self,
        settings: &SandboxSettings,
        cwd: &Path,
        mode_override: Option<SandboxMode>,
    ) -> Arc<Sandbox> {
        let mut slot = self.inner.lock().expect("shared sandbox lock poisoned");
        if let Some(existing) = slot.as_ref() {
            return Arc::clone(existing);
        }
        let sbx = Arc::new(Sandbox::new(settings.clone(), cwd));
        if let Some(mode) = mode_override {
            sbx.set_mode_override(Some(mode));
        }
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

#[derive(Debug)]
pub(crate) struct SandboxState {
    pub(crate) settings: SandboxSettings,
    pub(crate) overlay: SandboxOverlay,
    pub(crate) cwd: PathBuf,
    pub(crate) home: Option<PathBuf>,
    pub(crate) tmpdir: Option<PathBuf>,
    pub(crate) xdg_cache_home: Option<PathBuf>,
    pub(crate) xdg_runtime_dir: Option<PathBuf>,
    /// 平台探测结果；`None` 表示以 `off` 起步、从未探测过，切到需要沙盒的模式时补测。
    pub(crate) probe: Option<ProbeResult>,
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
        let policy = Arc::new(policy::build_policy(self, mode));
        let slot = match mode {
            EffectiveMode::WorkspaceWrite => &mut self.cached_workspace_write,
            EffectiveMode::ReadOnly => &mut self.cached_read_only,
            EffectiveMode::Off => unreachable!("off mode has no policy"),
        };
        *slot = Some(Arc::clone(&policy));
        policy
    }

    fn effective_network(&self) -> bool {
        self.overlay.network.unwrap_or(self.settings.network)
    }

    /// 切到需要沙盒的模式后补一次探测；`off` 保持未探测，避免白起一次沙盒探针子进程。
    fn ensure_probe(&mut self, mode: SandboxMode) {
        if self.probe.is_none() && mode != SandboxMode::Off {
            self.probe = Some(platform_probe(self.effective_network()));
        }
    }
}

#[derive(Debug)]
struct ModeSource {
    configured: SandboxMode,
    override_mode: Option<SandboxMode>,
}

/// 进程级沙盒句柄，由 `main::run` 构造并向下传递给 `Bootstrap` / `Tools`。
#[derive(Debug)]
pub struct Sandbox {
    state: RwLock<SandboxState>,
    modes: Arc<std::sync::Mutex<ModeSource>>,
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
        let configured = settings.mode;
        Self {
            state: RwLock::new(SandboxState {
                settings,
                overlay: SandboxOverlay::default(),
                cwd: norm_cwd,
                home: norm_home,
                tmpdir: norm_tmpdir,
                xdg_cache_home: norm_xdg_cache,
                xdg_runtime_dir: norm_xdg_runtime,
                probe,
                cached_workspace_write: None,
                cached_read_only: None,
            }),
            modes: Arc::new(std::sync::Mutex::new(ModeSource {
                configured,
                override_mode: None,
            })),
        }
    }

    /// 合并来自 `permission.toml` 激活 profile 的文件系统与网络覆盖，并刷新策略缓存。
    pub fn set_permission_overlay(&self, overlay: SandboxOverlay) {
        let mut state = self.state.write().expect("sandbox state lock poisoned");
        if state.overlay != overlay {
            if state.overlay.network != overlay.network {
                state.probe = None;
            }
            state.overlay = overlay;
            state.cached_workspace_write = None;
            state.cached_read_only = None;
        }
    }

    pub fn mode_override(&self) -> Option<SandboxMode> {
        self.modes
            .lock()
            .expect("sandbox modes lock poisoned")
            .override_mode
    }

    pub fn set_mode_override(&self, mode: Option<SandboxMode>) {
        let effective = {
            let mut modes = self.modes.lock().expect("sandbox modes lock poisoned");
            modes.override_mode = mode;
            modes.override_mode.unwrap_or(modes.configured)
        };
        let mut state = self.state.write().expect("sandbox state lock poisoned");
        state.ensure_probe(effective);
    }

    pub fn update_settings_mode(&self, mode: SandboxMode) {
        {
            let mut modes = self.modes.lock().expect("sandbox modes lock poisoned");
            modes.configured = mode;
            modes.override_mode = None;
        }
        let mut state = self.state.write().expect("sandbox state lock poisoned");
        state.ensure_probe(mode);
    }

    pub fn for_cwd(&self, cwd: &Path) -> Arc<Self> {
        let state = self.state.read().expect("sandbox state lock poisoned");
        Arc::new(Self {
            state: RwLock::new(SandboxState {
                settings: state.settings.clone(),
                overlay: state.overlay.clone(),
                cwd: guard::normalize_path(cwd),
                home: state.home.clone(),
                tmpdir: state.tmpdir.clone(),
                xdg_cache_home: state.xdg_cache_home.clone(),
                xdg_runtime_dir: state.xdg_runtime_dir.clone(),
                probe: state.probe.clone(),
                cached_workspace_write: None,
                cached_read_only: None,
            }),
            modes: Arc::clone(&self.modes),
        })
    }

    pub fn view(&self, mode: Option<SandboxMode>) -> SandboxView<'_> {
        let modes = self.modes.lock().expect("sandbox modes lock poisoned");
        let mode = modes.override_mode.or(mode).unwrap_or(modes.configured);
        SandboxView {
            sandbox: self,
            mode,
        }
    }

    pub fn status(&self) -> SandboxStatus {
        self.view(None).status()
    }

    fn probe_for_mode(&self, mode: SandboxMode) -> ProbeResult {
        if mode == SandboxMode::Off {
            return ProbeResult::Available;
        }
        {
            let state = self.state.read().expect("sandbox state lock poisoned");
            if let Some(probe) = &state.probe {
                return probe.clone();
            }
        }
        let mut state = self.state.write().expect("sandbox state lock poisoned");
        let network = state.effective_network();
        state
            .probe
            .get_or_insert_with(|| platform_probe(network))
            .clone()
    }

    fn status_for_mode(&self, configured: SandboxMode) -> SandboxStatus {
        let probe = self.probe_for_mode(configured);
        let network = {
            let state = self.state.read().expect("sandbox state lock poisoned");
            state.effective_network()
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
                        allow_write_files: &policy.allow_write_files,
                        protect: &policy.protect,
                        immutable_protect: &policy.immutable_protect,
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
        self.view(None).policy()
    }

    fn policy_for_mode(&self, configured: SandboxMode) -> Result<Option<Arc<Policy>>> {
        let (effective, reason) = resolve_effective(configured, &self.probe_for_mode(configured));
        if effective == EffectiveMode::Off {
            if matches!(
                configured,
                SandboxMode::WorkspaceWrite | SandboxMode::ReadOnly
            ) {
                anyhow::bail!(
                    "sandbox mode '{}' is enabled but unavailable: {}",
                    configured.as_str(),
                    reason.as_deref().unwrap_or("unsupported platform"),
                );
            }
            return Ok(None);
        }
        Ok(Some(self.policy_for_effective(effective)))
    }

    pub fn check_read(&self, path: &Path) -> io::Result<()> {
        self.view(None).check_read(path)
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

/// 每次调用解析一次配置，后续检查沿用同一个沙盒模式。
#[derive(Clone, Copy)]
pub struct SandboxView<'a> {
    sandbox: &'a Sandbox,
    mode: SandboxMode,
}

impl SandboxView<'_> {
    pub fn policy(self) -> Result<Option<Arc<Policy>>> {
        self.sandbox.policy_for_mode(self.mode)
    }

    pub fn status(self) -> SandboxStatus {
        self.sandbox.status_for_mode(self.mode)
    }

    pub fn check_read(self, path: &Path) -> io::Result<()> {
        if let Some(policy) = self
            .policy()
            .map_err(|err| io::Error::new(io::ErrorKind::PermissionDenied, err.to_string()))?
        {
            policy.check_read(path)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
