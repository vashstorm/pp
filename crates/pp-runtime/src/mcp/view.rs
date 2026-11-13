//! 工具表与 `/mcp list` 快照：可用性过滤与来源状态。

use async_openai::types::chat::FunctionObject;
use std::sync::TryLockError;

use super::connect::exposed_name;
use super::{Access, EntryView, McpManager, RemoteTool, SharedServer, State, View};

impl McpManager {
    pub(crate) fn is_read_only(&self, name: &str) -> bool {
        let state = self.lock_state();
        state
            .tools
            .get(name)
            .is_some_and(|tool| read_only_tool(&state, tool))
    }

    pub(crate) fn definitions(&self, access: Access) -> Vec<FunctionObject> {
        let state = self.lock_state();
        state
            .tools
            .values()
            .filter(|tool| {
                tool_available(&state, tool)
                    && (access == Access::Full || read_only_tool(&state, tool))
            })
            .map(|tool| tool.definition.clone())
            .collect()
    }

    pub(crate) fn view(&self, access: Access) -> View {
        let state = self.lock_state();
        View {
            entries: state
                .directory
                .entries
                .iter()
                .map(|(name, entry)| {
                    let status = if !entry.enabled {
                        "disabled"
                    } else if entry.config.is_none() {
                        entry.status
                    } else if !entry
                        .config
                        .as_ref()
                        .is_some_and(|config| access.allows_server(config))
                    {
                        "suspended (Plan/Review)"
                    } else if let Some(server) = state.servers.get(name) {
                        // 并行调用正在使用服务时视为可用，不阻塞管理查询。
                        match server.try_lock() {
                            Ok(server) => {
                                if server.restart_after_cancel {
                                    "not connected (cancelled)"
                                } else if server.client.stopped {
                                    "failed (connection closed)"
                                } else {
                                    "connected"
                                }
                            }
                            Err(TryLockError::Poisoned(_)) | Err(TryLockError::WouldBlock) => {
                                "connected"
                            }
                        }
                    } else {
                        entry.status
                    };
                    EntryView {
                        name: name.clone(),
                        source: entry.source.clone(),
                        path: entry.path.clone(),
                        status,
                    }
                })
                .collect(),
            diagnostics: state
                .directory
                .diagnostics
                .iter()
                .cloned()
                .chain(state.servers.iter().filter_map(|(name, server)| {
                    let missing = server
                        .read_only_tools
                        .iter()
                        .filter(|original| {
                            state
                                .tools
                                .get(&exposed_name(name, original))
                                .is_none_or(|tool| {
                                    tool.server != *name || tool.original_name != **original
                                })
                        })
                        .count();
                    (missing > 0).then(|| {
                        format!(
                            "MCP {name}: {missing} configured read-only tools were not discovered"
                        )
                    })
                }))
                .collect(),
        }
    }
}

// 服务可用性：`WouldBlock` 表示正被其他线程调用，视为可用；中毒沿用旧的可读语义。
fn server_available(server: &SharedServer) -> bool {
    match server.try_lock() {
        Ok(state) => !state.client.stopped || state.restart_after_cancel,
        Err(TryLockError::Poisoned(_)) | Err(TryLockError::WouldBlock) => true,
    }
}

fn tool_available(state: &State, tool: &RemoteTool) -> bool {
    state
        .servers
        .get(&tool.server)
        .is_some_and(server_available)
}

pub(super) fn read_only_tool(state: &State, tool: &RemoteTool) -> bool {
    state
        .servers
        .get(&tool.server)
        .is_some_and(|server| server.read_only_tools.contains(&tool.original_name))
}
