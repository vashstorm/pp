//! 文档状态、didOpen/didChange、overlay 与文件文本读取。

use anyhow::{Context, Result, ensure};
use rustix::fs::{Mode, OFlags};
use serde_json::json;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Instant;

use crate::lsp::config::ServerDefinition;
use crate::lsp::position::whole_range;
#[cfg(test)]
use crate::lsp::transport::ServerRequest;
use crate::lsp::types::SyncKind;
use crate::lsp::uri::path_to_uri;
#[cfg(test)]
use pp_core::cancellation::CancellationToken;

#[cfg(test)]
use super::ABORT_BUDGET;
#[cfg(test)]
use super::handshake::unauthorized_edit;
use super::{Client, Op};

#[derive(Clone, Debug)]
pub(crate) struct DocumentState {
    pub(crate) uri: String,
    pub(crate) language_id: String,
    pub(crate) version: i64,
    pub(crate) text: String,
    pub(crate) opened: bool,
    /// 最近一次 didOpen 的时间；冷启动空报告的窗口按文档计算。
    pub(crate) opened_at: Instant,
    pub(crate) last_used: Instant,
    pub(crate) pending_write: bool,
}

impl Client {
    pub(crate) fn document(&self, path: &Path) -> Option<DocumentState> {
        self.documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .cloned()
    }

    /// 打开或刷新目标文档：读取当前盘内容；真正首次打开时只发 didOpen。
    ///
    /// 文档可能因为 5 分钟空闲被 didClose，再次打开时必须重新 didOpen，
    /// 不能在未打开的文档上先发 didChange。
    pub(crate) fn open_or_refresh(&self, op: &mut Op<'_>, path: &Path) -> Result<DocumentState> {
        let uri = path_to_uri(path);
        let previous = self
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(path)
            .cloned();
        let language_id = language_id(&self.definition, path);
        if let Some(previous) = previous.as_ref()
            && previous.pending_write
        {
            // 写入流程自己负责同步；此时不得用盘上旧内容覆盖 overlay。
            return Ok(previous.clone());
        }
        let text = read_document_text(path)?;
        let now = Instant::now();
        match previous {
            None => {
                self.ensure_workspace_folder(op, path)?;
                let state = DocumentState {
                    uri: uri.clone(),
                    language_id: language_id.clone(),
                    version: 1,
                    text: text.clone(),
                    opened: true,
                    opened_at: now,
                    last_used: now,
                    pending_write: false,
                };
                if self.syncs_documents() {
                    self.notify_did_open(op, &state)?;
                }
                self.diagnostics.invalidate(&uri);
                self.store_document(path, &state);
                Ok(state)
            }
            Some(mut state) => {
                state.last_used = now;
                if !state.opened {
                    // 空闲关闭后重开：先用完整内容 didOpen，绝不再发一次 didChange。
                    self.ensure_workspace_folder(op, path)?;
                    state.version += 1;
                    state.text = text;
                    state.opened = true;
                    state.opened_at = now;
                    self.diagnostics.invalidate(&uri);
                    if self.syncs_documents() {
                        self.notify_did_open(op, &state)?;
                    }
                    self.store_document(path, &state);
                    return Ok(state);
                }
                if state.text == text {
                    self.store_document(path, &state);
                    return Ok(state);
                }
                let previous_text = std::mem::replace(&mut state.text, text);
                state.version += 1;
                self.diagnostics.invalidate(&uri);
                self.notify_did_change(op, &state, &previous_text)?;
                self.store_document(path, &state);
                Ok(state)
            }
        }
    }

    /// 服务器是否需要 didOpen（只有 openClose 或文本同步能力时才有意义）。
    pub(super) fn syncs_documents(&self) -> bool {
        self.capabilities.open_close || self.capabilities.sync != SyncKind::None
    }

    fn notify_did_open(&self, op: &mut Op<'_>, state: &DocumentState) -> Result<()> {
        op.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": state.uri,
                    "languageId": state.language_id,
                    "version": state.version,
                    "text": state.text,
                }
            }),
        )
    }

    pub(super) fn notify_did_change(
        &self,
        op: &mut Op<'_>,
        state: &DocumentState,
        previous_text: &str,
    ) -> Result<()> {
        if self.capabilities.sync == SyncKind::None {
            return Ok(());
        }
        let change = match self.capabilities.sync {
            SyncKind::Full => json!({"text": state.text}),
            SyncKind::Incremental => {
                json!({"range": whole_range(previous_text), "text": state.text})
            }
            SyncKind::None => json!({"text": state.text}),
        };
        op.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": state.uri, "version": state.version},
                "contentChanges": [change],
            }),
        )
    }

    pub(super) fn store_document(&self, path: &Path, state: &DocumentState) {
        self.documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_path_buf(), state.clone());
    }

    /// 把一份尚未落盘的内容放进服务器文档视图（临时 overlay）。
    ///
    /// 调用方必须已经持有本客户端的 operation（`op` 来自 [`Client::operation`]），
    /// 这里既不重新加锁，也不再次进入 operation。内容从不读盘：新文件即使还
    /// 不存在也能先以 `didOpen` 建立临时文档；`pending_write=true` 期间
    /// [`Client::open_or_refresh`] 不会用盘上旧内容覆盖它。
    ///
    /// 已打开的文档内容变化发 `didChange`，空闲关闭或从未打开的文档发
    /// `didOpen`。诊断缓存按 URI 失效，旧版本的报告不会被当成新鲜结果。
    #[cfg(test)]
    pub(crate) fn set_overlay(
        &self,
        op: &mut Op<'_>,
        path: &Path,
        content: &str,
        pending_write: bool,
    ) -> Result<DocumentState> {
        let uri = path_to_uri(path);
        let previous = self.document(path);
        let now = Instant::now();
        let state = match previous {
            None => {
                self.ensure_workspace_folder(op, path)?;
                let state = DocumentState {
                    uri: uri.clone(),
                    language_id: language_id(&self.definition, path),
                    version: 1,
                    text: content.to_owned(),
                    opened: true,
                    opened_at: now,
                    last_used: now,
                    pending_write,
                };
                if self.syncs_documents() {
                    self.notify_did_open(op, &state)?;
                }
                state
            }
            Some(mut state) => {
                state.last_used = now;
                state.pending_write = pending_write;
                if !state.opened {
                    // 空闲关闭后重开：只能用完整内容 didOpen，不能先发 didChange。
                    self.ensure_workspace_folder(op, path)?;
                    state.version += 1;
                    state.text = content.to_owned();
                    state.opened = true;
                    state.opened_at = now;
                    if self.syncs_documents() {
                        self.notify_did_open(op, &state)?;
                    }
                } else if state.text != content {
                    let previous_text = std::mem::replace(&mut state.text, content.to_owned());
                    state.version += 1;
                    self.notify_did_change(op, &state, &previous_text)?;
                }
                state
            }
        };
        self.diagnostics.invalidate(&uri);
        self.store_document(path, &state);
        Ok(state)
    }

    /// 放弃一次尚未落盘的临时内容：恢复盘上内容；文件仍不存在时关闭临时文档。
    ///
    /// 本地失效是无条件且先发生的：即使调用方令牌已取消、`deadline` 已过期，
    /// 或随后的通知失败，`pending_write` 也会被清除，新文件的状态会被移除。
    /// 这样下一次 [`Client::open_or_refresh`] 只会读盘，绝不会继续使用失败的候选
    /// 内容。磁盘内容读取用非阻塞 fd 完成（FIFO 不会挂住），通知本身使用与调用
    /// 方取消解耦的有界预算；失败只作为警告返回，已完成的清除不会回退。
    #[cfg(test)]
    pub(crate) fn abort_pending_write(
        &self,
        path: &Path,
        _deadline: Instant,
        _cancellation: &CancellationToken,
    ) -> Result<()> {
        let uri = path_to_uri(path);
        let Some(mut state) = self.take_document(path) else {
            return Ok(());
        };
        let previous_text = state.text.clone();
        state.pending_write = false;
        state.last_used = Instant::now();
        let restored = read_document_text(path).ok();
        let mut changed = false;
        if let Some(text) = restored.as_ref()
            && state.text != *text
        {
            state.version += 1;
            state.text = text.clone();
            changed = true;
        }
        if restored.is_some() {
            // 盘上内容可读：缓存回到真实内容，但已有内容不一致时仍需通知服务器。
            self.store_document(path, &state);
        }
        self.diagnostics.invalidate(&uri);

        let notify_close = restored.is_none() && state.opened && self.capabilities.open_close;
        let notify_change = restored.is_some() && changed && state.opened && self.syncs_documents();
        if !notify_close && !notify_change {
            return Ok(());
        }
        // 有界、与调用方取消解耦的 best-effort 通知。
        let mut deny = |request: ServerRequest| request.respond(unauthorized_edit());
        let service: &mut dyn FnMut(ServerRequest) = &mut deny;
        let limit = Instant::now() + ABORT_BUDGET;
        let token = CancellationToken::new();
        let notified = self.operation(limit, &token, service, |op| {
            if notify_close {
                op.notify(
                    "textDocument/didClose",
                    json!({"textDocument": {"uri": uri}}),
                )?;
            } else {
                self.notify_did_change(op, &state, &previous_text)?;
            }
            Ok(())
        });
        match notified {
            Ok(()) => Ok(()),
            Err(error) => {
                // 通知没送达时缓存不能留下可能与会话不一致的文档：下一次打开
                // 会以盘上内容重新 didOpen。清除先于这个错误，错误只是警告。
                let _ = self.take_document(path);
                self.diagnostics.invalidate(&uri);
                Err(error)
            }
        }
    }

    pub(super) fn take_document(&self, path: &Path) -> Option<DocumentState> {
        self.documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(path)
    }

    /// 关闭并丢弃文档状态；`didClose` 只在服务器声明 openClose 时发送。
    pub(super) fn close_document(&self, op: &mut Op<'_>, path: &Path) -> Result<()> {
        match self.take_document(path) {
            Some(state) => self.close_document_state(op, path, state),
            None => Ok(()),
        }
    }

    fn close_document_state(
        &self,
        op: &mut Op<'_>,
        path: &Path,
        state: DocumentState,
    ) -> Result<()> {
        let uri = path_to_uri(path);
        self.diagnostics.invalidate(&uri);
        if state.opened && self.capabilities.open_close {
            op.notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": uri}}),
            )?;
        }
        Ok(())
    }
}

/// 目标文件的文本；缺失、非普通文件与非法 UTF-8 都是错误。
///
/// 先以 `O_NONBLOCK` 打开 fd，再检查它确实是普通文件：这样 stat 与 read
/// 之间即使路径被换成 FIFO，也不会在 `read` 上永久挂住。符号链接仍按
/// 只读导航的需要正常跟随（工作区外文件也允许读取）。
pub(crate) fn read_document_text(path: &Path) -> Result<String> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("reading {} for the language server", path.display()))?;
    let mut file = File::from(fd);
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting {}", path.display()))?;
    ensure!(
        metadata.is_file(),
        "{} is not a regular file",
        path.display()
    );
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    Ok(text)
}

/// 语言标识：配置优先，否则按扩展名推断。
pub(crate) fn language_id(definition: &ServerDefinition, path: &Path) -> String {
    if let Some(language_id) = &definition.language_id {
        return language_id.clone();
    }
    let extension = path
        .extension()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if name == "dockerfile" {
        return "dockerfile".to_owned();
    }
    match extension.as_str() {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "json" => "json",
        "jsonc" => "jsonc",
        "css" => "css",
        "go" => "go",
        "py" | "pyi" => "python",
        "rb" => "ruby",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "cs" => "csharp",
        "php" => "php",
        "lua" => "lua",
        "sh" | "bash" | "zsh" => "shellscript",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "html" | "htm" => "html",
        "vue" => "vue",
        "svelte" => "svelte",
        "astro" => "astro",
        "swift" => "swift",
        "c" => "c",
        "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => "cpp",
        "m" => "objective-c",
        "mm" => "objective-cpp",
        "zig" => "zig",
        "tla" | "tlaplus" => "tlaplus",
        "tex" => "latex",
        "md" => "markdown",
        "dart" => "dart",
        "erl" | "hrl" => "erlang",
        "ex" | "exs" => "elixir",
        "hs" => "haskell",
        "ml" | "mli" => "ocaml",
        "sol" => "solidity",
        "tf" | "tfvars" => "terraform",
        "graphql" | "gql" => "graphql",
        "prisma" => "prisma",
        "vim" => "vim",
        "nix" => "nix",
        "cmake" => "cmake",
        _ => "plaintext",
    }
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use serde_json::Value;
    use std::time::{Duration, Instant};

    #[test]
    fn lsp_client_reopen_sends_did_open_without_did_change() {
        let (dir, client) = fixture_client(&[]);
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "first\n").unwrap();
        let _ = with_client(&client, |op| client.open_or_refresh(op, &path));
        client
            .documents
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&path)
            .unwrap()
            .opened = false;
        std::fs::write(&path, "second\n").unwrap();
        let _ = with_client(&client, |op| client.open_or_refresh(op, &path));

        let events = wait_logged(&dir, Instant::now() + Duration::from_secs(5), |events| {
            events
                .iter()
                .filter(|event| event["method"] == "textDocument/didOpen")
                .count()
                >= 2
        });
        let opens: Vec<&Value> = events
            .iter()
            .filter(|event| event["method"] == "textDocument/didOpen")
            .collect();
        let changes = events
            .iter()
            .filter(|event| event["method"] == "textDocument/didChange")
            .count();
        assert_eq!(opens.len(), 2, "reopen must send didOpen again");
        assert_eq!(
            changes, 0,
            "an unopened document must not receive didChange"
        );
        assert_eq!(
            opens[1]["params"]["textDocument"]["text"], "second\n",
            "reopen must send the full current text"
        );
        client.shutdown().unwrap();
    }

    #[test]
    fn lsp_client_pending_overlay_survives_disk_reconcile() {
        let (dir, client) = fixture_client(&[]);
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "disk-old\n").unwrap();
        let state = with_client(&client, |op| {
            client.set_overlay(op, &path, "pending   \n\n", true)
        });
        assert!(state.pending_write);
        assert_eq!(state.text, "pending   \n\n");

        // 盘上内容在这期间变化，但 pending_write 的 overlay 不能被读盘覆盖。
        std::fs::write(&path, "disk-new\n").unwrap();
        let refreshed = with_client(&client, |op| client.open_or_refresh(op, &path));
        assert_eq!(refreshed.text, "pending   \n\n");
        assert!(refreshed.pending_write);

        // 提交后同步：overlay 采用最终盘上内容并清除 pending_write。
        std::fs::write(&path, "committed\n").unwrap();
        with_client(&client, |op| {
            client.sync_committed_in_operation(op, std::slice::from_ref(&path))
        });
        let synced = with_client(&client, |op| client.open_or_refresh(op, &path));
        assert_eq!(synced.text, "committed\n");
        assert!(!synced.pending_write);

        let events = wait_logged(&dir, Instant::now() + Duration::from_secs(5), |events| {
            events.iter().any(|event| {
                event["method"] == "textDocument/didChange"
                    && event["params"]["contentChanges"][0]["text"] == "committed\n"
            })
        });
        let open = events
            .iter()
            .find(|event| event["method"] == "textDocument/didOpen")
            .unwrap();
        assert_eq!(open["params"]["textDocument"]["text"], "pending   \n\n");
        let changes: Vec<&Value> = events
            .iter()
            .filter(|event| event["method"] == "textDocument/didChange")
            .collect();
        let last = changes.last().expect("committed didChange");
        assert_eq!(
            last["params"]["contentChanges"][0]["text"], "committed\n",
            "committed content must replace the pending overlay"
        );
        client.shutdown().unwrap();
    }

    #[test]
    fn lsp_client_abort_pending_write_restores_disk_and_closes_new_file() {
        let (dir, client) = fixture_client(&[]);
        let existing = dir.path().join("existing.fixture");
        std::fs::write(&existing, "disk\n").unwrap();
        let _ = with_client(&client, |op| {
            client.set_overlay(op, &existing, "pending\n", true)
        });
        client
            .abort_pending_write(
                &existing,
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .unwrap();
        let state = client.document(&existing).unwrap();
        assert_eq!(state.text, "disk\n");
        assert!(!state.pending_write);

        let new_file = dir.path().join("new.fixture");
        let _ = with_client(&client, |op| {
            client.set_overlay(op, &new_file, "pending\n", true)
        });
        client
            .abort_pending_write(
                &new_file,
                Instant::now() + Duration::from_secs(5),
                &CancellationToken::new(),
            )
            .unwrap();
        assert!(client.document(&new_file).is_none());
        assert!(!new_file.exists(), "aborting must not create the file");

        let events = wait_logged(&dir, Instant::now() + Duration::from_secs(5), |events| {
            events
                .iter()
                .any(|event| event["method"] == "textDocument/didClose")
        });
        let closed: Vec<&Value> = events
            .iter()
            .filter(|event| event["method"] == "textDocument/didClose")
            .collect();
        assert_eq!(closed.len(), 1, "only the temporary new document closed");
        let uri = closed[0]["params"]["textDocument"]["uri"].as_str().unwrap();
        assert!(uri.ends_with("new.fixture"), "{uri}");
        client.shutdown().unwrap();
    }

    #[test]
    fn lsp_client_abort_pending_write_clears_with_cancelled_token() {
        let (dir, client) = fixture_client(&[]);
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "disk\n").unwrap();
        let _ = with_client(&client, |op| {
            client.set_overlay(op, &path, "failed candidate\n", true)
        });
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        // 已取消的令牌 + 已过期的 deadline：仍必须立刻清除 pending_write。
        let _ = client.abort_pending_write(
            &path,
            Instant::now() - Duration::from_secs(1),
            &cancellation,
        );
        // 下一次语义查询必须读盘，不能继续读失败的候选内容。
        let state = with_client(&client, |op| client.open_or_refresh(op, &path));
        assert_eq!(state.text, "disk\n");
        assert!(!state.pending_write);

        // 新文件的失败候选直接移除，且不会创建文件。
        let new_file = dir.path().join("new.fixture");
        let _ = with_client(&client, |op| {
            client.set_overlay(op, &new_file, "failed candidate\n", true)
        });
        let _ = client.abort_pending_write(
            &new_file,
            Instant::now() - Duration::from_secs(1),
            &cancellation,
        );
        assert!(client.document(&new_file).is_none());
        assert!(!new_file.exists());
        let reopened = with_client(&client, |op| client.open_or_refresh(op, &path));
        assert_eq!(reopened.text, "disk\n");
        client.shutdown().unwrap();
    }

    #[test]
    fn lsp_client_reads_regular_files_through_nonblocking_fd() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("ok.fixture");
        std::fs::write(&file, "ok\n").unwrap();
        assert_eq!(read_document_text(&file).unwrap(), "ok\n");
        assert!(
            read_document_text(dir.path()).is_err(),
            "directories are not documents"
        );

        let fifo = dir.path().join("pipe.fixture");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        let error = read_document_text(&fifo).unwrap_err();
        assert!(
            format!("{error:#}").contains("not a regular file"),
            "{error:#}"
        );

        let invalid = dir.path().join("bad.fixture");
        std::fs::write(&invalid, [0xff, 0xfe]).unwrap();
        let error = read_document_text(&invalid).unwrap_err();
        assert!(
            format!("{error:#}").contains("not valid UTF-8"),
            "{error:#}"
        );
    }
}
