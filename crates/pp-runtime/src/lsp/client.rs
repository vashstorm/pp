//! 单个语言服务器的受管理客户端：初始化握手、文档状态、诊断缓存与操作串行化。

mod diagnostics;
mod document;
mod handshake;
mod lifecycle;
mod operation;
mod save;
mod workspace;

use std::time::Duration;

pub(crate) use document::read_document_text;
pub(crate) use lifecycle::Client;
pub(crate) use operation::Op;

pub(super) const POLL: Duration = Duration::from_millis(20);
/// 未版本化报告的稳定窗口。
pub(super) const SETTLE: Duration = Duration::from_millis(250);
/// 冷启动/重开后的未版本化空报告在这个窗口内不算干净。
pub(super) const COLD_WINDOW: Duration = Duration::from_secs(12);
pub(super) const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const DEFAULT_READY_POLL: Duration = Duration::from_millis(100);
pub(super) const DEFAULT_READY_SETTLE: Duration = Duration::from_secs(2);
pub(super) const DEFAULT_STATUS_TIMEOUT: Duration = Duration::from_millis(1000);
const PROJECT_LOAD_TIMEOUT: Duration = Duration::from_secs(15);
/// 正常 shutdown RPC 的等待上限。
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// shutdown 之后等待进程自己退出的宽限。
const EXIT_GRACE: Duration = Duration::from_secs(1);
/// initialize 失败后回收进程的宽限。
const HANDSHAKE_GRACE: Duration = Duration::from_millis(500);
/// 放弃待写 overlay 时 best-effort 通知的预算；与调用方取消/已过期 deadline 解耦。
#[cfg(test)]
const ABORT_BUDGET: Duration = Duration::from_secs(2);

#[cfg(test)]
mod fixtures {
    use super::*;
    use crate::lsp::config;
    use crate::lsp::transport::{ProcessSlot, ServerRequest};
    use anyhow::Result;
    use pp_core::cancellation::CancellationToken;
    use serde_json::{Value, json};
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    use super::handshake::unauthorized_edit;

    /// 用受控 stdio fixture 启动一个独立客户端；不读取真实 HOME，也不经 registry。
    pub(super) fn fixture_client(pull: &[&str]) -> (TempDir, Arc<Client>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".pp")).unwrap();
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/lsp_server.py");
        let log = dir.path().join("requests.jsonl");
        let mut args = vec![
            script.to_string_lossy().into_owned(),
            "--log".to_owned(),
            log.to_string_lossy().into_owned(),
        ];
        args.extend(pull.iter().map(|value| (*value).to_owned()));
        std::fs::write(
            dir.path().join(".pp/lsp.json"),
            json!({"servers": {"fixture": {
                "command": "python3",
                "args": args,
                "fileTypes": [".fixture"],
                "rootMarkers": [".pp"],
            }}})
            .to_string(),
        )
        .unwrap();
        let config = config::Config::load_with(dir.path(), None).unwrap();
        let definition = config.server("fixture").unwrap().clone();
        let process = Arc::new(Mutex::new(ProcessSlot {
            child: None,
            closing: false,
        }));
        let client = Client::start(
            &definition,
            dir.path(),
            "fixture".to_owned(),
            1,
            Instant::now() + Duration::from_secs(5),
            &CancellationToken::new(),
            process,
        )
        .unwrap();
        (dir, client)
    }

    pub(super) fn logged(dir: &TempDir) -> Vec<Value> {
        std::fs::read_to_string(dir.path().join("requests.jsonl"))
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect()
    }

    /// 服务器在自己的进程里写日志；有界轮询等待目标事件真正被处理。
    pub(super) fn wait_logged(
        dir: &TempDir,
        deadline: Instant,
        mut ready: impl FnMut(&[Value]) -> bool,
    ) -> Vec<Value> {
        loop {
            let events = logged(dir);
            if ready(&events) || Instant::now() >= deadline {
                return events;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn with_client<T>(
        client: &Arc<Client>,
        body: impl FnOnce(&mut Op<'_>) -> Result<T>,
    ) -> T {
        let mut service = |request: ServerRequest| request.respond(unauthorized_edit());
        client
            .operation(
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
                &mut service,
                body,
            )
            .unwrap()
    }
}
