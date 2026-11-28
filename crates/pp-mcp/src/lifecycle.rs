//! Shutdown stop, server disconnect and the change flows for `/mcp enable|disable|reconnect|reload`.

use anyhow::{Result, ensure};
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::CleanupError;
use std::sync::Arc;

use super::client::{SharedConnection, stop_locked};
use super::{Access, Action, McpManager, SharedServer, config};

// Working set for shutdown: slots drive a slot-level hard stop and server handles are released only afterwards,
// so `Client::drop`'s graceful reaping path cannot interfere with an explicit stop.
struct Tracked {
    slots: Vec<(String, SharedConnection)>,
    servers: Vec<SharedServer>,
}

// Slot-level stop: takes only the connection slot lock and never waits for `ServerState`, so it can interrupt an in-flight request.
fn stop_slots(slots: &[(String, SharedConnection)]) -> Result<()> {
    let mut failures = Vec::new();
    for (label, connection) in slots {
        let mut slot = connection.lock().unwrap_or_else(|error| error.into_inner());
        // Slot-level close flag: dispatch threads cannot revive the process once cleanup finished.
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
    /// Exit path: let idle servers exit gracefully first, then hard-stop the remaining connections slot by slot.
    /// Afterwards the manager is closing and accepts no new connections; failures are aggregated into a typed `CleanupError`.
    pub fn shutdown(&self) -> Result<()> {
        self.stop_tracked()
    }

    pub(super) fn stop_tracked(&self) -> Result<()> {
        // Cancel the startup connect first: the worker checks the token between servers, so control is released quickly afterwards.
        let token = self.lock_state().connect_token.clone();
        if let Some(token) = token {
            token.cancel();
        }
        let result = {
            let _control = self.lock_control();
            let tracked = self.take_tracked();
            // Release server handles first: an idle stdio server closes stdin in `Client::drop` and gets a
            // bounded grace period to finish up (after exit the fixture's `.closed` is still observable).
            // A server in use is still held by the worker, so this does not wait for it.
            drop(tracked.servers);
            // Then hard-stop the remaining connections slot by slot: without waiting for the `ServerState` lock it can interrupt
            // active requests, and it marks the slot as closing so the worker cannot revive the process after cleanup.
            stop_slots(&tracked.slots)
        };
        // Join only after the control lock is released: a starting worker needs control, so joining first would deadlock.
        let thread = self
            .connect_thread
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(thread) = thread {
            let _ = thread.join();
        }
        result
    }

    // Mark closing under state and take out all slots and server handles; the actual stop happens outside the lock.
    fn take_tracked(&self) -> Tracked {
        let mut state = self.lock_state();
        state.closing = true;
        let servers = std::mem::take(&mut state.servers);
        state.tools.clear();
        state.publish_tools_change();
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

    // The caller must already hold control.
    pub(super) fn manage_locked(
        &self,
        action: &Action,
        token: &CancellationToken,
        access: Access,
    ) -> Result<()> {
        match action {
            Action::Help | Action::List => {}
            Action::Reload => {
                // Reload on the in-memory instance is a no-op and never reads external sources.
                let Some(path) = self.config.clone() else {
                    return Ok(());
                };
                // A parse failure keeps the old connections and tools and returns the error directly.
                let directory = config::Directory::load(&path, self.discover_user_sources)?;
                // state-level stop: on failure keep servers/tools for retry instead of pretending success.
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
                // The name must exist in the loaded directory, so an in-memory instance reports unknown MCP server.
                let directory = {
                    let state = self.lock_state();
                    ensure!(
                        state.directory.entries.contains_key(name),
                        "unknown MCP server"
                    );
                    state.directory.clone()
                };
                // The file lock and atomic replacement may wait on other processes, so never hold state.
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

    // Stops a single server and removes its tools: enable/disable and reconnect disconnect the old connection first.
    fn disconnect(&self, name: &str) -> Result<()> {
        let server = { self.lock_state().servers.get(name).cloned() };
        if let Some(server) = server {
            let mut state = server.lock().unwrap_or_else(|error| error.into_inner());
            state.client.stop().map_err(CleanupError)?;
            // Permanently close the old reference so an old subagent snapshot cannot start it again.
            state.restart_after_cancel = false;
        }
        // The old handle is released outside the state lock; `_removed` triggers `Client::drop` at the end of the function.
        let _removed = {
            let mut state = self.lock_state();
            let removed = state.servers.remove(name);
            if removed.is_some() {
                state.publish_tools_change();
                if let Some(entry) = state.directory.entries.get_mut(name) {
                    entry.status = "not connected";
                }
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

    // Explicitly stops every server (used by `/mcp reload`): calls client.stop() per server and checks the
    // result; any failure keeps servers/tools for retry, and only after every stop is confirmed does it clear them and prune leftover slots.
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
            // An old snapshot cannot rebuild the process after cleanup.
            state.restart_after_cancel = false;
        }
        if !failures.is_empty() {
            return Err(CleanupError(anyhow::anyhow!("{}", failures.join("; "))).into());
        }
        // A connection that failed discovery may not have registered tools yet but is still tracked.
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
            state.publish_tools_change();
            state.connections.clear();
            dropped
        };
        Ok(())
    }
}

#[cfg(test)]
mod integration {
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
            // Every process group has been reaped and the slot no longer holds a handle.
            assert!(process.lock().unwrap().child.is_none());
        }
        // Repeated stop after clearing still succeeds and reports nothing falsely.
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
        // Register the slots so connection tracking is not lost when validation fails.
        {
            let mut state = manager.test_state();
            state.connections.push(healthy_process.clone());
            state.connections.push(faulty_process.clone());
        }

        // Fault injection: reap the child externally first so the later kill/wait cannot confirm the reaping.
        let pid = faulty_process.lock().unwrap().child.as_ref().unwrap().id() as i32;
        waitid(
            WaitId::Pid(Pid::from_raw(pid).expect("test child has a pid")),
            WaitIdOptions::EXITED,
        )
        .unwrap();
        assert!(faulty_process.lock().unwrap().is_live());

        let error = manager.test_stop_servers().err().unwrap();
        // The aggregated error stays a typed CleanupError and keeps per-server detail.
        assert!(error.downcast_ref::<CleanupError>().is_some());
        assert!(format!("{error:#}").contains("b: stopping MCP process failed"));
        // Any single failure keeps the whole servers/tools set and the tracking slots.
        {
            let state = manager.test_state();
            assert_eq!(state.servers.len(), 2);
            assert_eq!(state.tools.len(), 2);
        }
        assert_eq!(manager.test_connection_count(), 2);
        // A retry must not fake success: stopping still fails until the fault is cleared.
        assert!(manager.test_stop_servers().is_err());
        {
            let state = manager.test_state();
            assert!(!state.servers.is_empty() && !state.tools.is_empty());
        }

        // Only after the fault is cleared (handle reaped) does a retry really succeed and clear everything.
        faulty_process.lock().unwrap().child.take();
        manager.test_stop_servers().unwrap();
        {
            let state = manager.test_state();
            assert!(state.servers.is_empty() && state.tools.is_empty());
        }
        assert!(healthy_process.lock().unwrap().child.is_none());
    }
}
