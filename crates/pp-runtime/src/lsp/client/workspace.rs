//! 工作区根发现与追加，以及 rust-analyzer 的 workspace-ready 轮询。

use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::lsp::transport::ServerRequest;
use crate::lsp::uri::path_to_uri;
use pp_core::cancellation::CancellationToken;

use super::handshake::{is_rust_analyzer, unauthorized_edit};
use super::{
    Client, DEFAULT_READY_POLL, DEFAULT_READY_SETTLE, DEFAULT_READY_TIMEOUT,
    DEFAULT_STATUS_TIMEOUT, Op,
};

/// 单个工作区根的 `WorkspaceFolder` 形态；initialize 与后续追加共用同一形状。
pub(super) fn folder_json(path: &Path) -> Value {
    json!({
        "uri": path_to_uri(path),
        "name": path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
    })
}

/// 打开 `file` 前应该追加的工作区根：解析 symlink 后文件所在 checkout，
/// 且不是客户端根所在的 checkout、也不是包含 HOME 的根（dotfiles 仓库会把
/// 整个主目录交给服务器索引）。只有声明 `changeNotifications` 的服务器才追加。
pub(super) fn checkout_to_add(
    root: &Path,
    change_notifications: bool,
    path: &Path,
    home: Option<&Path>,
) -> Option<PathBuf> {
    if !change_notifications {
        return None;
    }
    let file = real_path(path);
    let checkout = find_repo_root(file.parent()?)?;
    if find_repo_root(&real_path(root)).as_deref() == Some(checkout.as_path()) {
        return None;
    }
    if let Some(home) = home
        && path_inside(&checkout, &real_path(home))
    {
        return None;
    }
    Some(checkout)
}

/// 解析 symlink 的绝对路径；缺失的尾部保留，逐级解析已存在的父目录。
/// 解析失败时退回词法绝对路径，绝不因为权限/竞态而放弃判断。
pub(super) fn real_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    match std::fs::canonicalize(&absolute) {
        Ok(canonical) => canonical,
        Err(_) => match absolute.parent() {
            Some(parent) if parent != absolute.as_path() => match absolute.file_name() {
                Some(name) => real_path(parent).join(name),
                None => absolute,
            },
            _ => absolute,
        },
    }
}

/// 从 `start` 向上找最近的含 `.git`（文件或目录）的目录。
fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = real_path(start);
    loop {
        if std::fs::symlink_metadata(current.join(".git")).is_ok() {
            return Some(current);
        }
        current = current.parent()?.to_path_buf();
    }
}

/// `path` 是否等于或位于 `workspace` 之内（按路径组件比较）。
fn path_inside(workspace: &Path, path: &Path) -> bool {
    path.strip_prefix(workspace).is_ok()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

impl Client {
    pub(super) fn folders_json(&self) -> Vec<Value> {
        self.folders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|path| folder_json(path))
            .collect()
    }

    /// rust-analyzer 的 workspace-ready 轮询；其它服务器立即返回。
    pub(crate) fn wait_workspace_ready(
        &self,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        if !is_rust_analyzer(&self.command) {
            return Ok(());
        }
        let ready = self.definition.ready;
        let total = ready
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_READY_TIMEOUT);
        let poll = ready
            .poll_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_READY_POLL);
        let settle = ready
            .settle_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_READY_SETTLE);
        let status_timeout = ready
            .status_request_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_STATUS_TIMEOUT);
        let started = Instant::now();
        let limit = deadline.min(started + total);
        let mut service = |request: ServerRequest| request.respond(unauthorized_edit());
        self.operation(limit, cancellation, &mut service, |op| {
            loop {
                let poll_deadline = Instant::now() + status_timeout;
                let probe = self.transport.request(
                    "rust-analyzer/analyzerStatus",
                    json!({}),
                    poll_deadline.min(limit),
                    cancellation,
                    op.service,
                );
                if let Ok(Value::String(status)) = probe
                    && !status.starts_with("No workspaces")
                    && started.elapsed() >= settle
                {
                    return Ok(());
                }
                if Instant::now() >= limit {
                    return Ok(());
                }
                std::thread::sleep(poll);
            }
        })
    }

    /// 打开 `path` 前把其 checkout 加入工作区。
    ///
    /// 仅在服务器声明 `workspaceFolders.changeNotifications`、文件位于另一个
    /// checkout，且该 checkout 不包含 HOME 时追加。调用方已经持有操作锁，因此
    /// 这里直接使用同一个 `Op`，不再进入另一个 operation；追加必须发生在首次
    /// didOpen 之前。
    pub(crate) fn ensure_workspace_folder(&self, op: &mut Op<'_>, path: &Path) -> Result<()> {
        let Some(checkout) = self.workspace_folder_to_add(path) else {
            return Ok(());
        };
        if self.folder_listed(&checkout) {
            return Ok(());
        }
        self.deliver_workspace_folder(op, &checkout)
    }

    fn folder_listed(&self, root: &Path) -> bool {
        self.folders
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|existing| existing == root)
    }

    fn workspace_folder_to_add(&self, path: &Path) -> Option<PathBuf> {
        checkout_to_add(
            &self.root,
            self.capabilities.workspace_folders_change_notifications,
            path,
            home_dir().as_deref(),
        )
    }

    /// 在已持有的操作里发送一次文件夹追加；失败撤销本次新增与加载登记。
    fn deliver_workspace_folder(&self, op: &mut Op<'_>, root: &Path) -> Result<()> {
        // 先登记待加载：旧项目的进度 end 不能完成这次新根的加载等待。
        self.signals.begin_pending_load();
        {
            let mut folders = self.folders.lock().unwrap_or_else(|e| e.into_inner());
            folders.push(root.to_path_buf());
        }
        let notified = op.notify(
            "workspace/didChangeWorkspaceFolders",
            json!({"event": {"added": [folder_json(root)], "removed": []}}),
        );
        if let Err(error) = notified {
            let mut folders = self.folders.lock().unwrap_or_else(|e| e.into_inner());
            folders.retain(|existing| existing != root);
            self.signals.cancel_pending_load();
            return Err(error);
        }
        self.transport.set_folders(self.folders_json());
        self.roots_added.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_client_checkout_rules_follow_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("workspace");
        let other = dir.path().join("other");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(other.join(".git")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        std::fs::write(other.join("b.rs"), "").unwrap();

        // 服务器不支持 changeNotifications：永不追加。
        assert_eq!(
            checkout_to_add(&root, false, &root.join("src/a.rs"), None),
            None
        );
        // 文件与客户端根在同一个 checkout：服务器已经知道它。
        assert_eq!(
            checkout_to_add(&root, true, &root.join("src/a.rs"), None),
            None
        );
        // 另一个 checkout：追加解析 symlink 后的根。
        assert_eq!(
            checkout_to_add(&root, true, &other.join("b.rs"), None),
            Some(real_path(&other))
        );
        // 包含 HOME 的 checkout 不追加。
        assert_eq!(
            checkout_to_add(&root, true, &other.join("b.rs"), Some(&other)),
            None
        );
        assert_eq!(
            checkout_to_add(&root, true, &other.join("b.rs"), Some(&root)),
            Some(real_path(&other))
        );
        // 最近的 .git 优先。
        std::fs::create_dir_all(other.join("nested/.git")).unwrap();
        std::fs::write(other.join("nested/c.rs"), "").unwrap();
        assert_eq!(
            checkout_to_add(&root, true, &other.join("nested/c.rs"), None),
            Some(real_path(&other.join("nested")))
        );
        // 路径中的 symlink 先解析，再判断 checkout。
        std::os::unix::fs::symlink(&other, dir.path().join("link")).unwrap();
        assert_eq!(
            checkout_to_add(&root, true, &dir.path().join("link/b.rs"), None),
            Some(real_path(&other))
        );
    }
}
