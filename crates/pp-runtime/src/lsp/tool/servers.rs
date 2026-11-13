//! 服务器发现、路径解析与项目索引等待。

use crate::lsp::Registry;
use crate::lsp::adapters;
use crate::lsp::client::Client;
use crate::lsp::config::ServerDefinition;
use anyhow::Result;
use pp_core::cancellation::CancellationToken;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{Action, project_aware};

/// 需要先等项目索引完成的导航动作。
pub(super) const PROJECT_INDEXED_ACTIONS: &[&str] = &[
    "definition",
    "type_definition",
    "implementation",
    "references",
    "rename",
    "hover",
];

pub(super) fn resolve_path(cwd: &Path, file: &str) -> PathBuf {
    let path = Path::new(file);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

pub(super) fn display_path(cwd: &Path, path: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(relative) => {
            let text = relative.display().to_string();
            if text.is_empty() {
                ".".to_owned()
            } else {
                text
            }
        }
        Err(_) => path.display().to_string(),
    }
}

pub(super) fn real_servers_for(registry: &Registry, path: &Path) -> Vec<ServerDefinition> {
    registry.real_servers_for_file(path)
}

pub(super) fn all_real_servers(registry: &Registry) -> Vec<ServerDefinition> {
    registry
        .discovered()
        .into_iter()
        .filter(|definition| !adapters::is_adapter(&definition.name))
        .collect()
}

/// 项目根的 Rust 标记：只有存在 Cargo.toml/rust-analyzer.toml 祖先才等待索引。
fn has_rust_workspace_ancestor(path: &Path) -> bool {
    let mut current = path.parent();
    while let Some(dir) = current {
        if dir.join("Cargo.toml").is_file() || dir.join("rust-analyzer.toml").is_file() {
            return true;
        }
        current = dir.parent();
    }
    false
}

pub(super) fn wait_for_project(
    client: &Client,
    definition: &ServerDefinition,
    action: Action,
    path: &Path,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<()> {
    if project_aware(definition)
        && PROJECT_INDEXED_ACTIONS.contains(&action.as_str())
        && has_rust_workspace_ancestor(path)
    {
        client.wait_workspace_ready(deadline, cancellation)?;
    }
    Ok(())
}
