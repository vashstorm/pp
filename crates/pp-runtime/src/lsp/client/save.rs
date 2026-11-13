//! 已提交路径与文档视图的同步，不重新进入 operation。

use anyhow::Result;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(test)]
use crate::lsp::transport::ServerRequest;
use crate::lsp::types::SaveKind;
use crate::lsp::uri::path_to_uri;
#[cfg(test)]
use pp_core::cancellation::CancellationToken;

use super::document::read_document_text;
#[cfg(test)]
use super::handshake::unauthorized_edit;
use super::{Client, Op};

impl Client {
    /// 在已持有的 operation 里同步刚提交到磁盘的路径，不重新进入 operation。
    ///
    /// 调用方给出的是它已经按 Registry 覆盖关系筛选过的路径；这里不再用本客户端的
    /// `fileTypes` 二次过滤（共享/完整 identity 的别名可能比单一定义更宽）。
    /// 存在的普通文件：刷新文档文本、清除 `pending_write`，已打开的文档补发
    /// `didChange`/`didSave`；缺失或不再是普通文件：关闭已打开的文档、丢弃
    /// 文档状态与诊断。两种情况都发出对应的 watched-files 变更。
    ///
    /// 磁盘读取失败返回错误且不改写 overlay：调用方可以把它记成警告，但已提交
    /// 的事实不会因此被改写。
    #[cfg(test)]
    pub(crate) fn sync_committed_in_operation(
        &self,
        op: &mut Op<'_>,
        paths: &[PathBuf],
    ) -> Result<()> {
        self.sync_committed_locked(op, paths.iter().map(|path| (path.as_path(), 2)))
    }

    /// 在**已经持有本客户端 operation** 的 server 回调里同步刚提交的路径。
    ///
    /// 不变量：调用方必须位于 [`Client::operation`] 的 body 或 service 回调中，
    /// 且该 operation 仍持有 `op_lock`。因此本方法绝不重新取得 operation 锁、
    /// 也绝不进入另一个 operation（那会等待自己并永久阻塞）；它只写 transport
    /// 队列、更新文档缓存与诊断缓存。
    ///
    /// 不在 operation 内的普通保存后同步必须使用
    /// [`Client::sync_committed_in_operation`] 或 `edits::sync_committed`。
    #[cfg(test)]
    pub(crate) fn notify_committed_from_current_operation(
        &self,
        deadline: Instant,
        cancellation: &CancellationToken,
        paths: &[PathBuf],
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut deny = |request: ServerRequest| request.respond(unauthorized_edit());
        let service: &mut dyn FnMut(ServerRequest) = &mut deny;
        let mut op = Op {
            client: self,
            deadline,
            cancellation,
            service,
        };
        self.touch();
        self.sync_committed_locked(&mut op, paths.iter().map(|path| (path.as_path(), 2)))
    }

    pub(crate) fn sync_file_events_in_operation(
        &self,
        op: &mut Op<'_>,
        events: &[(PathBuf, u8)],
    ) -> Result<()> {
        self.sync_committed_locked(
            op,
            events.iter().map(|(path, kind)| (path.as_path(), *kind)),
        )
    }

    fn sync_committed_locked<'a>(
        &self,
        op: &mut Op<'_>,
        events: impl IntoIterator<Item = (&'a Path, u8)>,
    ) -> Result<()> {
        let mut watched = Vec::new();
        for (path, change_kind) in events {
            let uri = path_to_uri(path);
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_file() => {
                    let text = match read_document_text(path) {
                        Ok(text) => text,
                        Err(error) => {
                            // 已提交但无法表示为 UTF-8 文本：不清除 pending_write
                            // 会让后续 reconcile 永远被挡住，所以先清除标记再报错；
                            // 调用方仍会看到错误并保留“已提交”的事实。
                            if let Some(mut state) = self.take_document(path) {
                                state.pending_write = false;
                                self.store_document(path, &state);
                            }
                            return Err(error);
                        }
                    };
                    let state = self.take_document(path);
                    let Some(mut state) = state else {
                        // 没有打开过的文档：只通知内容变化，不替调用方打开新文档。
                        self.diagnostics.invalidate(&uri);
                        watched.push(json!({"uri": uri, "type": change_kind}));
                        continue;
                    };
                    state.pending_write = false;
                    state.last_used = Instant::now();
                    if state.text != text {
                        let previous_text = std::mem::replace(&mut state.text, text);
                        state.version += 1;
                        if state.opened {
                            self.notify_did_change(op, &state, &previous_text)?;
                        }
                    }
                    if state.opened && self.capabilities.save != SaveKind::Never {
                        let mut params = json!({"textDocument": {"uri": uri}});
                        if self.capabilities.save == SaveKind::IncludeText {
                            params["text"] = json!(&state.text);
                        }
                        op.notify("textDocument/didSave", params)?;
                    }
                    self.diagnostics.invalidate(&uri);
                    self.store_document(path, &state);
                    watched.push(json!({"uri": uri, "type": change_kind}));
                }
                Ok(_) => {
                    // 目录/FIFO/设备：不是可同步的文档，按删除处理。
                    self.close_document(op, path)?;
                    watched.push(json!({"uri": uri, "type": 3}));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    self.close_document(op, path)?;
                    watched.push(json!({"uri": uri, "type": 3}));
                }
                Err(error) => {
                    return Err(anyhow::Error::from(error).context(format!(
                        "inspecting {} while syncing {}",
                        path.display(),
                        self.name
                    )));
                }
            }
        }
        if !watched.is_empty() {
            op.notify(
                "workspace/didChangeWatchedFiles",
                json!({"changes": watched}),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;

    #[test]
    fn lsp_client_current_operation_sync_does_not_reenter_lock() {
        let (dir, client) = fixture_client(&[]);
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "committed\n").unwrap();
        // 在同一个 operation 里先放 overlay 再同步提交内容：helper 不能自己
        // 再取 operation 锁，否则会永久阻塞。
        with_client(&client, |op| {
            client.set_overlay(op, &path, "pending\n", true)?;
            client.notify_committed_from_current_operation(
                op.deadline(),
                op.cancellation(),
                std::slice::from_ref(&path),
            )
        });
        let state = client.document(&path).unwrap();
        assert_eq!(state.text, "committed\n");
        assert!(!state.pending_write);
        client.shutdown().unwrap();
    }
}
