//! 工具调用：短临界区解析句柄，再在 server 锁内转发 `tools/call`。

use anyhow::{Context, Result, bail, ensure};
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc, thread, time::Duration};

use super::view::read_only_tool;
use super::{Access, McpManager, RemoteTool};

impl McpManager {
    pub(crate) fn proxy_call(
        self: &Arc<Self>,
        name: &str,
        arguments: &str,
        access: Access,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        cancellation::check(cancellation)?;
        // worker 持独立 Arc 与新 transport token：父连接不因子 Agent 取消而中断。
        let manager = Arc::clone(self);
        let request_token = cancellation.clone();
        let name = name.to_owned();
        let arguments = arguments.to_owned();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        thread::Builder::new()
            .name("pp-mcp-proxy".into())
            .spawn(move || {
                let outcome = manager.call_inner(&name, &arguments, access, &request_token, true);
                let _ = sender.send(outcome);
            })?;
        loop {
            cancellation::check(cancellation)?;
            match receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(result) => return result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => bail!("MCP worker exited"),
            }
        }
    }

    pub(crate) fn call(
        &self,
        name: &str,
        arguments: &str,
        access: Access,
        cancellation: &CancellationToken,
    ) -> Result<Value> {
        self.call_inner(name, arguments, access, cancellation, false)
    }

    fn call_inner(
        &self,
        name: &str,
        arguments: &str,
        access: Access,
        cancellation: &CancellationToken,
        proxy: bool,
    ) -> Result<Value> {
        // 短临界区：解析工具、校验 access、克隆服务句柄与该服务的定义快照；
        // 之后阻塞获取 server 锁，绝不在持 state 锁时等待网络或进程。
        let (server, server_name, original_name, expected) = {
            let state = self.lock_state();
            let tool = state.tools.get(name).context("unknown tool")?;
            ensure!(
                access == Access::Full || read_only_tool(&state, tool),
                "MCP tool is not authorized for read-only access"
            );
            let server = state
                .servers
                .get(&tool.server)
                .cloned()
                .context("MCP server is not connected")?;
            let expected: BTreeMap<String, RemoteTool> = state
                .tools
                .iter()
                .filter(|(_, other)| other.server == tool.server)
                .map(|(name, other)| (name.clone(), other.clone()))
                .collect();
            (
                server,
                tool.server.clone(),
                tool.original_name.clone(),
                expected,
            )
        };
        let arguments: Value = serde_json::from_str(arguments)
            .map_err(|_| anyhow::anyhow!("invalid MCP tool arguments"))?;
        ensure!(
            arguments.is_object(),
            "MCP tool arguments must be an object"
        );
        let mut state = server.lock().unwrap_or_else(|error| error.into_inner());
        if state.restart_after_cancel {
            Self::restart(&server_name, &mut state, &expected, cancellation)?;
        }
        cancellation::check(cancellation)?;
        let transport_token = if proxy {
            CancellationToken::new()
        } else {
            cancellation.clone()
        };
        let result = match state.client.request(
            "tools/call",
            json!({"name":original_name,"arguments":arguments}),
            &transport_token,
        ) {
            Ok(result) => result,
            Err(error) => {
                // 仅任务取消赋予下一次实际调用时的重建资格；普通故障保持不可用。
                if error.downcast_ref::<Cancelled>().is_some() {
                    state.restart_after_cancel = true;
                }
                return Err(error);
            }
        };
        ensure!(
            result["content"].is_array() && result.get("isError").is_none_or(Value::is_boolean),
            "invalid MCP tools/call result"
        );
        // 保留 MCP 内容块和结构化结果，工具级失败仍作为正常协议响应处理。
        Ok(json!({"ok":result["isError"] != true, "result":result}))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{POLL, ServerConfig, StdioConfig, TransportConfig};
    use super::*;
    use std::collections::BTreeSet;
    use std::time::Instant;

    #[test]
    fn proxy_cancelled_while_queued_never_dispatches_or_closes_shared_connection() {
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            transport: TransportConfig::Stdio(StdioConfig {
                command: "bash".into(),
                args: vec![format!(
                    "{}/../../tests/fixtures/mcp_server.sh",
                    env!("CARGO_MANIFEST_DIR")
                )],
                env: BTreeMap::from([("MCP_LOG".into(), "proxy-log".into())]),
                cwd: None,
            }),
            timeout_seconds: 1,
            timeout_millis: None,
            read_only_tools: BTreeSet::new(),
        };
        let manager = Arc::new(
            McpManager::connect(
                &BTreeMap::from([("local".into(), config)]),
                dir.path(),
                &CancellationToken::new(),
            )
            .unwrap(),
        );
        let server = manager.test_server("local").expect("已连接的服务");
        // 先占住 server 锁，让代理调用停在 `call_inner` 的阻塞获取上。
        let guard = server.lock().unwrap();
        let proxy = manager.clone();
        let token = CancellationToken::new();
        let worker_token = token.clone();
        let worker = thread::spawn(move || {
            proxy.proxy_call(
                "mcp__local__echo",
                r#"{"text":"must-not-dispatch"}"#,
                Access::Full,
                &worker_token,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        // worker 在 state 下克隆服务句柄后 strong_count 才达到 3；
        // 此时取消才能证明“取到锁后才发现已取消”，而不是压根没进入调用。
        while Arc::strong_count(&server.state) < 3 {
            assert!(Instant::now() < deadline);
            thread::sleep(POLL);
        }
        token.cancel();
        assert!(worker.join().unwrap().unwrap_err().is::<Cancelled>());
        drop(guard);
        while Arc::strong_count(&server.state) > 2 {
            assert!(Instant::now() < deadline);
            thread::sleep(POLL);
        }
        // 取消不关闭共享连接：同一 manager 的后继调用照常成功。
        assert!(
            manager
                .call(
                    "mcp__local__echo",
                    r#"{"text":"survived"}"#,
                    Access::Full,
                    &CancellationToken::new()
                )
                .unwrap()["ok"]
                == true
        );
        let log = std::fs::read_to_string(dir.path().join("proxy-log")).unwrap();
        assert!(!log.contains("must-not-dispatch"));
    }
}
