use std::{
    ffi::OsString,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Result;

#[cfg(target_os = "linux")]
use crate::bwrap;
#[cfg(target_os = "macos")]
use crate::seatbelt;
use crate::{EffectiveMode, SandboxState, guard, platform_backend};

/// 来自 `permission.toml` 激活 profile 的文件系统与网络叠加配置。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxOverlay {
    pub writable: Vec<PathBuf>,
    pub read_only: Vec<PathBuf>,
    pub deny_read: Vec<PathBuf>,
    pub network: Option<bool>,
}

/// 写访问判定分类：区分硬不可绕过的系统/凭据路径与可经审批放行的受保护/工作区外路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteAccessCheck {
    Allowed,
    ImmutableDenied(String),
    NeedsProtectedApproval(PathBuf),
    NeedsOutsideApproval(PathBuf),
    NeedsReadOnlyApproval(PathBuf),
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
    pub(crate) mode: EffectiveMode,
    pub(crate) network: bool,
    pub(crate) cwd: PathBuf,
    pub(crate) writable: Vec<PathBuf>,
    pub(crate) allow_write_files: Vec<PathBuf>,
    pub(crate) protect: Vec<PathBuf>,
    pub(crate) immutable_protect: Vec<PathBuf>,
    pub(crate) deny_read: Vec<PathBuf>,
    /// 词法快路径的“可能命中”集合：拒绝根本身加系统别名（如 macOS 的
    /// `/tmp` 与 `/private/tmp`），避免未规范化的别名被误判为不相交。
    pub(crate) deny_read_lexical: Vec<PathBuf>,
    pub(crate) allow_read: Vec<PathBuf>,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) docker_user_sock: Option<PathBuf>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) docker_sockets: Vec<PathBuf>,
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

    /// 为单次运行（如 `read-only` 下的本次 scratch 目录）附加额外可写路径。
    pub fn with_extra_writable(&self, path: &Path) -> Arc<Self> {
        let mut next = self.clone();
        let normalized = guard::normalize_path(path);
        if !next.writable.iter().any(|existing| existing == &normalized) {
            next.writable.push(normalized);
        }
        Arc::new(next)
    }

    /// 为单次工具调用附加已批准的精确作用域（额外可写目录、单个放行文件、受保护路径豁免与网络）。
    /// 硬不可豁免路径（`permission.toml`、`deny_read` 凭据路径）绝不在此放行。
    pub fn with_approved_scope(
        &self,
        writable: &[PathBuf],
        allow_files: &[PathBuf],
        unprotect: &[PathBuf],
        network: bool,
    ) -> Result<Arc<Self>> {
        let mut next = self.clone();
        for dir in writable {
            let norm = guard::normalize_path(dir);
            anyhow::ensure!(
                !next.is_immutable_path(&norm),
                "approved directory overlaps an immutable protected or credential path"
            );
            if !next.writable.iter().any(|existing| existing == &norm) {
                next.writable.push(norm);
            }
        }
        for file in allow_files {
            let norm = guard::normalize_path(file);
            anyhow::ensure!(
                !next.is_immutable_path(&norm),
                "approved file overlaps an immutable protected or credential path"
            );
            if !next
                .allow_write_files
                .iter()
                .any(|existing| existing == &norm)
            {
                next.allow_write_files.push(norm);
            }
        }
        for root in unprotect {
            let norm = guard::normalize_path(root);
            anyhow::ensure!(
                !next.is_immutable_path(&norm),
                "cannot unprotect an immutable protected or credential path"
            );
            next.protect
                .retain(|existing| !guard::paths_overlap(existing, &norm));
            if !next.writable.iter().any(|existing| existing == &norm) {
                next.writable.push(norm);
            }
        }
        next.network |= network;
        Ok(Arc::new(next))
    }

    fn is_immutable_path(&self, normalized: &Path) -> bool {
        self.immutable_protect
            .iter()
            .any(|prot| guard::paths_overlap(normalized, prot))
            || self
                .deny_read
                .iter()
                .filter(|d| {
                    !self
                        .allow_read
                        .iter()
                        .any(|a| guard::path_starts_with(d, a))
                })
                .any(|denied| guard::paths_overlap(normalized, denied))
    }

    /// 从当前命令的策略快照派生更窄的、显式批准过的额外权限。
    pub fn resolve_extra_writable(&self, paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
        let mut resolved = Vec::with_capacity(paths.len());
        for path in paths {
            anyhow::ensure!(
                path.is_absolute(),
                "extra writable directory must be absolute"
            );
            let directory = std::fs::canonicalize(path)
                .map_err(|_| anyhow::anyhow!("cannot resolve extra writable directory"))?;
            anyhow::ensure!(
                directory.is_dir(),
                "extra writable path must be a directory"
            );
            anyhow::ensure!(
                !self
                    .protect
                    .iter()
                    .chain(&self.immutable_protect)
                    .chain(&self.deny_read)
                    .any(|protected| guard::paths_overlap(&directory, protected)),
                "extra writable directory overlaps a protected path"
            );
            if self.check_write(&directory).is_err() {
                resolved.push(directory);
            }
        }
        resolved.sort_unstable();
        resolved.dedup();
        Ok(resolved)
    }

    pub fn with_command_grant(&self, writable: &[PathBuf], network: bool) -> Result<Arc<Self>> {
        anyhow::ensure!(
            self.resolve_extra_writable(writable)? == writable,
            "extra writable directories changed during permission check"
        );
        let mut next = self.clone();
        next.writable.extend_from_slice(writable);
        next.network |= network;
        Ok(Arc::new(next))
    }

    pub fn check_read(&self, path: &Path) -> io::Result<()> {
        let abs = self.absolutize(path);
        guard::check_read_path(&abs, &self.deny_read, &self.allow_read)
    }

    /// 遍历剪枝判定：绝对且与拒绝根不相交的路径直接放行，其余情况规范化后判定。
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

    pub fn can_contain_denied_read(&self, canonical_root: &Path) -> bool {
        self.deny_read_lexical
            .iter()
            .any(|denied| guard::paths_overlap(canonical_root, denied))
    }

    pub fn is_read_denied_resolved(&self, path: &Path) -> bool {
        let abs = self.absolutize(path);
        guard::is_read_denied_path(&abs, &self.deny_read, &self.allow_read)
    }

    pub fn check_write(&self, path: &Path) -> io::Result<()> {
        let abs = self.absolutize(path);
        guard::check_write_path(
            &abs,
            self.mode,
            &self.cwd,
            &self.writable,
            &self.allow_write_files,
            &self.protect,
            &self.immutable_protect,
            &self.deny_read,
            &self.allow_read,
        )
    }

    /// 精确分类写路径被沙盒拦截的原因，供权限门决定硬拒绝还是发起审批。
    pub fn classify_write(&self, path: &Path) -> WriteAccessCheck {
        let abs = self.absolutize(path);
        if guard::is_safe_device_write(&abs) {
            return WriteAccessCheck::Allowed;
        }
        if self
            .immutable_protect
            .iter()
            .any(|prot| guard::path_starts_with(&abs, prot))
        {
            return WriteAccessCheck::ImmutableDenied(format!(
                "blocked by sandbox policy (immutable protected path is read-only): {}",
                abs.display()
            ));
        }
        if guard::is_read_denied_path(&abs, &self.deny_read, &self.allow_read) {
            return WriteAccessCheck::ImmutableDenied(format!(
                "blocked by sandbox policy (credential or protected path write denied): {}",
                abs.display()
            ));
        }
        if self
            .allow_write_files
            .iter()
            .any(|allowed| guard::path_eq_platform(&abs, allowed))
        {
            return WriteAccessCheck::Allowed;
        }
        if let Some(prot) = self
            .protect
            .iter()
            .find(|prot| guard::path_starts_with(&abs, prot))
        {
            return WriteAccessCheck::NeedsProtectedApproval(prot.clone());
        }
        if self.mode == EffectiveMode::ReadOnly && guard::path_starts_with(&abs, &self.cwd) {
            if self.writable.iter().any(|root| {
                guard::path_starts_with(root, &self.cwd)
                    && !guard::path_eq_platform(root, &self.cwd)
                    && guard::path_starts_with(&abs, root)
            }) {
                return WriteAccessCheck::Allowed;
            }
            return WriteAccessCheck::NeedsReadOnlyApproval(abs);
        }
        if guard::is_macos_var_folder_temp(&abs)
            || self
                .writable
                .iter()
                .any(|root| guard::path_starts_with(&abs, root))
        {
            return WriteAccessCheck::Allowed;
        }
        WriteAccessCheck::NeedsOutsideApproval(abs)
    }

    pub fn command_wrapper_prefix(&self) -> Result<(String, Vec<OsString>)> {
        #[cfg(target_os = "macos")]
        {
            let spec = seatbelt::SeatbeltSpec {
                mode: self.mode,
                cwd: &self.cwd,
                writable: &self.writable,
                allow_write_files: &self.allow_write_files,
                protect: &self.protect,
                immutable_protect: &self.immutable_protect,
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
                allow_write_files: &self.allow_write_files,
                protect: &self.protect,
                immutable_protect: &self.immutable_protect,
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
                &self.allow_write_files,
                &self.immutable_protect,
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
        let lower = output.to_ascii_lowercase();
        lower.contains("blocked by sandbox policy")
            || lower.contains("operation not permitted")
            || if cfg!(target_os = "linux") {
                lower.contains("permission denied") || lower.contains("read-only file system")
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
            "Command hit the Linux bubblewrap sandbox (Permission denied / Read-only file system). Explain the restriction to the user or request permission."
        } else {
            "Command hit the macOS sandbox (Operation not permitted). Explain the restriction to the user or request permission."
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

/// 若工作区 `.git` 是 git worktree/submodule 的指针文件（`gitdir: ...`），
/// 解析并返回规范化的外部 gitdir 路径以纳入只读保护。
fn resolve_worktree_gitdir(cwd: &Path) -> Option<PathBuf> {
    let dot_git = cwd.join(".git");
    let meta = std::fs::symlink_metadata(&dot_git).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    let file = std::fs::File::open(&dot_git).ok()?;
    let mut buf = String::new();
    file.take(4096).read_to_string(&mut buf).ok()?;
    let raw = buf
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))?
        .trim();
    if raw.is_empty() {
        return None;
    }
    let candidate = Path::new(raw);
    let abs = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        cwd.join(candidate)
    };
    let normalized = guard::normalize_path(&abs);
    if guard::path_eq_platform(&normalized, cwd) {
        return None;
    }
    Some(normalized)
}

pub(crate) fn build_policy(state: &SandboxState, mode: EffectiveMode) -> Policy {
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
        for path in &state.overlay.writable {
            push_unique(&mut writable, guard::normalize_path(path));
        }
    }

    let mut protect = Vec::new();
    for rel in [".git", ".agents", ".pp", ".mcp.json", ".claude/.mcp.json"] {
        push_unique(&mut protect, guard::normalize_path(&state.cwd.join(rel)));
    }
    if let Some(gitdir) = resolve_worktree_gitdir(&state.cwd) {
        push_unique(&mut protect, gitdir);
    }
    for rel in &state.settings.protect {
        push_unique(&mut protect, guard::normalize_path(&state.cwd.join(rel)));
    }
    for path in &state.overlay.read_only {
        push_unique(&mut protect, guard::normalize_path(path));
    }
    // permission.toml 中显式把某个受保护路径（如 `.git` 或 `.agents`）设为 `"write"` 时，解除默认只读保护。
    if mode == EffectiveMode::WorkspaceWrite {
        protect.retain(|prot| {
            !state
                .overlay
                .writable
                .iter()
                .any(|w| guard::path_starts_with(prot, &guard::normalize_path(w)))
        });
    }

    let mut immutable_protect = Vec::new();
    if let Some(home) = home {
        for rel in [".pp/agent/permission.toml", ".pp/agent/permission.lock"] {
            push_unique(
                &mut immutable_protect,
                guard::normalize_path(&home.join(rel)),
            );
        }
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
    for path in &state.overlay.deny_read {
        push_unique(&mut deny_read, guard::normalize_path(path));
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

    let mut deny_read_lexical = deny_read.clone();
    for root in &deny_read {
        for alias in guard::lexical_aliases(root) {
            if !deny_read_lexical.contains(&alias) {
                deny_read_lexical.push(alias);
            }
        }
    }

    let network = state.overlay.network.unwrap_or(state.settings.network);

    Policy {
        mode,
        network,
        cwd: state.cwd.clone(),
        writable,
        allow_write_files: Vec::new(),
        protect,
        immutable_protect,
        deny_read,
        deny_read_lexical,
        allow_read,
        docker_user_sock,
        docker_sockets,
    }
}
