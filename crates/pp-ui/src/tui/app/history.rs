use crate::tui::{
    App, Level,
    history::{self, Reply, Request},
};
use anyhow::Result;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Fixed at 100 entries per page; matches the paging convention of persistent input recall
/// and adds no configuration key.
const HISTORY_PAGE: usize = 100;

/// The only user-visible hint when `history.db` is unavailable: shared by startup and
/// runtime and shown once.
const HISTORY_UNAVAILABLE: &str = "Input history is unavailable; recall is limited to this run.";

impl App {
    /// Starts the history worker at construction time and submits the first LoadInputs page.
    ///
    /// Must run before any `remember_text`, so FIFO ordering keeps this process's appends out
    /// of the startup load. Never blocks the main thread: `Store::open` runs inside the worker
    /// and a failure becomes the same hint through `Reply::Unavailable` handled by
    /// [`App::handle_history_reply`]. Returns the startup hint: on failure recall is explicitly
    /// limited to this process and no other HOME is ever used.
    pub(in crate::tui) fn start_history(&mut self, path: Option<PathBuf>) -> Option<String> {
        let Some(path) = path else {
            return Some("Input history is not persisted; recall is limited to this run.".into());
        };
        let worker = match history::Worker::new(path) {
            Ok(worker) => worker,
            Err(_) => return Some(HISTORY_UNAVAILABLE.into()),
        };
        self.history_request_id = 1;
        let request = Request::LoadInputs {
            request_id: 1,
            cwd: self.history_cwd.clone(),
            before_id: None,
        };
        if worker.requests.send(request).is_err() {
            return Some(HISTORY_UNAVAILABLE.into());
        }
        self.history_page_pending = Some(1);
        self.history_worker = Some(worker);
        None
    }

    /// Shuts the history worker down: cancels an unfinished index scan and joins queued
    /// appends. Replies still in the FIFO are drained after the join, so append/load failures
    /// discovered only at exit are reported too.
    pub(in crate::tui) fn shutdown_history(&mut self) {
        self.history_page_pending = None;
        let Some(mut worker) = self.history_worker.take() else {
            return;
        };
        let shutdown = worker.shutdown();
        let failed = drain_input_failures(&mut worker);
        if shutdown.is_err() {
            self.notify(
                Level::Error,
                "Input history did not shut down cleanly.".into(),
            );
        } else if failed {
            self.notify(
                Level::Error,
                "Input history did not save all accepted inputs.".into(),
            );
        }
    }

    /// Drains every history worker reply; session-search results go to the search panel to
    /// install.
    pub(in crate::tui) fn poll_history(&mut self) -> bool {
        let mut replies = Vec::new();
        if let Some(worker) = &self.history_worker {
            while let Ok(reply) = worker.replies.try_recv() {
                replies.push(reply);
            }
        }
        if replies.is_empty() {
            return false;
        }
        for reply in replies {
            self.handle_history_reply(reply);
        }
        true
    }

    /// Applies a single history reply; variants other than `Unavailable` keep the existing
    /// handling.
    fn handle_history_reply(&mut self, reply: Reply) {
        match reply {
            Reply::Unavailable => self.receive_history_unavailable(),
            Reply::InputAppended(result) => {
                if result.is_err() {
                    self.notify_history_failure(
                        "Could not save input history; the input was still accepted.",
                    );
                }
            }
            Reply::InputsLoaded {
                request_id,
                before_id,
                result,
            } => self.apply_history_page(request_id, before_id, result),
            Reply::SessionsSearched {
                panel_generation,
                request_id,
                result,
            } => self.receive_session_search(panel_generation, request_id, result),
            Reply::SessionPinned {
                panel_generation,
                request_id,
                result,
            } => self.receive_session_pin(panel_generation, request_id, result),
        }
    }

    /// `history.db` failed to open: disable persistent recall and release the worker so appends
    /// stop queueing; already accepted input stays in the in-process history, and the hint
    /// wording matches the startup failure exactly.
    fn receive_history_unavailable(&mut self) {
        self.history_page_pending = None;
        self.history_waiting_up = false;
        self.history_has_more = false;
        self.history_worker = None;
        self.notify_history_failure(HISTORY_UNAVAILABLE);
    }

    /// Session search sent to the history worker; when the worker is unavailable it only
    /// notifies and never blocks the UI.
    pub(in crate::tui) fn history_search(
        &mut self,
        panel_generation: u64,
        request_id: u64,
        cwd: Option<PathBuf>,
        query: String,
    ) -> bool {
        self.send_history(Request::Search {
            panel_generation,
            request_id,
            cwd,
            query,
        })
    }

    pub(in crate::tui) fn pin_session(&mut self, index: usize) {
        let Some(panel) = &mut self.session_panel else {
            return;
        };
        let panel_generation = panel.generation();
        let Some((request_id, id, pinned)) = panel.pin_request(index) else {
            return;
        };
        if !self.send_history(Request::SetSessionPinned {
            panel_generation,
            request_id,
            id,
            pinned,
        }) {
            self.receive_session_pin(
                panel_generation,
                request_id,
                Err(anyhow::anyhow!("History database unavailable.")),
            );
        }
    }

    pub(in crate::tui) fn receive_session_pin(
        &mut self,
        panel_generation: u64,
        request_id: u64,
        result: Result<()>,
    ) {
        let Some(panel) = &mut self.session_panel else {
            return;
        };
        if panel.generation() != panel_generation || !panel.pin_finished(request_id) {
            return;
        }
        match result {
            Ok(()) => {
                let (request_id, cwd, query) = panel.refresh_request();
                if !self.history_search(panel_generation, request_id, cwd, query)
                    && let Some(panel) = &mut self.session_panel
                {
                    panel.fail(request_id, "Pin saved; session search unavailable.".into());
                }
            }
            Err(_) => panel.set_feedback("Could not save session pin; please retry.".into()),
        }
    }

    /// After opening/deleting, the JSONL index is synced before searching.
    pub(in crate::tui) fn history_sync_and_search(
        &mut self,
        panel_generation: u64,
        request_id: u64,
        cwd: Option<PathBuf>,
        query: String,
    ) -> bool {
        self.send_history(Request::SyncAndSearch {
            panel_generation,
            request_id,
            cwd,
            query,
        })
    }

    /// The session panel generation increases monotonically; late replies after closing are
    /// dropped as stale.
    pub(in crate::tui) fn next_session_generation(&mut self) -> u64 {
        self.session_generation = self.session_generation.wrapping_add(1);
        self.session_generation
    }

    fn send_history(&mut self, request: Request) -> bool {
        let sent = self
            .history_worker
            .as_ref()
            .is_some_and(|worker| worker.requests.send(request).is_ok());
        if !sent {
            self.notify_history_failure("History search unavailable; restart pp.");
        }
        sent
    }

    fn notify_history_failure(&mut self, message: &str) {
        if self.history_failure_notified {
            return;
        }
        self.history_failure_notified = true;
        self.notify(Level::Error, message.into());
    }

    /// Installs one page of persistent input; both the startup page and paging prepend to the
    /// locally appended tail.
    fn apply_history_page(
        &mut self,
        request_id: u64,
        _before_id: Option<i64>,
        result: Result<Vec<pp_runtime::history::InputEntry>>,
    ) {
        // Only one load request may be in flight; replies to a non-current request must not
        // install the same page twice.
        if self.history_page_pending != Some(request_id) {
            return;
        }
        self.history_page_pending = None;
        match result {
            Err(_) => {
                self.history_waiting_up = false;
                self.notify_history_failure(
                    "Could not load older input history; recall keeps the loaded pages.",
                );
            }
            Ok(entries) => {
                let count = entries.len();
                if count == 0 {
                    self.history_has_more = false;
                    self.history_waiting_up = false;
                    return;
                }
                let oldest = entries.first().map(|entry| entry.id);
                self.resources.history_images = std::mem::take(&mut self.resources.history_images)
                    .into_iter()
                    .map(|(index, images)| (index + count, images))
                    .collect();
                let texts: Vec<String> = entries
                    .into_iter()
                    .enumerate()
                    .map(|(index, entry)| {
                        if !entry.images.is_empty() {
                            self.resources.history_images.insert(index, entry.images);
                        }
                        entry.text
                    })
                    .collect();
                // Prepending shifts every existing entry right, so correct the current
                // navigation index.
                if let Some(index) = &mut self.history_index {
                    *index += count;
                }
                self.input_history.splice(0..0, texts);
                if let Some(oldest) = oldest {
                    self.history_oldest_id = Some(oldest);
                }
                self.history_has_more = count == HISTORY_PAGE;
                if self.history_waiting_up {
                    // Complete this suspended Up: show the newest entry of the page just loaded.
                    self.history_waiting_up = false;
                    self.show_history(count - 1);
                }
            }
        }
    }

    /// Requests the next page (100 older entries); only issued when no page is pending.
    fn request_history_page(&mut self) {
        if self.history_page_pending.is_some() {
            return;
        }
        self.history_request_id = self.history_request_id.saturating_add(1);
        let request_id = self.history_request_id;
        let Some(worker) = &self.history_worker else {
            self.history_waiting_up = false;
            self.history_has_more = false;
            self.notify_history_failure("Input history is unavailable; recall keeps loaded pages.");
            return;
        };
        let request = Request::LoadInputs {
            request_id,
            cwd: self.history_cwd.clone(),
            before_id: self.history_oldest_id,
        };
        if worker.requests.send(request).is_ok() {
            self.history_page_pending = Some(request_id);
        } else {
            self.history_waiting_up = false;
            self.history_has_more = false;
            self.notify_history_failure("Input history is unavailable; recall keeps loaded pages.");
        }
    }

    /// Up at the bottom waits for the next page; repeated Up collapses into the same wait.
    pub(in crate::tui) fn wait_for_history_page(&mut self) {
        if self.history_waiting_up {
            return;
        }
        // The draft was captured long before browsing started and must not be overwritten by
        // the oldest entry.
        if self.history_index.is_none() {
            self.resources.history_draft = Some(self.resources.snapshot_draft());
            self.history_draft = Some((self.input.clone(), self.shell_mode));
        }
        self.history_waiting_up = true;
        self.request_history_page();
    }

    /// Refills one entry of the in-memory history, keeping multi-line text and the shell_mode
    /// decision.
    pub(in crate::tui) fn show_history(&mut self, index: usize) {
        if index >= self.input_history.len() {
            return;
        }
        self.resources.cancel();
        let images = self
            .resources
            .history_images
            .get(&index)
            .cloned()
            .unwrap_or_default();
        self.file_query = None;
        self.input = crate::tui::editor();
        self.shell_mode = false;
        let text = self
            .resources
            .restore_input(images, self.input_history[index].clone());
        self.insert_input(text);
        self.history_index = Some(index);
        // Keep the menu closed while refilling so the up/down keys can browse history
        // continuously.
        self.menu = None;
        self.menu_dismissed = true;
    }
}

/// Drains the replies of a worker that has finished; only input append/load failures are
/// reported (no hint is needed after session search exits).
fn drain_input_failures(worker: &mut history::Worker) -> bool {
    let mut failed = false;
    while let Ok(reply) = worker.replies.try_recv() {
        failed |= matches!(
            &reply,
            Reply::InputAppended(Err(_)) | Reply::InputsLoaded { result: Err(_), .. }
        );
    }
    failed
}

/// Both normal exit and drop guarantee the join; queued appends are still handled after the
/// scan is cancelled.
impl Drop for App {
    fn drop(&mut self) {
        if let Some(mut worker) = self.history_worker.take() {
            let _ = worker.shutdown();
            // The exit path does not draw: remaining replies are discarded, but the worker
            // must finish its queue before the connection is released.
            let _ = drain_input_failures(&mut worker);
        }
    }
}

/// Timestamps use integer UTC milliseconds; a broken system clock degrades to 0 and never
/// fakes a future time.
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

impl App {
    /// Single entry point for accepted raw input: appends to the in-memory history and
    /// submits the persistent append request.
    pub(in crate::tui) fn remember_text(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.history_index = None;
        self.history_draft = None;
        self.history_waiting_up = false;
        let images = if text == self.input_text() {
            self.resources.active_images(&text)
        } else {
            Vec::new()
        };
        if !images.is_empty() {
            self.resources
                .history_images
                .insert(self.input_history.len(), images.clone());
        }
        self.input_history.push(text.clone());
        let Some(worker) = &self.history_worker else {
            return;
        };
        let request = Request::AppendInput {
            cwd: self.history_cwd.clone(),
            text,
            images,
            created_at_ms: now_ms(),
        };
        if worker.requests.send(request).is_err() {
            self.notify_history_failure(
                "Input history is unavailable; the input was still accepted.",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::history::Reply;
    use crate::tui::{App, editor};

    fn page(start: i64, count: i64) -> Vec<pp_runtime::history::InputEntry> {
        (start..start + count)
            .map(|id| pp_runtime::history::InputEntry {
                id,
                text: format!("prompt-{id}"),
                images: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn sqlite_history_unavailable_and_first_page() {
        let directory = tempfile::tempdir().unwrap();
        let blocker = directory.path().join("blocked");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let mut app = App::new_with_skills(
            Err(anyhow::anyhow!("fixture")),
            pp_config::skills::Skills::default().shared(),
            crate::commands::FileCommands::default(),
            std::path::Path::new("."),
            Some(blocker.join("history.db")),
        )
        .unwrap();
        let before = app.conversation.entry_count();
        let reply = app
            .history_worker
            .as_ref()
            .unwrap()
            .replies
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(reply, Reply::Unavailable));
        app.handle_history_reply(reply);
        assert!(app.history_failure_notified && app.history_worker.is_none());
        assert!(app.history_page_pending.is_none() && !app.history_waiting_up);
        assert_eq!(app.conversation.entry_count(), before + 1);
        app.handle_history_reply(Reply::Unavailable);
        assert_eq!(app.conversation.entry_count(), before + 1);

        app.input = editor();
        app.insert_input("still accepted".into());
        app.remember_input();
        assert_eq!(app.input_history, ["still accepted"]);
        app.input = editor();
        app.navigate_history(true);
        assert_eq!(app.input.lines(), &["still accepted"]);
        app.navigate_history(false);
        assert_eq!(app.input.lines(), &[""]);

        let dir_ok = tempfile::tempdir().unwrap();
        let path = dir_ok.path().join("history.db");
        let cwd = std::path::Path::new(".");
        pp_runtime::history::Store::open(&path)
            .unwrap()
            .append_input_images(cwd, "previous process", &[], 0)
            .unwrap();
        let mut ok_app = App::new_with_skills(
            Err(anyhow::anyhow!("fixture")),
            pp_config::skills::Skills::default().shared(),
            crate::commands::FileCommands::default(),
            cwd,
            Some(path),
        )
        .unwrap();
        let reply = ok_app
            .history_worker
            .as_ref()
            .unwrap()
            .replies
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        ok_app.handle_history_reply(reply);
        assert_eq!(ok_app.input_history, ["previous process"]);
        assert!(ok_app.history_page_pending.is_none() && !ok_app.history_failure_notified);
    }

    #[test]
    fn sqlite_history_prepend_pending_up_and_stale() {
        let mut app = App::new(Err(anyhow::anyhow!("fixture"))).unwrap();
        app.input_history = vec!["local oldest".into()];
        app.input = editor();
        app.insert_input("local oldest".into());
        app.history_index = Some(0);
        app.history_oldest_id = Some(500);
        app.history_has_more = true;
        app.history_page_pending = Some(4);
        app.history_waiting_up = true;

        app.apply_history_page(4, Some(500), Ok(page(400, 100)));
        assert_eq!(
            (app.input_history.len(), app.history_index),
            (101, Some(99))
        );
        assert_eq!(app.input_history[100], "local oldest");
        assert_eq!(app.input.lines(), &["prompt-499"]);
        assert!(!app.history_waiting_up && app.history_page_pending.is_none());
        assert_eq!(
            (app.history_oldest_id, app.history_has_more),
            (Some(400), true)
        );
        app.navigate_history(false);
        assert_eq!(app.input.lines(), &["local oldest"]);
        app.navigate_history(false);
        assert_eq!(app.input.lines(), &[""]);

        let mut pend = App::new(Err(anyhow::anyhow!("fixture"))).unwrap();
        pend.input = editor();
        pend.insert_input("draft".into());
        assert_eq!(pend.history_page_pending, Some(1));
        pend.navigate_history(true);
        assert!(pend.history_waiting_up && pend.input_history.is_empty());
        assert_eq!(pend.history_request_id, 1);
        pend.apply_history_page(1, None, Ok(page(0, 3)));
        assert_eq!(
            (pend.history_index, pend.history_oldest_id),
            (Some(2), Some(0))
        );
        assert_eq!(pend.input.lines(), &["prompt-2"]);
        assert!(!pend.history_waiting_up);
        pend.navigate_history(false);
        assert_eq!(pend.input.lines(), &["draft"]);
        assert_eq!(pend.history_index, None);

        let mut stale = App::new(Err(anyhow::anyhow!("fixture"))).unwrap();
        stale.history_page_pending = Some(9);
        stale.history_waiting_up = true;
        stale.apply_history_page(8, None, Ok(page(0, 100)));
        assert!(stale.input_history.is_empty() && stale.history_page_pending == Some(9));
        stale.apply_history_page(9, None, Ok(Vec::new()));
        assert!(!stale.history_has_more && !stale.history_waiting_up);
        assert!(stale.history_page_pending.is_none() && stale.input_history.is_empty());
    }

    #[test]
    fn sqlite_history_errors_duplicates_and_generations() {
        let mut app = App::new(Err(anyhow::anyhow!("fixture"))).unwrap();
        app.input_history = vec!["kept".into()];
        app.history_page_pending = Some(3);
        app.history_waiting_up = true;
        app.apply_history_page(3, None, Err(anyhow::anyhow!("fixture unavailable")));
        assert!(app.history_failure_notified && !app.history_waiting_up);
        assert_eq!(app.input_history, ["kept"]);
        app.history_page_pending = Some(4);
        app.apply_history_page(4, None, Err(anyhow::anyhow!("fixture unavailable")));
        assert_eq!(app.input_history, ["kept"]);

        let mut dup = App::new(Err(anyhow::anyhow!("fixture"))).unwrap();
        dup.remember_text("repeat".into());
        dup.remember_text("repeat".into());
        dup.remember_text("  ".into());
        assert_eq!(dup.input_history, ["repeat", "repeat"]);
        let first = dup.next_session_generation();
        let second = dup.next_session_generation();
        assert_eq!(second, first + 1);
    }
}
