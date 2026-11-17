//! LSP 支持：服务器发现、受管理 stdio 客户端、`lsp` 工具、保存联动与诊断投递。
//!
//! 上层保持同步：每个语言服务器有自己的 I/O 线程，Agent/TUI 不引入 Tokio。
//! LSP 仅提供信息与修改建议，文件修改交给普通工具；服务器进程本身没有文件系统沙箱。

use pp_config::config::LspSettings;

use anyhow::{Context, Result, bail, ensure};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

pub(crate) mod adapters;
pub(crate) mod client;
pub(crate) mod config;
pub mod diagnostics;
pub(crate) mod edits;
pub(crate) mod format_options;
pub(crate) mod position;
pub mod tool;
pub(crate) mod transport;
pub(crate) mod types;
pub mod uri;
pub mod writethrough;

#[cfg(test)]
mod tests;

pub(crate) use client::Client;

/// 确定性初始化失败缓存时长。
const NEGATIVE_CACHE: Duration = Duration::from_secs(180);
const LOCK_POLL: Duration = Duration::from_millis(20);

#[derive(Clone, Debug)]
pub(crate) enum ClientState {
    NotStarted,
    Initializing,
    Ready,
    Failed(String),
}

/// 一个工作区的客户端注册表；可 clone 的句柄共享同一批受管理进程。
#[derive(Clone)]
pub struct Registry {
    inner: Arc<Inner>,
}

struct Inner {
    cwd: PathBuf,
    /// 用户级 `lsp.json` 的实际路径；由 Agent 从配置目录注入，测试不读真实 HOME。
    config_path: Option<PathBuf>,
    settings: LspSettings,
    /// 惰性配置与发现快照：`None` 表示还没有任何查询。首次查询在锁内装载
    /// 用户/项目 `lsp.json` 并做本地发现，之后复用同一快照，只有 `reload`
    /// 才会重新读盘。
    config: Mutex<Option<Result<Arc<config::Config>, String>>>,
    /// 保存联动与迟到诊断的受管协调器；生命周期跟随本 Registry。
    diagnostics: writethrough::Coordinator,
    /// 身份 → 客户端：同一工作区内同身份（无论配置里用哪个名字）共享一个进程。
    clients: Mutex<HashMap<String, Arc<Client>>>,
    /// 身份 → 包括初始化中的进程；只有确认回收后才能移除。
    slots: Mutex<HashMap<String, Arc<Mutex<transport::ProcessSlot>>>>,
    /// 当前配置的服务器名 → 身份；reload 时随配置一起替换。
    names: Mutex<HashMap<String, String>>,
    lifecycle: Mutex<()>,
    maintenance_stop: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    statuses: Mutex<HashMap<String, String>>,
    /// 身份 → 启动锁；并发同身份（含别名）只启动一次。
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    negative: Mutex<HashMap<String, (String, Instant)>>,
    closing: AtomicBool,
    generation: AtomicU64,
}

impl Inner {
    /// 读取用户/项目 `lsp.json`、合并内置定义并做本地发现。
    ///
    /// 首次查询或 `reload` 才执行；错误保持脱敏后的说明。
    fn load_config(&self) -> Result<Arc<config::Config>, String> {
        if !self.settings.enabled {
            return Ok(Arc::new(config::Config {
                servers: Vec::new(),
                definitions: Vec::new(),
                idle_timeout_ms: None,
            }));
        }
        config::Config::load_with(&self.cwd, self.config_path.as_deref())
            .map(Arc::new)
            .map_err(|error| format!("{error:#}"))
    }
}

impl Registry {
    /// `user_config` 是用户级 `lsp.json` 的完整路径；由 Agent 依据配置目录注入，
    /// 不能在 Registry 内部再次读取 HOME。
    ///
    /// 构造只保留发现输入（cwd、用户配置路径与 settings）：`lsp.json` 的读取、
    /// 合并与本地发现推迟到首次查询，结果按这三项缓存到 `reload` 为止。
    pub fn new(cwd: PathBuf, user_config: Option<PathBuf>, settings: LspSettings) -> Self {
        Self::assemble(cwd, user_config, settings, None)
    }

    /// 测试入口：显式提供已经解析好的配置，且不读取任何用户级文件。
    #[cfg(test)]
    fn from_config(
        cwd: PathBuf,
        settings: LspSettings,
        config: Result<Arc<config::Config>, String>,
    ) -> Self {
        Self::assemble(cwd, None, settings, Some(config))
    }

    fn assemble(
        cwd: PathBuf,
        config_path: Option<PathBuf>,
        settings: LspSettings,
        config: Option<Result<Arc<config::Config>, String>>,
    ) -> Self {
        let names = match &config {
            Some(Ok(config)) => alias_index(config, &cwd),
            _ => HashMap::new(),
        };
        let registry = Self {
            inner: Arc::new(Inner {
                cwd,
                config_path,
                settings,
                config: Mutex::new(config),
                diagnostics: writethrough::Coordinator::default(),
                clients: Mutex::new(HashMap::new()),
                slots: Mutex::new(HashMap::new()),
                names: Mutex::new(names),
                lifecycle: Mutex::new(()),
                maintenance_stop: Mutex::new(None),
                statuses: Mutex::new(HashMap::new()),
                locks: Mutex::new(HashMap::new()),
                negative: Mutex::new(HashMap::new()),
                closing: AtomicBool::new(false),
                generation: AtomicU64::new(1),
            }),
        };
        if settings.enabled {
            let (stop, receiver) = std::sync::mpsc::channel();
            let weak = Arc::downgrade(&registry.inner);
            match std::thread::Builder::new()
                .name("pp-lsp-maintenance".into())
                .spawn(move || {
                    while matches!(
                        receiver.recv_timeout(Duration::from_secs(1)),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                    ) {
                        let Some(inner) = weak.upgrade() else { break };
                        let registry = Registry { inner };
                        if registry.inner.closing.load(Ordering::SeqCst) {
                            break;
                        }
                        if let Err(error) = registry.idle_sweep() {
                            tracing::error!(event = "lsp_idle_cleanup", error = %error);
                        }
                    }
                }) {
                Ok(_) => {
                    *registry
                        .inner
                        .maintenance_stop
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(stop)
                }
                Err(_) => {
                    *registry
                        .inner
                        .config
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) =
                        Some(Err("LSP maintenance thread could not be started".into()))
                }
            }
        }
        registry
    }

    pub fn settings(&self) -> LspSettings {
        self.inner.settings
    }

    /// 保存联动与迟到诊断协调器；生命周期与客户端进程一致。
    pub fn diagnostics(&self) -> &writethrough::Coordinator {
        &self.inner.diagnostics
    }

    pub fn cwd(&self) -> &Path {
        &self.inner.cwd
    }

    pub fn enabled(&self) -> bool {
        self.inner.settings.enabled
    }

    pub fn is_lazy(&self) -> bool {
        self.inner.settings.lazy
    }

    /// reload 成功后清除该服务器名对应身份的确定性初始化失败缓存。
    ///
    /// 只影响负缓存，不启动或停止任何进程；返回是否确实移除了一条记录。
    pub(crate) fn clear_failure(&self, name: &str) -> bool {
        let identity = self
            .inner
            .names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned();
        let Some(identity) = identity else {
            return false;
        };
        self.inner
            .negative
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&identity)
            .is_some()
    }

    /// 当前配置；首次调用装载并做一次本地发现，之后复用同一快照。
    /// 配置错误时返回同一个脱敏说明。
    pub(crate) fn config(&self) -> Result<Arc<config::Config>> {
        self.cached_config()
            .map_err(|message| anyhow::anyhow!("{message}"))
    }

    pub(crate) fn config_error(&self) -> Option<String> {
        self.cached_config().err()
    }

    /// 惰性配置槽：只有首次查询与 `reload` 会读盘，并发查询共用一次装载。
    ///
    /// 装载成功时同步重建服务器名 → 身份的索引，使 `ensure_client` 不再依赖
    /// 构造期解析。
    fn cached_config(&self) -> Result<Arc<config::Config>, String> {
        let mut slot = self.inner.config.lock().unwrap_or_else(|e| e.into_inner());
        let state = slot.get_or_insert_with(|| {
            let loaded = self.inner.load_config();
            if let Ok(config) = &loaded {
                *self.inner.names.lock().unwrap_or_else(|e| e.into_inner()) =
                    alias_index(config, &self.inner.cwd);
            }
            loaded
        });
        match state {
            Ok(config) => Ok(config.clone()),
            Err(message) => Err(message.clone()),
        }
    }

    pub(crate) fn definitions(&self) -> Vec<config::ServerDefinition> {
        self.config()
            .map(|config| config.definitions.clone())
            .unwrap_or_default()
    }

    pub(crate) fn discovered(&self) -> Vec<config::ServerDefinition> {
        self.config()
            .map(|config| config.servers.clone())
            .unwrap_or_default()
    }

    /// 后台预热所有已发现的真实 language server。
    ///
    /// 只处理 `lazy=false` 且已启用的注册表；CLI adapter（Biome/SwiftLint）
    /// 与工作区 checker 不在 `servers` 中，不会被启动。每个服务器使用自己的
    /// `warmupTimeoutMs`（缺省取 `requestTimeout`），并与正常 `ensure_client`
    /// 共用单飞初始化；单个失败只记录在返回值中，不影响其他服务器或普通读写。
    /// 返回 `(server, error)` 失败列表，空表示全部成功或无需启动。
    pub fn warmup(
        &self,
        cancellation: &pp_core::cancellation::CancellationToken,
    ) -> Vec<(String, String)> {
        if !self.inner.settings.enabled || self.inner.settings.lazy {
            return Vec::new();
        }
        let mut failures = Vec::new();
        for definition in self.discovered() {
            if crate::adapters::is_adapter(&definition.name) {
                continue;
            }
            if cancellation.is_cancelled() || self.inner.closing.load(Ordering::SeqCst) {
                break;
            }
            let budget = definition
                .warmup_timeout_ms
                .filter(|value| *value > 0)
                .map(Duration::from_millis)
                .unwrap_or_else(|| self.inner.settings.request_budget());
            // 配置中的超大毫秒值会让 Instant 溢出；退化为立即失败而不是 panic。
            let deadline = Instant::now()
                .checked_add(budget)
                .unwrap_or_else(Instant::now);
            if let Err(error) = self.ensure_client(&definition.name, deadline, cancellation) {
                failures.push((definition.name, format!("{error:#}")));
            }
        }
        failures
    }

    pub(crate) fn servers_for_file(&self, path: &Path) -> Vec<config::ServerDefinition> {
        self.config()
            .map(|config| config.servers_for_file(path).into_iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn has_servers_for_file(&self, path: &Path) -> bool {
        !self.servers_for_file(path).is_empty()
    }

    pub fn has_started(&self) -> bool {
        !self.started().is_empty()
    }

    pub fn start_client(
        &self,
        name: &str,
        deadline: Instant,
        cancellation: &pp_core::cancellation::CancellationToken,
    ) -> Result<()> {
        self.ensure_client(name, deadline, cancellation).map(|_| ())
    }

    pub fn document_snapshot(&self, name: &str, path: &Path) -> Option<(i64, String)> {
        let doc = self.client(name)?.document(path)?;
        Some((doc.version, doc.text))
    }

    pub(crate) fn real_servers_for_file(&self, path: &Path) -> Vec<config::ServerDefinition> {
        self.config()
            .map(|config| {
                config
                    .real_servers_for_file(path)
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn started(&self) -> Vec<Arc<Client>> {
        self.inner
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn client(&self, name: &str) -> Option<Arc<Client>> {
        let identity = self
            .inner
            .names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()?;
        self.client_by_identity(&identity)
    }

    fn client_by_identity(&self, identity: &str) -> Option<Arc<Client>> {
        self.inner
            .clients
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(identity)
            .cloned()
    }

    /// 当前配置里的服务器名对应的完整身份；未发现的服务器仍然是错误。
    fn identity_for(&self, name: &str) -> Result<String> {
        self.config()?;
        self.inner
            .names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
            .with_context(|| format!("LSP server `{name}` is not discovered for this workspace"))
    }

    fn lock_for(&self, identity: &str) -> Arc<Mutex<()>> {
        self.inner
            .locks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(identity.to_owned())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn register_name(&self, name: &str, identity: &str) {
        self.inner
            .names
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_owned(), identity.to_owned());
    }

    /// 只在 slot 仍是本次登记的那一个时移除，避免覆盖或误删并发登记。
    fn remove_slot_if(&self, identity: &str, process: &Arc<Mutex<transport::ProcessSlot>>) {
        let mut slots = self.inner.slots.lock().unwrap_or_else(|e| e.into_inner());
        if slots
            .get(identity)
            .is_some_and(|existing| Arc::ptr_eq(existing, process))
        {
            slots.remove(identity);
        }
    }

    /// 清除映射到该身份的所有配置名的状态，避免别名共享客户端后留下
    /// 永久的 `initializing` 或旧失败文案。
    fn clear_statuses_for_identity(&self, identity: &str) {
        let names: Vec<String> = {
            let names = self.inner.names.lock().unwrap_or_else(|e| e.into_inner());
            names
                .iter()
                .filter(|(_, mapped)| mapped.as_str() == identity)
                .map(|(name, _)| name.clone())
                .collect()
        };
        if names.is_empty() {
            return;
        }
        let mut statuses = self
            .inner
            .statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for name in names {
            statuses.remove(&name);
        }
    }

    /// 当前配置中与该客户端同身份的所有服务器是否覆盖 `path`。
    ///
    /// 共享客户端只保存启动它的第一个定义；其他别名可能声明了额外
    /// fileTypes，且 reload 可以在身份不变时修改 fileTypes。因此覆盖范围
    /// 必须只看当前配置，不能沿用客户端自己保存的旧定义。
    pub(crate) fn covers(&self, client: &Client, path: &Path) -> bool {
        let aliases: Vec<String> = {
            let names = self.inner.names.lock().unwrap_or_else(|e| e.into_inner());
            names
                .iter()
                .filter(|(_, identity)| identity.as_str() == client.identity)
                .map(|(name, _)| name.clone())
                .collect()
        };
        if aliases.is_empty() {
            return false;
        }
        let Ok(config) = self.config() else {
            return false;
        };
        aliases.iter().any(|name| {
            config
                .server(name)
                .is_some_and(|definition| definition.supports_file(path))
        })
    }

    pub(crate) fn state(&self, name: &str) -> ClientState {
        if let Some(client) = self.client(name) {
            return if client.is_closed() {
                ClientState::Failed(format!("{}: {}", client.name, client.close_reason()))
            } else {
                ClientState::Ready
            };
        }
        match self
            .inner
            .statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
        {
            Some(status) if status == "initializing" => ClientState::Initializing,
            Some(status) => ClientState::Failed(status.clone()),
            None => ClientState::NotStarted,
        }
    }

    fn set_status(&self, name: &str, status: Option<String>) {
        let mut statuses = self
            .inner
            .statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match status {
            Some(status) => {
                statuses.insert(name.to_owned(), status);
            }
            None => {
                statuses.remove(name);
            }
        }
    }

    /// 取得（必要时启动）一个服务器客户端；同 identity 单飞初始化。
    ///
    /// 名称只用于状态展示与配置路由：客户端、启动锁与进程 slot 都以完整
    /// identity 为键，因此同一工作区里同初始化的多个别名共享一个进程。
    pub(crate) fn ensure_client(
        &self,
        name: &str,
        deadline: Instant,
        cancellation: &pp_core::cancellation::CancellationToken,
    ) -> Result<Arc<Client>> {
        pp_core::cancellation::check(cancellation)?;
        ensure!(self.inner.settings.enabled, "LSP support is disabled.");
        loop {
            // reload 可能在取得生命周期锁前替换配置；身份不一致时释放锁重来，
            // 绝不用旧身份启动进程或写入新的 slot。
            let identity = self.identity_for(name)?;
            let lock = self.lock_for(&identity);
            let _guard = acquire(&lock, deadline, cancellation)?;
            pp_core::cancellation::check(cancellation)?;
            let prepared = {
                let _lifecycle = acquire(&self.inner.lifecycle, deadline, cancellation)?;
                ensure!(
                    !self.inner.closing.load(Ordering::SeqCst),
                    "LSP support is shutting down"
                );
                let definition = self.config()?.server(name).cloned().with_context(|| {
                    format!("LSP server `{name}` is not discovered for this workspace")
                })?;
                if identity_of(&definition, &self.inner.cwd) != identity {
                    None
                } else {
                    if let Some(client) = self.client_by_identity(&identity) {
                        if !client.is_closed() {
                            self.set_status(name, None);
                            return Ok(client);
                        }
                        self.evict_identity(&identity, &client)?;
                    }
                    if let Some((message, at)) = self
                        .inner
                        .negative
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&identity)
                        && at.elapsed() < NEGATIVE_CACHE
                    {
                        bail!("{message}");
                    }
                    // 上次回收失败而保留的 slot 必须先重试，不能插入新 slot 覆盖它。
                    let retained = self
                        .inner
                        .slots
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(&identity)
                        .cloned();
                    if let Some(retained) = retained {
                        match transport::terminate_slot(&retained, Duration::from_millis(500)) {
                            Ok(()) => self.remove_slot_if(&identity, &retained),
                            Err(error) => {
                                return Err(pp_core::cancellation::CleanupError(error).into());
                            }
                        }
                    }
                    let process = Arc::new(Mutex::new(transport::ProcessSlot {
                        child: None,
                        closing: false,
                    }));
                    self.inner
                        .slots
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(identity.clone(), process.clone());
                    Some((
                        definition,
                        self.inner.generation.load(Ordering::SeqCst),
                        process,
                    ))
                }
            };
            let Some((definition, generation, process)) = prepared else {
                continue;
            };
            self.set_status(name, Some("initializing".to_owned()));
            let started = Client::start(
                &definition,
                &self.inner.cwd,
                identity.clone(),
                generation,
                deadline,
                cancellation,
                process.clone(),
            );
            return match started {
                Ok(client) => {
                    let _lifecycle = match acquire(&self.inner.lifecycle, deadline, cancellation) {
                        Ok(guard) => guard,
                        Err(error) => {
                            let message = format!("{error:#}");
                            return match client.shutdown() {
                                Ok(()) => {
                                    self.remove_slot_if(&identity, &process);
                                    self.set_status(name, Some(message));
                                    Err(error)
                                }
                                Err(cleanup) => {
                                    self.set_status(name, Some(format!("{cleanup:#}")));
                                    Err(cleanup.context(message))
                                }
                            };
                        }
                    };
                    if self.inner.closing.load(Ordering::SeqCst)
                        || generation != self.inner.generation.load(Ordering::SeqCst)
                    {
                        return match client.shutdown() {
                            Ok(()) => {
                                self.remove_slot_if(&identity, &process);
                                self.set_status(
                                    name,
                                    Some("LSP initialization was superseded".to_owned()),
                                );
                                bail!("LSP initialization was superseded");
                            }
                            Err(cleanup) => {
                                let message = format!("{cleanup:#}");
                                self.set_status(name, Some(message));
                                Err(cleanup.context("LSP initialization was superseded"))
                            }
                        };
                    }
                    self.set_status(name, None);
                    self.register_name(name, &identity);
                    self.inner
                        .negative
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&identity);
                    self.inner
                        .clients
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(identity.clone(), client.clone());
                    Ok(client)
                }
                Err(error) => {
                    if let Err(cleanup) =
                        transport::terminate_slot(&process, Duration::from_millis(500))
                    {
                        self.set_status(name, Some(format!("{cleanup:#}")));
                        return Err(cleanup.context(error));
                    }
                    self.remove_slot_if(&identity, &process);
                    let message = format!("{error:#}");
                    let deterministic = !cancellation.is_cancelled()
                        && Instant::now() < deadline
                        && !error.chain().any(|cause| {
                            cause.is::<pp_core::cancellation::Cancelled>()
                                || cause.is::<pp_core::cancellation::CleanupError>()
                        });
                    self.set_status(name, Some(message.clone()));
                    if deterministic {
                        self.inner
                            .negative
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(identity, (message.clone(), Instant::now()));
                    }
                    Err(error)
                }
            };
        }
    }

    /// 移除并回收一个客户端（身份过期、reload 或配置切换）。
    pub(crate) fn evict(&self, name: &str, client: &Arc<Client>) -> Result<()> {
        self.evict_identity(&client.identity, client)?;
        self.set_status(name, None);
        Ok(())
    }

    /// 按身份回收客户端；只有确认回收成功才移除 map 与 slot。
    fn evict_identity(&self, identity: &str, client: &Arc<Client>) -> Result<()> {
        client.shutdown()?;
        {
            let mut clients = self.inner.clients.lock().unwrap_or_else(|e| e.into_inner());
            if clients
                .get(identity)
                .is_some_and(|existing| Arc::ptr_eq(existing, client))
            {
                clients.remove(identity);
            }
        }
        self.clear_statuses_for_identity(identity);
        self.inner
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(identity);
        Ok(())
    }

    /// reload：先读并验证完整候选配置，失败时保持现有配置与客户端不变。
    pub(crate) fn reload_config(&self) -> Result<Vec<String>> {
        ensure!(self.inner.settings.enabled, "LSP support is disabled.");
        let candidate = Arc::new(
            config::Config::load_with(&self.inner.cwd, self.inner.config_path.as_deref())
                .context("validating the LSP configuration candidate")?,
        );
        self.reload_candidate(candidate)
    }

    fn reload_candidate(&self, candidate: Arc<config::Config>) -> Result<Vec<String>> {
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        ensure!(
            !self.inner.closing.load(Ordering::SeqCst),
            "LSP support is shutting down"
        );
        // 新鲜的键是身份集合，不是名字：别名改名只要身份不变就继续复用进程。
        let fresh = alias_index(&candidate, &self.inner.cwd);
        let fresh_identities: HashSet<&str> = fresh.values().map(String::as_str).collect();
        let mut stale = Vec::new();
        let mut stale_clients = Vec::new();
        for client in self.started() {
            if fresh_identities.contains(client.identity.as_str()) {
                continue;
            }
            stale.push(client.name.clone());
            stale_clients.push(client);
        }
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        let mut errors = Vec::new();
        for client in stale_clients {
            if let Err(error) = self.evict_identity(&client.identity, &client) {
                errors.push(error);
            }
        }
        let slots: Vec<_> = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(identity, slot)| (identity.clone(), slot.clone()))
            .collect();
        for (identity, slot) in slots {
            if fresh_identities.contains(identity.as_str()) {
                continue;
            }
            match transport::terminate_slot(&slot, Duration::from_millis(500)) {
                Ok(()) => self.remove_slot_if(&identity, &slot),
                Err(error) => errors.push(error),
            }
        }
        cleanup_result(errors)?;
        *self.inner.config.lock().unwrap_or_else(|e| e.into_inner()) = Some(Ok(candidate));
        *self.inner.names.lock().unwrap_or_else(|e| e.into_inner()) = fresh;
        self.inner
            .negative
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        Ok(stale)
    }

    /// 仅取得句柄；停止从显式 stop 或兜底 Drop 开始。
    pub fn cleanup(&self) -> Cleanup {
        Cleanup {
            registry: self.clone(),
            armed: true,
        }
    }

    pub fn stop_all(&self) -> Result<()> {
        self.inner.closing.store(true, Ordering::SeqCst);
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
        // worker 初始化的收尾也需要 lifecycle；join 时不能持有它。
        let mut errors = Vec::new();
        if let Err(error) = self.inner.diagnostics.stop() {
            errors.push(error);
        }
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        self.inner
            .maintenance_stop
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        for client in self.started() {
            if let Err(error) = self.evict_identity(&client.identity, &client) {
                errors.push(error);
            }
        }
        let slots: Vec<_> = self
            .inner
            .slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(identity, slot)| (identity.clone(), slot.clone()))
            .collect();
        for (identity, slot) in slots {
            match transport::terminate_slot(&slot, Duration::from_millis(500)) {
                Ok(()) => self.remove_slot_if(&identity, &slot),
                Err(error) => errors.push(error),
            }
        }
        self.inner
            .statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        cleanup_result(errors)
    }

    /// 空闲回收：超过配置 `idleTimeoutMs` 且无活动操作的客户端被关闭。
    pub(crate) fn idle_sweep(&self) -> Result<Vec<String>> {
        let _lifecycle = self
            .inner
            .lifecycle
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 没有已启动客户端时无需超时配置：也避免惰性发现被后台扫描提前触发。
        let clients = self.started();
        if clients.is_empty() {
            return Ok(Vec::new());
        }
        let Some(timeout) = self
            .config()
            .ok()
            .and_then(|config| config.idle_timeout_ms)
            .filter(|value| *value > 0)
        else {
            return Ok(Vec::new());
        };
        let timeout = Duration::from_millis(timeout);
        let mut stopped = Vec::new();
        for client in clients {
            if client.shutdown_if_idle(timeout)? {
                self.evict_identity(&client.identity, &client)?;
                stopped.push(client.name.clone());
            }
        }
        Ok(stopped)
    }
}

fn cleanup_result(mut errors: Vec<anyhow::Error>) -> Result<()> {
    if errors.is_empty() {
        return Ok(());
    }
    let first = errors.remove(0);
    let note = errors
        .iter()
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>()
        .join("; ");
    Err(
        pp_core::cancellation::CleanupError(first.context(format!("LSP cleanup failures: {note}")))
            .into(),
    )
}

fn identity_of(definition: &config::ServerDefinition, cwd: &Path) -> String {
    let spawn = definition
        .resolved_command
        .as_deref()
        .unwrap_or_else(|| Path::new(&definition.command));
    let payload = serde_json::json!([
        definition.args,
        definition.init_options,
        definition.settings,
        definition.language_id,
    ])
    .to_string();
    format!(
        "{}\u{1f}{}\u{1f}{}",
        cwd.display(),
        spawn.display(),
        payload
    )
}

/// 已发现服务器名 → 完整身份。名称不参与身份，同一身份可以有多个别名。
fn alias_index(config: &config::Config, cwd: &Path) -> HashMap<String, String> {
    config
        .servers
        .iter()
        .map(|definition| (definition.name.clone(), identity_of(definition, cwd)))
        .collect()
}

fn acquire<'a>(
    lock: &'a Mutex<()>,
    deadline: Instant,
    cancellation: &pp_core::cancellation::CancellationToken,
) -> Result<MutexGuard<'a, ()>> {
    loop {
        pp_core::cancellation::check(cancellation)?;
        ensure!(
            Instant::now() < deadline,
            "LSP client initialization timed out"
        );
        match lock.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(poisoned)) => return Ok(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(LOCK_POLL);
            }
        }
    }
}

/// 退出清理句柄；显式 `stop` 返回错误，`Drop` 尽力而为。
pub struct Cleanup {
    registry: Registry,
    armed: bool,
}

impl Cleanup {
    /// 资源被明确转移给新的持有者时，旧句柄不再回收同一批客户端。
    pub fn supersede(&mut self, replacement: &Self) {
        if Arc::ptr_eq(&self.registry.inner, &replacement.registry.inner) {
            self.armed = false;
        }
    }

    pub fn stop(&mut self) -> Result<()> {
        self.registry.stop_all()?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let _ = self.registry.stop_all();
    }
}

// 内置服务器定义来源于 oh-my-pi 的 packages/coding-agent/src/lsp/defaults.json，遵循 MIT License。
