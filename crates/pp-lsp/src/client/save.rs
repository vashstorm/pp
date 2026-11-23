//! 已提交路径与文档视图的同步，不重新进入 operation。

use anyhow::Result;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[cfg(test)]
use crate::transport::ServerRequest;
use crate::types::SaveKind;
use crate::uri::path_to_uri;
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
    /// 磁盘读取失败不中断同批其余事件：清除 `pending_write`、使诊断失效、把已知的
    /// change kind 记入 watched 列表，并以 warning 返回给调用方。已提交的事实不会
    /// 因此被改写，但失败也不会阻塞后续 reconcile。
    #[cfg(test)]
    pub(crate) fn sync_committed_in_operation(
        &self,
        op: &mut Op<'_>,
        paths: &[PathBuf],
    ) -> Result<Vec<String>> {
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
    ) -> Result<Vec<String>> {
        if paths.is_empty() {
            return Ok(Vec::new());
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
    ) -> Result<Vec<String>> {
        self.sync_committed_locked(
            op,
            events.iter().map(|(path, kind)| (path.as_path(), *kind)),
        )
    }

    fn sync_committed_locked<'a>(
        &self,
        op: &mut Op<'_>,
        events: impl IntoIterator<Item = (&'a Path, u8)>,
    ) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        let mut watched = Vec::new();
        for (path, change_kind) in events {
            let uri = path_to_uri(path);
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_file() => {
                    let text = match read_document_text(path) {
                        Ok(text) => text,
                        Err(error) => {
                            // 已提交但无法表示为 UTF-8 文本：不清除 pending_write
                            // 会让后续 reconcile 永远被挡住，所以先清标记、失效诊断，
                            // 再把已知的 change kind 记进 watched；错误本身降为
                            // warning，同批其余文件继续同步。
                            if let Some(mut state) = self.take_document(path) {
                                state.pending_write = false;
                                self.store_document(path, &state);
                            }
                            self.diagnostics.invalidate(&uri);
                            watched.push(json!({"uri": uri, "type": change_kind}));
                            warnings.push(format!(
                                "Failed to sync the committed text of {} with {}: {error:#}",
                                path.display(),
                                self.name
                            ));
                            continue;
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
                    let changed = state.text != text;
                    let will_notify_save =
                        state.opened && self.capabilities.save != SaveKind::Never;
                    if (changed && state.opened) || will_notify_save {
                        // 在向 writer 投递任何变更/保存通知之前使旧诊断失效，防止与 reader 线程竞态。
                        self.diagnostics.invalidate(&uri);
                    }
                    if changed {
                        let previous_text = std::mem::replace(&mut state.text, text);
                        state.version += 1;
                        if state.opened {
                            self.notify_did_change(op, &state, &previous_text)?;
                        }
                    }
                    if will_notify_save {
                        let mut params = json!({"textDocument": {"uri": uri}});
                        if self.capabilities.save == SaveKind::IncludeText {
                            params["text"] = json!(&state.text);
                        }
                        op.notify("textDocument/didSave", params)?;
                    }
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
        Ok(warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use crate::uri::path_to_uri;
    use std::time::{Duration, Instant};

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

    #[test]
    fn lsp_client_sync_continues_batch_after_unreadable_file() {
        let (dir, client) = fixture_client(&[]);
        let broken = dir.path().join("broken.fixture");
        let normal = dir.path().join("normal.fixture");
        std::fs::write(&broken, [0xff, 0xfe]).unwrap();
        std::fs::write(&normal, "committed\n").unwrap();
        // 两个文件各有 overlay（pending_write）；didOpen 的推送会在客户端缓存里
        // 建立旧报告，这是同步前必须存在的前置条件。
        with_client(&client, |op| {
            client.set_overlay(op, &broken, "pending\n", true)?;
            client.set_overlay(op, &normal, "pending\n", true)?;
            Ok(())
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let cached = [&broken, &normal].into_iter().all(|path| {
                client
                    .diagnostics
                    .snapshot(&path_to_uri(path))
                    .is_some_and(|entry| entry.seq > 0)
            });
            if cached || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        for path in [&broken, &normal] {
            assert!(
                client
                    .diagnostics
                    .snapshot(&path_to_uri(path))
                    .is_some_and(|entry| entry.seq > 0),
                "the didOpen push for {} must be cached before syncing",
                path.display()
            );
        }

        let result = with_client(&client, |op| {
            Ok(client.sync_committed_in_operation(op, &[broken.clone(), normal.clone()]))
        });
        let warnings = match result {
            Ok(warnings) => warnings,
            Err(error) => vec![format!("{error:#}")],
        };
        // 读取失败以 warning 返回，而不是中止整批同步。
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("not valid UTF-8")),
            "the unreadable file must produce a readable warning: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("broken.fixture")),
            "warnings must name the file that failed: {warnings:?}"
        );

        // 读取失败的文件之后不会再有任何推送，所以旧报告必须停在已失效状态。
        let broken_entry = client.diagnostics.snapshot(&path_to_uri(&broken));
        assert!(
            broken_entry.is_some_and(|entry| entry.seq == 0),
            "the diagnostics cache for {} must be invalidated",
            broken.display()
        );

        // 两份文件的 watched 通知都送达：非 UTF-8 不能挤掉同批的正常文件。
        let events = wait_logged(&dir, Instant::now() + Duration::from_secs(5), |events| {
            events.iter().any(|event| {
                event["method"] == "workspace/didChangeWatchedFiles"
                    && event["params"]["changes"]
                        .as_array()
                        .is_some_and(|changes| changes.len() == 2)
            })
        });
        let watched = events
            .iter()
            .filter(|event| event["method"] == "workspace/didChangeWatchedFiles")
            .flat_map(|event| {
                event["params"]["changes"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let watched_uris: Vec<String> = watched
            .iter()
            .filter_map(|change| change["uri"].as_str().map(str::to_owned))
            .collect();
        assert!(
            watched_uris.contains(&path_to_uri(&broken)),
            "the unreadable file must still be reported as changed: {watched:?}"
        );
        assert!(
            watched_uris.contains(&path_to_uri(&normal)),
            "a later normal file in the same batch must be synced: {watched:?}"
        );

        // 正常文件的文档状态继续推进：overlay 被 committed 内容替换并清除 pending_write。
        let state = client.document(&normal).unwrap();
        assert_eq!(state.text, "committed\n");
        assert!(!state.pending_write);
        // 失败文件的 overlay 也被清掉 pending_write，后续 reconcile 不再被挡；
        // 磁盘内容无法表示，所以保留它上一次已知的文本。
        let broken_state = client.document(&broken).unwrap();
        assert!(!broken_state.pending_write);
        assert_eq!(broken_state.text, "pending\n");

        // 失败之后同一客户端的后续同步仍可继续。
        let follow_up = with_client(&client, |op| {
            Ok(client.sync_committed_in_operation(op, std::slice::from_ref(&normal)))
        })
        .unwrap();
        assert!(follow_up.is_empty(), "a readable batch reports no warnings");
        client.shutdown().unwrap();
    }
}
