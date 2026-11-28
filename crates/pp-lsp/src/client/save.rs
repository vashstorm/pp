//! Synchronization of committed paths with the document view, without re-entering the operation.

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
    /// Sync paths just committed to disk inside an already held operation, without re-entering it.
    ///
    /// The caller supplies paths it has already filtered by Registry coverage; they are not filtered a second time by this client's
    /// `fileTypes` (aliases of a shared/full identity may be wider than a single definition).
    /// Existing regular file: refresh the document text, clear `pending_write`, and re-send
    /// `didChange`/`didSave` for an open document; missing or no longer a regular file: close the open document and discard
    /// the document state and diagnostics. Both cases emit the corresponding watched-files change.
    ///
    /// A failed disk read does not abort the rest of the batch: it clears `pending_write`, invalidates diagnostics, records the known
    /// change kind in the watched list, and returns the failure as a warning to the caller. The committed fact is never
    /// rewritten by it, and the failure does not block later reconciliation.
    #[cfg(test)]
    pub(crate) fn sync_committed_in_operation(
        &self,
        op: &mut Op<'_>,
        paths: &[PathBuf],
    ) -> Result<Vec<String>> {
        self.sync_committed_locked(op, paths.iter().map(|path| (path.as_path(), 2)))
    }

    /// Sync just-committed paths inside a server callback that **already holds this client's operation**.
    ///
    /// Invariant: the caller is inside the body or service callback of [`Client::operation`],
    /// and that operation still holds `op_lock`. So this method never re-acquires the operation lock and
    /// never enters another operation (that would wait on itself and block forever); it only writes the transport
    /// queue and updates the document and diagnostic caches.
    ///
    /// Post-save sync outside an operation must use
    /// [`Client::sync_committed_in_operation`] or `edits::sync_committed`.
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
                            // Committed but not representable as UTF-8 text: leaving pending_write set
                            // would block later reconciliation forever, so clear the flag, invalidate diagnostics,
                            // then record the known change kind in watched; the error itself is downgraded to a
                            // warning and the rest of the batch keeps syncing.
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
                        // A document never opened: only notify the content change, do not open the new document on the caller's behalf.
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
                        // Invalidate stale diagnostics before delivering any change/save notification to the writer, avoiding a race with the reader thread.
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
                    // Directory/FIFO/device: not a document that can be synced, treat it as deleted.
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
mod integration {
    use super::super::fixtures::*;
    use crate::uri::path_to_uri;
    use std::time::{Duration, Instant};

    #[test]
    fn lsp_client_current_operation_sync_does_not_reenter_lock() {
        let (dir, client) = fixture_client(&[]);
        let path = dir.path().join("sample.fixture");
        std::fs::write(&path, "committed\n").unwrap();
        // Inside one operation, install the overlay first and then sync the committed content: the helper must not
        // take the operation lock itself or it would block forever.
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
        // Both files have their own overlay (pending_write); the didOpen pushes establish stale reports
        // in the client cache, which is a prerequisite that must exist before syncing.
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
        // Read failures are returned as warnings instead of aborting the whole batch sync.
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

        // No further push will arrive for the file that failed to read, so its stale report must stay invalidated.
        let broken_entry = client.diagnostics.snapshot(&path_to_uri(&broken));
        assert!(
            broken_entry.is_some_and(|entry| entry.seq == 0),
            "the diagnostics cache for {} must be invalidated",
            broken.display()
        );

        // Watched notifications for both files are delivered: non-UTF-8 content cannot crowd out the normal file in the same batch.
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

        // The normal file's document state keeps advancing: the overlay is replaced by the committed content and pending_write is cleared.
        let state = client.document(&normal).unwrap();
        assert_eq!(state.text, "committed\n");
        assert!(!state.pending_write);
        // The failed file's overlay also has pending_write cleared so later reconciliation is not blocked; the on-disk content
        // cannot be represented, so its last known text is kept.
        let broken_state = client.document(&broken).unwrap();
        assert!(!broken_state.pending_write);
        assert_eq!(broken_state.text, "pending\n");

        // Later syncs on the same client still work after the failure.
        let follow_up = with_client(&client, |op| {
            Ok(client.sync_committed_in_operation(op, std::slice::from_ref(&normal)))
        })
        .unwrap();
        assert!(follow_up.is_empty(), "a readable batch reports no warnings");
        client.shutdown().unwrap();
    }
}
