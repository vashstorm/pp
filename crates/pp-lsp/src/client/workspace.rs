//! Workspace root discovery and appending, plus rust-analyzer's workspace-ready polling.

use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::transport::ServerRequest;
use crate::uri::path_to_uri;
use pp_core::cancellation::CancellationToken;

use super::handshake::{is_rust_analyzer, unauthorized_edit};
use super::{
    Client, DEFAULT_READY_POLL, DEFAULT_READY_SETTLE, DEFAULT_READY_TIMEOUT,
    DEFAULT_STATUS_TIMEOUT, Op,
};

/// `WorkspaceFolder` shape for a single workspace root; initialize and later appends share the same shape.
pub(super) fn folder_json(path: &Path) -> Value {
    json!({
        "uri": path_to_uri(path),
        "name": path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string()),
    })
}

/// Workspace root that should be appended before opening `file`: the checkout containing the file after resolving symlinks,
/// provided it is neither the checkout of the client root nor a root containing HOME (a dotfiles repository would hand
/// the entire home directory to the server for indexing). Only servers declaring `changeNotifications` append.
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

/// Absolute path with symlinks resolved; a missing tail is kept, resolving existing parent directories step by step.
/// Falls back to the lexical absolute path on failure, never giving up the decision because of permissions or a race.
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

/// Walk up from `start` to the nearest directory containing `.git` (file or directory).
fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = real_path(start);
    loop {
        if std::fs::symlink_metadata(current.join(".git")).is_ok() {
            return Some(current);
        }
        current = current.parent()?.to_path_buf();
    }
}

/// Whether `path` equals or lies inside `workspace` (compared by path component).
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

    /// rust-analyzer's workspace-ready polling; other servers return immediately.
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
        let limit = started
            .checked_add(total)
            .map_or(deadline, |at| deadline.min(at));
        let mut service = |request: ServerRequest| request.respond(unauthorized_edit());
        self.operation(limit, cancellation, &mut service, |op| {
            loop {
                pp_core::cancellation::check(cancellation)?;
                let now = Instant::now();
                if now >= limit {
                    return Ok(());
                }
                let poll_deadline = now
                    .checked_add(status_timeout)
                    .map_or(limit, |at| at.min(limit));
                let probe = self.transport.request(
                    "rust-analyzer/analyzerStatus",
                    json!({}),
                    poll_deadline,
                    cancellation,
                    op.service,
                );
                match probe {
                    Ok(Value::String(status))
                        if !status.starts_with("No workspaces") && started.elapsed() >= settle =>
                    {
                        return Ok(());
                    }
                    Err(crate::transport::Failure::Cancelled) => {
                        return Err(pp_core::cancellation::Cancelled.into());
                    }
                    Err(crate::transport::Failure::Closed) => {
                        return Ok(());
                    }
                    _ => {}
                }
                let remaining = limit.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Ok(());
                }
                std::thread::sleep(poll.min(remaining));
            }
        })
    }

    /// Add the file's checkout to the workspace before opening `path`.
    ///
    /// It is appended only when the server declares `workspaceFolders.changeNotifications`, the file lives in another
    /// checkout, and that checkout does not contain HOME. The caller already holds the operation lock, so
    /// this uses the same `Op` directly and does not enter another operation; the append must happen before the first
    /// didOpen.
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

    /// Send one folder append inside the operation already held; on failure undo this addition and its load registration.
    fn deliver_workspace_folder(&self, op: &mut Op<'_>, root: &Path) -> Result<()> {
        // Register the pending load first: a progress end from the old project cannot complete the wait for this new root.
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
mod integration {
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

        // The server does not support changeNotifications: never append.
        assert_eq!(
            checkout_to_add(&root, false, &root.join("src/a.rs"), None),
            None
        );
        // The file and the client root are in the same checkout: the server already knows it.
        assert_eq!(
            checkout_to_add(&root, true, &root.join("src/a.rs"), None),
            None
        );
        // Another checkout: append the symlink-resolved root.
        assert_eq!(
            checkout_to_add(&root, true, &other.join("b.rs"), None),
            Some(real_path(&other))
        );
        // A checkout containing HOME is not appended.
        assert_eq!(
            checkout_to_add(&root, true, &other.join("b.rs"), Some(&other)),
            None
        );
        assert_eq!(
            checkout_to_add(&root, true, &other.join("b.rs"), Some(&root)),
            Some(real_path(&other))
        );
        // The nearest .git wins.
        std::fs::create_dir_all(other.join("nested/.git")).unwrap();
        std::fs::write(other.join("nested/c.rs"), "").unwrap();
        assert_eq!(
            checkout_to_add(&root, true, &other.join("nested/c.rs"), None),
            Some(real_path(&other.join("nested")))
        );
        // Symlinks in the path are resolved first, then the checkout is decided.
        std::os::unix::fs::symlink(&other, dir.path().join("link")).unwrap();
        assert_eq!(
            checkout_to_add(&root, true, &dir.path().join("link/b.rs"), None),
            Some(real_path(&other))
        );
    }
}
