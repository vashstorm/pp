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
use pp_config::config::SandboxReadScope;

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

/// 读访问判定分类：区分硬不可绕过的凭据路径、仅限单次人工确认的会话路径与可审批放行的工作区外路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadAccessCheck {
    Allowed,
    ImmutableDenied(String),
    NeedsSessionApproval(PathBuf),
    NeedsOutsideApproval(PathBuf),
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
    pub(crate) read_scope: SandboxReadScope,
    pub(crate) cwd: PathBuf,
    pub(crate) writable: Vec<PathBuf>,
    pub(crate) allow_write_files: Vec<PathBuf>,
    pub(crate) protect: Vec<PathBuf>,
    pub(crate) immutable_protect: Vec<PathBuf>,
    pub(crate) home_deny_root: Option<PathBuf>,
    pub(crate) home_deny_root_lexical: Vec<PathBuf>,
    pub(crate) home_allow_read: Vec<PathBuf>,
    pub(crate) deny_read: Vec<PathBuf>,
    /// 词法快路径的“可能命中”集合：拒绝根本身加系统别名（如 macOS 的
    /// `/tmp` 与 `/private/tmp`），避免未规范化的别名被误判为不相交。
    pub(crate) deny_read_lexical: Vec<PathBuf>,
    pub(crate) allow_read: Vec<PathBuf>,
    session_read_root: Option<PathBuf>,
    #[cfg(target_os = "macos")]
    session_read_parent_metadata: Vec<PathBuf>,
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

    pub fn read_scope(&self) -> SandboxReadScope {
        self.read_scope
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// 供 `DangerFullAccess`（Full Access / YOLO）下的内置只读工具解除 `$HOME` 外读限制，
    /// 同时完整保留 `deny_read` 凭据黑名单。
    pub fn without_home_restriction(&self) -> Arc<Self> {
        let mut next = self.clone();
        next.read_scope = SandboxReadScope::Unrestricted;
        next.home_deny_root = None;
        next.home_deny_root_lexical.clear();
        next.home_allow_read.clear();
        Arc::new(next)
    }

    /// 计算策略安全生效字段的稳定指纹，供持久内核（如 `pp-eval`）检测策略变更并重启。
    pub fn fingerprint(&self) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325_u64;
        let mix_bytes = |h: &mut u64, bytes: &[u8]| {
            for &b in bytes {
                *h ^= u64::from(b);
                *h = h.wrapping_mul(0x0100_0000_01b3);
            }
            *h ^= 0xff;
            *h = h.wrapping_mul(0x0100_0000_01b3);
        };
        let mix_path = |h: &mut u64, p: &Path| {
            use std::os::unix::ffi::OsStrExt;
            mix_bytes(h, p.as_os_str().as_bytes());
        };
        let mix_slice = |h: &mut u64, paths: &[PathBuf]| {
            mix_bytes(h, &paths.len().to_le_bytes());
            for p in paths {
                mix_path(h, p);
            }
        };
        mix_bytes(&mut h, self.mode.as_str().as_bytes());
        mix_bytes(&mut h, &[u8::from(self.network)]);
        mix_bytes(&mut h, self.read_scope.as_str().as_bytes());
        mix_path(&mut h, &self.cwd);
        mix_slice(&mut h, &self.writable);
        mix_slice(&mut h, &self.allow_write_files);
        mix_slice(&mut h, &self.protect);
        mix_slice(&mut h, &self.immutable_protect);
        if let Some(home) = &self.home_deny_root {
            mix_bytes(&mut h, &[1]);
            mix_path(&mut h, home);
        } else {
            mix_bytes(&mut h, &[0]);
        }
        mix_slice(&mut h, &self.home_allow_read);
        mix_slice(&mut h, &self.deny_read);
        mix_slice(&mut h, &self.allow_read);
        h
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
        guard::check_read_path(
            &abs,
            &self.deny_read,
            &self.allow_read,
            self.home_deny_root.as_deref(),
            &self.home_allow_read,
        )
    }

    /// 精确分类读路径的沙盒权限：区分凭据硬拒绝、会话单次审批与工作区外 `$HOME` 读审批。
    pub fn classify_read(&self, path: &Path) -> ReadAccessCheck {
        let abs = self.absolutize(path);
        if self
            .allow_read
            .iter()
            .any(|allowed| guard::path_starts_with(&abs, allowed))
        {
            return ReadAccessCheck::Allowed;
        }
        if self
            .deny_read
            .iter()
            .any(|denied| guard::path_starts_with(&abs, denied))
        {
            if let Some(target) = self.session_read_target(&abs) {
                return ReadAccessCheck::NeedsSessionApproval(target);
            }
            return ReadAccessCheck::ImmutableDenied(format!(
                "blocked by sandbox policy (credential or protected path read denied): {}",
                abs.display()
            ));
        }
        if let Some(home) = self.home_deny_root.as_deref()
            && guard::path_starts_with(&abs, home)
            && !self
                .home_allow_read
                .iter()
                .any(|allowed| guard::path_starts_with(&abs, allowed))
        {
            return ReadAccessCheck::NeedsOutsideApproval(abs);
        }
        ReadAccessCheck::Allowed
    }

    /// 解析 `bash` 的 `sandbox_permissions.read` 申请列表，返回待审批的 `(session_reads, outside_reads)`。
    pub fn resolve_extra_readable(
        &self,
        paths: &[PathBuf],
    ) -> Result<(Vec<PathBuf>, Vec<PathBuf>)> {
        let mut session_reads = Vec::new();
        let mut outside_reads = Vec::new();
        for path in paths {
            anyhow::ensure!(path.is_absolute(), "extra readable path must be absolute");
            let resolved = std::fs::canonicalize(path)
                .map_err(|_| anyhow::anyhow!("cannot resolve extra readable path"))?;
            anyhow::ensure!(
                resolved != Path::new("/"),
                "cannot request read access to filesystem root '/'"
            );
            match self.classify_read(&resolved) {
                ReadAccessCheck::Allowed => {}
                ReadAccessCheck::ImmutableDenied(msg) => anyhow::bail!("{msg}"),
                ReadAccessCheck::NeedsSessionApproval(target) => session_reads.push(target),
                ReadAccessCheck::NeedsOutsideApproval(target) => outside_reads.push(target),
            }
        }
        session_reads.sort_unstable();
        session_reads.dedup();
        outside_reads.sort_unstable();
        outside_reads.dedup();
        Ok((session_reads, outside_reads))
    }

    /// 只有默认的 Agent 目录拒读规则可以为具体会话路径开一次性例外。
    pub fn session_read_target(&self, path: &Path) -> Option<PathBuf> {
        let root = self.session_read_root.as_deref()?;
        if !guard::path_starts_with(path, root) {
            return None;
        }
        let target = std::fs::canonicalize(path).ok()?;
        let agent = root.parent()?;
        (guard::path_starts_with(&target, root)
            && self.is_read_denied_resolved(&target)
            && !self.deny_read.iter().any(|denied| {
                !guard::path_eq_platform(denied, agent) && guard::paths_overlap(denied, &target)
            })
            && !self
                .immutable_protect
                .iter()
                .any(|protected| guard::paths_overlap(protected, &target)))
        .then_some(target)
    }

    /// 将批准的读取范围（会话路径与工作区外只读路径）加到当次策略，并继续禁止写入与保护凭据。
    pub fn with_approved_reads(
        &self,
        session_reads: &[PathBuf],
        outside_reads: &[PathBuf],
    ) -> Result<Arc<Self>> {
        let mut next = self.clone();
        for path in session_reads {
            anyhow::ensure!(
                self.session_read_target(path).as_ref() == Some(path),
                "approved session path changed during permission check"
            );
            next.allow_read.push(path.clone());
            next.immutable_protect.push(path.clone());
            #[cfg(target_os = "macos")]
            if path.is_dir()
                && let Some(parent) = path.parent()
                && self.is_read_denied_resolved(parent)
                && !next
                    .session_read_parent_metadata
                    .iter()
                    .any(|p| p == parent)
            {
                next.session_read_parent_metadata.push(parent.to_path_buf());
            }
        }
        for path in outside_reads {
            let abs = self.absolutize(path);
            match self.classify_read(&abs) {
                ReadAccessCheck::ImmutableDenied(msg) => anyhow::bail!("{msg}"),
                ReadAccessCheck::NeedsSessionApproval(_) => {
                    anyhow::bail!("session path must use dedicated session read approval")
                }
                ReadAccessCheck::Allowed | ReadAccessCheck::NeedsOutsideApproval(_) => {
                    if !next
                        .home_allow_read
                        .iter()
                        .any(|existing| guard::path_eq_platform(existing, &abs))
                    {
                        next.home_allow_read.push(abs);
                    }
                }
            }
        }
        Ok(Arc::new(next))
    }

    /// 将批准的读取范围加到当次策略，并继续禁止写入这些路径。
    pub fn with_approved_session_reads(&self, paths: &[PathBuf]) -> Result<Arc<Self>> {
        self.with_approved_reads(paths, &[])
    }

    /// 遍历剪枝判定：绝对且与拒绝根不相交（或位于已允许的工作区边界内）的路径直接放行，
    /// 其余情况规范化后判定。
    pub fn is_read_denied(&self, path: &Path) -> bool {
        let joined = self.join_cwd(path);
        if guard::lexically_normal_abs(&joined)
            && !self
                .deny_read_lexical
                .iter()
                .any(|denied| guard::paths_overlap(&joined, denied))
        {
            let in_home_boundary = self
                .home_deny_root_lexical
                .iter()
                .any(|home| guard::paths_overlap(&joined, home));
            if !in_home_boundary
                || self
                    .home_allow_read
                    .iter()
                    .any(|allowed| guard::path_starts_with(&joined, allowed))
            {
                return false;
            }
        }
        guard::is_read_denied_path(
            &guard::normalize_path(&joined),
            &self.deny_read,
            &self.allow_read,
            self.home_deny_root.as_deref(),
            &self.home_allow_read,
        )
    }

    pub fn can_contain_denied_read(&self, canonical_root: &Path) -> bool {
        if self
            .deny_read_lexical
            .iter()
            .any(|denied| guard::paths_overlap(canonical_root, denied))
        {
            return true;
        }
        self.home_deny_root_lexical
            .iter()
            .any(|home| guard::paths_overlap(canonical_root, home))
            && !self
                .home_allow_read
                .iter()
                .any(|allowed| guard::path_starts_with(canonical_root, allowed))
    }

    pub fn is_read_denied_resolved(&self, path: &Path) -> bool {
        let abs = self.absolutize(path);
        guard::is_read_denied_path(
            &abs,
            &self.deny_read,
            &self.allow_read,
            self.home_deny_root.as_deref(),
            &self.home_allow_read,
        )
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
        if guard::is_read_denied_path(&abs, &self.deny_read, &self.allow_read, None, &[]) {
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
                home_deny_root: self.home_deny_root.as_ref(),
                home_allow_read: &self.home_allow_read,
                deny_read: &self.deny_read,
                allow_read: &self.allow_read,
                allow_read_metadata: &self.session_read_parent_metadata,
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
                home_deny_root: self.home_deny_root.as_ref(),
                home_allow_read: &self.home_allow_read,
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
            "Command hit the Linux bubblewrap sandbox (Permission denied / Read-only file system). Explain the restriction to the user or request permission (use read/rg/glob or bash sandbox_permissions.read for out-of-workspace reads)."
        } else {
            "Command hit the macOS sandbox (Operation not permitted). Explain the restriction to the user or request permission (use read/rg/glob or bash sandbox_permissions.read for out-of-workspace reads)."
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
            ".cargo/credentials",
            ".cargo/credentials.toml",
            ".config/git/credentials",
            ".pgpass",
            ".my.cnf",
            ".s3cfg",
            ".vault-token",
            ".terraform.d/credentials.tfrc.json",
            ".gem/credentials",
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
    let mut session_read_root = home.map(|home| home.join(".pp/agent/sessions"));
    let mut add_explicit_read_deny = |path: &Path| {
        let path = guard::normalize_path(path);
        if session_read_root
            .as_ref()
            .is_some_and(|root| guard::path_starts_with(root, &path))
        {
            session_read_root = None;
        }
        push_unique(&mut deny_read, path);
    };
    for raw in &state.settings.deny_read {
        if let Some(path) = guard::expand_tilde(raw, home) {
            add_explicit_read_deny(&path);
        }
    }
    for path in &state.overlay.deny_read {
        add_explicit_read_deny(path);
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

    let read_scope = state.settings.read_scope;
    let home_deny_root = (read_scope == SandboxReadScope::Workspace)
        .then_some(home)
        .flatten()
        .filter(|home| !guard::path_starts_with(home, &state.cwd))
        .map(Path::to_path_buf);
    let mut home_deny_root_lexical = Vec::new();
    let mut home_allow_read = Vec::new();
    if let Some(home) = home_deny_root.as_deref() {
        push_unique(&mut home_deny_root_lexical, home.to_path_buf());
        for alias in guard::lexical_aliases(home) {
            push_unique(&mut home_deny_root_lexical, alias);
        }
        // 仅收录严格位于 $HOME 之内的路径；像 TMPDIR 这样包裹测试 $HOME 的祖先目录
        // 不得进入 home_allow_read，避免把整棵 $HOME 意外放开。
        let mut push_home_allow = |path: PathBuf| {
            if guard::path_starts_with(&path, home) && !guard::path_eq_platform(&path, home) {
                push_unique(&mut home_allow_read, path);
            }
        };
        push_home_allow(state.cwd.clone());
        if let Some(gitdir) = resolve_worktree_gitdir(&state.cwd) {
            push_home_allow(gitdir);
        }
        for path in &writable {
            push_home_allow(path.clone());
        }
        for path in &state.overlay.read_only {
            push_home_allow(guard::normalize_path(path));
        }
        for raw in &state.settings.allow_read {
            if let Some(path) = guard::expand_tilde(raw, Some(home)) {
                push_home_allow(guard::normalize_path(&path));
            }
        }
        for rel in [
            ".rustup",
            ".cargo/bin",
            ".cargo/config",
            ".cargo/config.toml",
            ".pyenv",
            ".nvm",
            ".fnm",
            ".volta",
            ".asdf",
            ".mise",
            ".local/bin",
            ".local/share/mise",
            ".local/share/uv",
            ".local/share/virtualenvs",
            ".rbenv",
            ".sdkman",
            ".go",
            "go/bin",
            ".bun/bin",
            ".deno",
            ".conda",
            "miniconda3",
            "anaconda3",
            ".venv",
            ".virtualenvs",
            ".gitconfig",
            ".config/git/ignore",
            ".config/git/attributes",
        ] {
            push_unique(&mut home_allow_read, guard::normalize_path(&home.join(rel)));
        }
        #[cfg(target_os = "macos")]
        for rel in ["Library/Python", "Library/Developer", "Library/Frameworks"] {
            push_unique(&mut home_allow_read, guard::normalize_path(&home.join(rel)));
        }
    }

    let network = state.overlay.network.unwrap_or(state.settings.network);

    Policy {
        mode,
        network,
        read_scope,
        cwd: state.cwd.clone(),
        writable,
        allow_write_files: Vec::new(),
        protect,
        immutable_protect,
        home_deny_root,
        home_deny_root_lexical,
        home_allow_read,
        deny_read,
        deny_read_lexical,
        allow_read,
        session_read_root,
        #[cfg(target_os = "macos")]
        session_read_parent_metadata: Vec::new(),
        docker_user_sock,
        docker_sockets,
    }
}
