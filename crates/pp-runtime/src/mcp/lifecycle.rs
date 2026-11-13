//! 退出停止、服务断开与 `/mcp enable|disable|reconnect|reload` 的变更流程。

use anyhow::{Result, ensure};
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::CleanupError;
use std::sync::Arc;

use super::client::{SharedConnection, stop_locked};
use super::{Access, Action, McpManager, SharedServer, config};

// 退出停止的工作集：槽用于 slot 级急停，服务句柄在停止后才释放，
// 避免 `Client::drop` 的优雅回收路径与显式停止互相干扰。
struct Tracked {
    slots: Vec<(String, SharedConnection)>,
    servers: Vec<SharedServer>,
}

// slot 级停止：只取连接槽锁，不等待 `ServerState`，因此能打断正在进行的请求。
fn stop_slots(slots: &[(String, SharedConnection)]) -> Result<()> {
    let mut failures = Vec::new();
    for (label, connection) in slots {
        let mut slot = connection.lock().unwrap_or_else(|error| error.into_inner());
        // 槽级关闭标记：清理完成后派发线程不能复活进程。
        slot.closing = true;
        if let Err(error) = stop_locked(&mut slot) {
            failures.push(format!("{label}: {error:#}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(CleanupError(anyhow::anyhow!("{}", failures.join("; "))).into())
    }
}

impl McpManager {
    /// 退出路径：先让空闲服务优雅退出，再按连接槽急停剩余连接。
    /// 调用后管理器进入关闭状态，不再接受新连接；失败聚合成 typed `CleanupError`。
    pub fn shutdown(&self) -> Result<()> {
        self.stop_tracked()
    }

    pub(super) fn stop_tracked(&self) -> Result<()> {
        let _control = self.lock_control();
        let tracked = self.take_tracked();
        // 先释放服务句柄：空闲的 stdio 服务在 `Client::drop` 中关闭 stdin 并得到
        // 有界宽限，服务端可自行收尾（退出后仍能观察到 fixture 的 `.closed`）。
        // 正被占用的服务仍由 worker 持有句柄，这里不会等待它。
        drop(tracked.servers);
        // 再按槽位急停剩余连接：不等待 `ServerState` 锁，能打断活动请求，
        // 并在槽级标记 closing，禁止清理完成后由 worker 复活进程。
        stop_slots(&tracked.slots)
    }

    // 在 state 下标记关闭并摘出全部槽与服务句柄；实际停止在锁外完成。
    fn take_tracked(&self) -> Tracked {
        let mut state = self.lock_state();
        state.closing = true;
        let servers = std::mem::take(&mut state.servers);
        state.tools.clear();
        let mut slots: Vec<(String, SharedConnection)> = servers
            .iter()
            .map(|(name, server)| (name.clone(), server.connection.clone()))
            .collect();
        for connection in std::mem::take(&mut state.connections) {
            if slots
                .iter()
                .any(|(_, existing)| Arc::ptr_eq(existing, &connection))
            {
                continue;
            }
            slots.push((format!("connection {}", slots.len() + 1), connection));
        }
        let names: Vec<String> = servers.keys().cloned().collect();
        for name in &names {
            if let Some(entry) = state.directory.entries.get_mut(name) {
                entry.status = "not connected";
            }
        }
        Tracked {
            slots,
            servers: servers.into_values().collect(),
        }
    }

    // 调用方必须已持有 control。
    pub(super) fn manage_locked(
        &self,
        action: &Action,
        token: &CancellationToken,
        access: Access,
    ) -> Result<()> {
        match action {
            Action::Help | Action::List => {}
            Action::Reload => {
                // 内存实例的 Reload 是空操作，绝不读取外部来源。
                let Some(path) = self.config.clone() else {
                    return Ok(());
                };
                // 解析失败保留旧连接与工具，直接返回错误。
                let directory = config::Directory::load(&path)?;
                // state 级停止：失败保留 servers/tools 供重试，不假装成功。
                self.stop_all_servers()?;
                {
                    let mut state = self.lock_state();
                    state.directory = directory;
                    state.loaded = true;
                }
                self.connect_enabled_locked(token, access)?;
            }
            Action::Enable(name) | Action::Disable(name) => {
                let enabled = matches!(action, Action::Enable(_));
                // 名称必须在已加载的目录里：内存实例因此报 unknown MCP server。
                let directory = {
                    let state = self.lock_state();
                    ensure!(
                        state.directory.entries.contains_key(name),
                        "unknown MCP server"
                    );
                    state.directory.clone()
                };
                // 文件锁与原子替换可能等待其他进程，绝不持 state。
                directory.save_enabled(name, enabled, token)?;
                if let Some(entry) = self.lock_state().directory.entries.get_mut(name) {
                    entry.enabled = enabled;
                }
                self.disconnect(name)?;
                if self.eligible(name, access) {
                    self.connect_one(name, token)?;
                }
            }
            Action::Reconnect(name) => {
                {
                    let state = self.lock_state();
                    let entry = state
                        .directory
                        .entries
                        .get(name)
                        .ok_or_else(|| anyhow::anyhow!("unknown MCP server"))?;
                    ensure!(entry.enabled, "MCP server is disabled; enable it first");
                    ensure!(
                        access == Access::Full || Self::eligible_entry(entry, access),
                        "MCP server has no authorized read-only tools for Plan/Review"
                    );
                }
                self.disconnect(name)?;
                self.connect_one(name, token)?;
            }
        }
        Ok(())
    }

    // 停止单个服务并摘除其工具：启停与重连前先断开旧连接。
    fn disconnect(&self, name: &str) -> Result<()> {
        let server = { self.lock_state().servers.get(name).cloned() };
        if let Some(server) = server {
            let mut state = server.lock().unwrap_or_else(|error| error.into_inner());
            state.client.stop().map_err(CleanupError)?;
            // 永久关闭旧引用，旧子 Agent 快照不能重新启动它。
            state.restart_after_cancel = false;
        }
        // 旧句柄在 state 锁外释放；`_removed` 在函数结束时触发 `Client::drop`。
        let _removed = {
            let mut state = self.lock_state();
            let removed = state.servers.remove(name);
            if removed.is_some()
                && let Some(entry) = state.directory.entries.get_mut(name)
            {
                entry.status = "not connected";
            }
            state.tools.retain(|_, tool| tool.server != name);
            removed
        };
        self.prune_connections();
        Ok(())
    }

    fn prune_connections(&self) {
        let mut state = self.lock_state();
        state.connections.retain(|slot| {
            slot.lock()
                .unwrap_or_else(|error| error.into_inner())
                .is_live()
        });
    }

    // 显式停止全部服务（`/mcp reload` 使用）：逐 server 调用 client.stop() 并检查
    // 结果；任何失败都保留 servers/tools 供重试，全部确认回收后才清空并清理残留槽。
    pub(super) fn stop_all_servers(&self) -> Result<()> {
        let servers: Vec<(String, SharedServer)> = {
            let state = self.lock_state();
            state
                .servers
                .iter()
                .map(|(name, server)| (name.clone(), server.clone()))
                .collect()
        };
        let mut failures = Vec::new();
        for (name, server) in &servers {
            let mut state = server.lock().unwrap_or_else(|error| error.into_inner());
            if let Err(error) = state.client.stop() {
                failures.push(format!("{name}: {error:#}"));
            }
            // 清理后旧快照不能再重建进程。
            state.restart_after_cancel = false;
        }
        if !failures.is_empty() {
            return Err(CleanupError(anyhow::anyhow!("{}", failures.join("; "))).into());
        }
        // 发现失败的连接可能还未注册工具，但仍留在跟踪列表里。
        let connections = { self.lock_state().connections.clone() };
        for (index, connection) in connections.iter().enumerate() {
            let mut slot = connection.lock().unwrap_or_else(|error| error.into_inner());
            if slot.is_live() {
                slot.closing = true;
                if let Err(error) = stop_locked(&mut slot) {
                    failures.push(format!("connection {}: {error:#}", index + 1));
                }
            }
        }
        if !failures.is_empty() {
            return Err(CleanupError(anyhow::anyhow!("{}", failures.join("; "))).into());
        }
        let _dropped = {
            let mut state = self.lock_state();
            let names: Vec<String> = state.servers.keys().cloned().collect();
            for name in &names {
                if let Some(entry) = state.directory.entries.get_mut(name) {
                    entry.status = "not connected";
                }
            }
            let dropped: Vec<SharedServer> =
                std::mem::take(&mut state.servers).into_values().collect();
            state.tools.clear();
            state.connections.clear();
            dropped
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{plan_register, plan_server_config, plan_spawn_client};
    use super::*;
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};

    #[test]
    fn plan_mcp_shutdown_stops_all_clients_and_clears_registry() {
        let manager = McpManager::empty();
        let mut processes = Vec::new();
        for name in ["a", "b"] {
            let (client, process) = plan_spawn_client(&plan_server_config(&["-c", "sleep 30"]));
            plan_register(&manager, name, client);
            processes.push(process);
        }
        manager.test_stop_servers().unwrap();
        {
            let state = manager.test_state();
            assert!(state.servers.is_empty() && state.tools.is_empty());
        }
        assert_eq!(manager.test_connection_count(), 0);
        for process in &processes {
            // 全部进程组已被回收，slot 不再持有句柄。
            assert!(process.lock().unwrap().child.is_none());
        }
        // 清空后重复停止仍是成功，不误报。
        manager.test_stop_servers().unwrap();
    }

    #[test]
    fn plan_mcp_shutdown_failure_retains_registry_and_retry_does_not_fake_success() {
        let manager = McpManager::empty();
        let (healthy, healthy_process) =
            plan_spawn_client(&plan_server_config(&["-c", "sleep 30"]));
        plan_register(&manager, "a", healthy);
        let (faulty, faulty_process) = plan_spawn_client(&plan_server_config(&["-c", "true"]));
        plan_register(&manager, "b", faulty);
        // 登记槽，验证失败时连接跟踪不被丢弃。
        {
            let mut state = manager.test_state();
            state.connections.push(healthy_process.clone());
            state.connections.push(faulty_process.clone());
        }

        // 故障注入：外部先回收子进程，使后续 kill/wait 无法确认回收。
        let pid = faulty_process.lock().unwrap().child.as_ref().unwrap().id() as i32;
        waitid(
            WaitId::Pid(Pid::from_raw(pid).expect("test child has a pid")),
            WaitIdOptions::EXITED,
        )
        .unwrap();
        assert!(faulty_process.lock().unwrap().is_live());

        let error = manager.test_stop_servers().err().unwrap();
        // 聚合错误保持 typed CleanupError，且逐服务保留明细。
        assert!(error.downcast_ref::<CleanupError>().is_some());
        assert!(format!("{error:#}").contains("b: stopping MCP process failed"));
        // 任一失败保留整个 servers/tools 与跟踪槽。
        {
            let state = manager.test_state();
            assert_eq!(state.servers.len(), 2);
            assert_eq!(state.tools.len(), 2);
        }
        assert_eq!(manager.test_connection_count(), 2);
        // 重试不假成功：故障未清除前停止仍然失败。
        assert!(manager.test_stop_servers().is_err());
        {
            let state = manager.test_state();
            assert!(!state.servers.is_empty() && !state.tools.is_empty());
        }

        // 故障清除后（句柄回收完成）重试才真正成功并清空。
        faulty_process.lock().unwrap().child.take();
        manager.test_stop_servers().unwrap();
        {
            let state = manager.test_state();
            assert!(state.servers.is_empty() && state.tools.is_empty());
        }
        assert!(healthy_process.lock().unwrap().child.is_none());
    }
}
