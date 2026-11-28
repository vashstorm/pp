use crate::tui::{App, Level, Result, sessions};

impl App {
    pub(in crate::tui) fn close_session_panel(&mut self) {
        self.session_panel = None;
        self.menu_dismissed = true;
        // The search may still be running in the worker: late replies are dropped by
        // generation and never reopen the panel.
    }

    /// Opens /resume: allocates a new generation and sends the first synchronous search to the
    /// history worker.
    pub(in crate::tui) fn open_session_panel(&mut self) {
        let generation = self.next_session_generation();
        let mut panel = sessions::Panel::new(
            generation,
            self.session_id.clone(),
            std::env::current_dir().unwrap_or_default(),
        );
        let (request_id, cwd, query) = panel.sync_request();
        self.session_panel = Some(panel);
        if !self.history_sync_and_search(generation, request_id, cwd, query)
            && let Some(panel) = &mut self.session_panel
        {
            panel.fail(
                request_id,
                "Session search unavailable; the history database could not be opened.".into(),
            );
        }
    }

    /// Sends one async Search after a query/range change; on failure the edits are kept but
    /// stale rows are cleared.
    pub(in crate::tui) fn search_session_panel(&mut self) {
        let request = self.session_panel.as_mut().and_then(|panel| {
            let generation = panel.generation();
            panel
                .take_search()
                .map(|(request_id, cwd, query)| (generation, request_id, cwd, query))
        });
        let Some((generation, request_id, cwd, query)) = request else {
            return;
        };
        if !self.history_search(generation, request_id, cwd, query)
            && let Some(panel) = &mut self.session_panel
        {
            panel.fail(request_id, "Session search unavailable.".into());
        }
    }

    /// Session search reply from the history worker: only results for the current panel
    /// generation and the newest request are installed.
    pub(in crate::tui) fn receive_session_search(
        &mut self,
        panel_generation: u64,
        request_id: u64,
        result: Result<pp_runtime::agent::persistence::SessionListing>,
    ) {
        let mut skipped = 0;
        let mut failure = None;
        {
            let Some(panel) = self.session_panel.as_mut() else {
                return;
            };
            if panel.generation() != panel_generation {
                return;
            }
            match result {
                Ok(listing) => {
                    let count = listing.diagnostics.len();
                    if panel.install(request_id, listing) {
                        skipped = count;
                    }
                }
                Err(error) => {
                    let message = format!("{error:#}");
                    if panel.fail(request_id, format!("Session search failed: {message}")) {
                        failure = Some(message);
                    }
                }
            }
        }
        if skipped > 0 {
            self.notify(
                Level::Error,
                format!("Skipped {skipped} unreadable saved session file(s)."),
            );
        }
        if let Some(message) = failure {
            self.notify(
                Level::Error,
                format!("Failed to search saved sessions: {message}"),
            );
        }
    }
}
