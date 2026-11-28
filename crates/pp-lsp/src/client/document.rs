//! Document state, didOpen/didChange, overlays and file text reads.

use anyhow::{Context, Result, ensure};
use rustix::fs::{Mode, OFlags};
use serde_json::json;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Instant;

use crate::config::ServerDefinition;
use crate::position::whole_range;
#[cfg(test)]
use crate::transport::ServerRequest;
use crate::types::SyncKind;
use crate::uri::path_to_uri;
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
    /// Time of the most recent didOpen; the cold-start empty-report window is computed per document.
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

    /// Open or refresh the target document: read the current on-disk content; only a real first open sends didOpen.
    ///
    /// A document may have been didClose'd after 5 minutes idle, so reopening must didOpen again
    /// and must not send didChange on a document that is not open.
    pub(crate) fn open_or_refresh(&self, op: &mut Op<'_>, path: &Path) -> Result<DocumentState> {
        self.open_or_refresh_with_sync(op, path)
            .map(|(state, _)| state)
    }

    pub(crate) fn open_or_refresh_with_sync(
        &self,
        op: &mut Op<'_>,
        path: &Path,
    ) -> Result<(DocumentState, bool)> {
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
            // The write path syncs itself; stale on-disk content must not overwrite the overlay here.
            return Ok((previous.clone(), false));
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
                self.diagnostics.invalidate(&uri);
                let synced = self.syncs_documents();
                if synced {
                    self.notify_did_open(op, &state)?;
                }
                self.store_document(path, &state);
                Ok((state, synced))
            }
            Some(mut state) => {
                state.last_used = now;
                if !state.opened {
                    // Reopen after an idle close: didOpen with the full content first, never another didChange.
                    self.ensure_workspace_folder(op, path)?;
                    state.version += 1;
                    state.text = text;
                    state.opened = true;
                    state.opened_at = now;
                    self.diagnostics.invalidate(&uri);
                    let synced = self.syncs_documents();
                    if synced {
                        self.notify_did_open(op, &state)?;
                    }
                    self.store_document(path, &state);
                    return Ok((state, synced));
                }
                if state.text == text {
                    self.store_document(path, &state);
                    return Ok((state, false));
                }
                let previous_text = std::mem::replace(&mut state.text, text);
                state.version += 1;
                self.diagnostics.invalidate(&uri);
                self.notify_did_change(op, &state, &previous_text)?;
                self.store_document(path, &state);
                Ok((state, true))
            }
        }
    }

    /// Whether the server needs didOpen (meaningful only with openClose or text synchronization capability).
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

    /// Put content that is not yet on disk into the server's document view (temporary overlay).
    ///
    /// The caller must already hold this client's operation (`op` comes from [`Client::operation`]);
    /// this neither re-locks nor re-enters the operation. Content is never read from disk: a new file that does not
    /// exist yet can still be established as a temporary document with `didOpen`; while `pending_write=true`,
    /// [`Client::open_or_refresh`] will not overwrite it with stale on-disk content.
    ///
    /// A change to an open document sends `didChange`; a document closed while idle or never opened sends
    /// `didOpen`. The diagnostic cache is invalidated per URI, so reports from an older version are never treated as fresh.
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
                    // Reopen after an idle close: only didOpen with the full content, never didChange first.
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

    /// Abandon temporary content that is not yet on disk: restore the on-disk content, and close the temporary document when the file still does not exist.
    ///
    /// Local invalidation is unconditional and happens first: even if the caller's token is already cancelled,
    /// the `deadline` has expired, or the subsequent notification fails, `pending_write` is cleared and the new file's state is removed.
    /// That way the next [`Client::open_or_refresh`] only reads from disk and never keeps using failed candidate
    /// content. On-disk content is read through a non-blocking fd (a FIFO cannot hang it), and the notification itself uses a bounded budget
    /// decoupled from caller cancellation; failures are returned only as warnings, and completed clearing is never rolled back.
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
            // On-disk content is readable: the cache returns to the real content, but an inconsistency with existing content still needs a server notification.
            self.store_document(path, &state);
        }
        self.diagnostics.invalidate(&uri);

        let notify_close = restored.is_none() && state.opened && self.capabilities.open_close;
        let notify_change = restored.is_some() && changed && state.opened && self.syncs_documents();
        if !notify_close && !notify_change {
            return Ok(());
        }
        // Bounded best-effort notification, decoupled from caller cancellation.
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
                // When the notification was not delivered, the cache must not keep a document that may disagree with the session: the next open
                // didOpens again from the on-disk content. Clearing happens before this error, which is only a warning.
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

    /// Close and discard the document state; `didClose` is sent only when the server declares openClose.
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

/// Text of the target file; missing, not a regular file and invalid UTF-8 are all errors.
///
/// The fd is opened with `O_NONBLOCK` first and then confirmed to be a regular file, so even if the path is swapped for a
/// FIFO between stat and read, `read` cannot hang forever. Symlinks are still followed normally for
/// read-only navigation (files outside the workspace may be read too).
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

/// Language identifier: configuration wins, otherwise inferred from the extension.
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
mod integration {
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

        // The on-disk content changes in the meantime, but the pending_write overlay must not be overwritten by a disk read.
        std::fs::write(&path, "disk-new\n").unwrap();
        let refreshed = with_client(&client, |op| client.open_or_refresh(op, &path));
        assert_eq!(refreshed.text, "pending   \n\n");
        assert!(refreshed.pending_write);

        // Sync after commit: the overlay adopts the final on-disk content and clears pending_write.
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
        // Cancelled token plus an expired deadline: pending_write must still be cleared immediately.
        let _ = client.abort_pending_write(
            &path,
            Instant::now() - Duration::from_secs(1),
            &cancellation,
        );
        // The next semantic query must read from disk and not keep reading the failed candidate content.
        let state = with_client(&client, |op| client.open_or_refresh(op, &path));
        assert_eq!(state.text, "disk\n");
        assert!(!state.pending_write);

        // A failed candidate for a new file is simply removed and no file is created.
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
