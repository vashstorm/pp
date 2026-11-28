//! Input recall and derived search use a dedicated thread and are not occupied by the model-request worker.
use anyhow::Result;
use pp_core::cancellation::CancellationToken;
use pp_runtime::agent::persistence::SessionListing;
use pp_runtime::history::InputEntry;
use pp_runtime::history::Store;
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

const UNAVAILABLE: &str = "History database unavailable.";
const INPUT_PAGE_SIZE: usize = 100;

pub(super) enum Request {
    AppendInput {
        cwd: PathBuf,
        text: String,
        images: Vec<pp_runtime::user_input::Image>,
        created_at_ms: i64,
    },
    LoadInputs {
        request_id: u64,
        cwd: PathBuf,
        before_id: Option<i64>,
    },
    SyncAndSearch {
        panel_generation: u64,
        request_id: u64,
        cwd: Option<PathBuf>,
        query: String,
    },
    Search {
        panel_generation: u64,
        request_id: u64,
        cwd: Option<PathBuf>,
        query: String,
    },
    SetSessionPinned {
        panel_generation: u64,
        request_id: u64,
        id: String,
        pinned: bool,
    },
    Shutdown,
}

pub(super) enum Reply {
    /// `history.db` failed to open: the main thread no longer blocks on the handshake; this reply reports it and disables persistent recall.
    Unavailable,
    InputAppended(Result<i64>),
    InputsLoaded {
        request_id: u64,
        before_id: Option<i64>,
        result: Result<Vec<InputEntry>>,
    },
    SessionsSearched {
        panel_generation: u64,
        request_id: u64,
        result: Result<SessionListing>,
    },
    SessionPinned {
        panel_generation: u64,
        request_id: u64,
        result: Result<()>,
    },
}

pub(super) struct Worker {
    pub(super) requests: Sender<Request>,
    pub(super) replies: Receiver<Reply>,
    scan_cancellation: CancellationToken,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    /// Builds only the channel and thread, then returns immediately: `history.db` is opened inside the worker.
    ///
    /// An open failure is reported through [`Reply::Unavailable`], and the main thread no longer waits for the handshake on the first frame.
    pub(super) fn new(path: PathBuf) -> Result<Self> {
        let (requests, receiver) = mpsc::channel();
        let (sender, replies) = mpsc::channel();
        let scan_cancellation = CancellationToken::new();
        let cancellation = scan_cancellation.clone();
        let thread = thread::Builder::new()
            .name("pp-history".into())
            .spawn(move || run(path, receiver, sender, cancellation))
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        Ok(Self {
            requests,
            replies,
            scan_cancellation,
            thread: Some(thread),
        })
    }

    pub(super) fn shutdown(&mut self) -> Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        // Cancelling the scan does not stop recv; every append ahead of Shutdown in the FIFO still lands on disk.
        self.scan_cancellation.cancel();
        let sent = self.requests.send(Request::Shutdown);
        let joined = thread.join();
        if sent.is_err() || joined.is_err() {
            return Err(anyhow::anyhow!(UNAVAILABLE));
        }
        Ok(())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if self.shutdown().is_err() {
            tracing::warn!(event = "history_worker_unavailable");
        }
    }
}

/// Worker body: open the database first, then handle requests one by one in FIFO order.
///
/// An open failure is reported once through `Reply::Unavailable`, then requests are only drained until `Shutdown`:
/// once the caller receives that reply it releases the worker, so no request is left waiting and no later append is queued.
fn run(
    path: PathBuf,
    receiver: Receiver<Request>,
    sender: Sender<Reply>,
    cancellation: CancellationToken,
) {
    let sessions = path.with_file_name("sessions");
    let mut store = match Store::open(&path) {
        Ok(store) => store,
        Err(_) => {
            let _ = sender.send(Reply::Unavailable);
            while let Ok(request) = receiver.recv() {
                if matches!(request, Request::Shutdown) {
                    break;
                }
            }
            return;
        }
    };
    while let Ok(request) = receiver.recv() {
        let Some(reply) = reply_to(&mut store, &sessions, &cancellation, request) else {
            break;
        };
        // Even if the reply receiver exits, appends already accepted and ordered before Shutdown must not be dropped.
        let _ = sender.send(reply);
    }
}

/// One request → one reply; `Shutdown` returns `None` and ends the loop.
fn reply_to(
    store: &mut Store,
    sessions: &Path,
    cancellation: &CancellationToken,
    request: Request,
) -> Option<Reply> {
    Some(match request {
        Request::AppendInput {
            cwd,
            text,
            images,
            created_at_ms,
        } => Reply::InputAppended(store.append_input_images(&cwd, &text, &images, created_at_ms)),
        Request::LoadInputs {
            request_id,
            cwd,
            before_id,
        } => Reply::InputsLoaded {
            request_id,
            before_id,
            result: store.load_inputs(&cwd, before_id, INPUT_PAGE_SIZE),
        },
        Request::SyncAndSearch {
            panel_generation,
            request_id,
            cwd,
            query,
        } => {
            let result = store
                .sync_sessions(sessions, cancellation)
                .and_then(|diagnostics| {
                    Ok(SessionListing {
                        sessions: store.search_sessions(cwd.as_deref(), &query)?,
                        diagnostics,
                    })
                });
            Reply::SessionsSearched {
                panel_generation,
                request_id,
                result,
            }
        }
        Request::Search {
            panel_generation,
            request_id,
            cwd,
            query,
        } => Reply::SessionsSearched {
            panel_generation,
            request_id,
            result: store
                .search_sessions(cwd.as_deref(), &query)
                .map(|sessions| SessionListing {
                    sessions,
                    diagnostics: Vec::new(),
                }),
        },
        Request::SetSessionPinned {
            panel_generation,
            request_id,
            id,
            pinned,
        } => Reply::SessionPinned {
            panel_generation,
            request_id,
            result: store.set_session_pinned(&id, pinned),
        },
        Request::Shutdown => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pp_core::cancellation::Cancelled;
    use std::time::Duration;

    #[test]
    fn sqlite_history_worker_shutdown_drains_appends_after_cancelled_scan() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.db");
        let cwd = directory.path().join("project");
        let mut worker = Worker::new(path.clone()).unwrap();
        worker.scan_cancellation.cancel();
        worker
            .requests
            .send(Request::SyncAndSearch {
                panel_generation: 3,
                request_id: 7,
                cwd: Some(cwd.clone()),
                query: String::new(),
            })
            .unwrap();
        for index in 0..150 {
            worker
                .requests
                .send(Request::AppendInput {
                    cwd: cwd.clone(),
                    text: format!("accepted {index}"),
                    images: Vec::new(),
                    created_at_ms: index,
                })
                .unwrap();
        }
        worker.shutdown().unwrap();
        worker.shutdown().unwrap();
        match worker.replies.recv_timeout(Duration::from_secs(1)).unwrap() {
            Reply::SessionsSearched {
                panel_generation: 3,
                request_id: 7,
                result,
            } => assert!(result.err().unwrap().is::<Cancelled>()),
            _ => panic!("expected cancelled scan reply"),
        }
        let store = Store::open(&path).unwrap();
        let entries = store.load_inputs(&cwd, None, 100).unwrap();
        assert_eq!(entries.first().unwrap().text, "accepted 50");
        assert_eq!(entries.last().unwrap().text, "accepted 149");
        let previous = store.load_inputs(&cwd, Some(entries[0].id), 100).unwrap();
        assert_eq!(previous.first().unwrap().text, "accepted 0");
        assert_eq!(previous.last().unwrap().text, "accepted 49");
    }

    #[test]
    fn sqlite_history_worker_load_precedes_local_appends() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("history.db");
        let cwd = directory.path().join("project");
        Store::open(&path)
            .unwrap()
            .append_input_images(&cwd, "previous process", &[], 0)
            .unwrap();
        let mut worker = Worker::new(path.clone()).unwrap();
        worker
            .requests
            .send(Request::LoadInputs {
                request_id: 1,
                cwd: cwd.clone(),
                before_id: None,
            })
            .unwrap();
        worker
            .requests
            .send(Request::AppendInput {
                cwd: cwd.clone(),
                text: "this process".into(),
                images: Vec::new(),
                created_at_ms: 1,
            })
            .unwrap();
        worker.shutdown().unwrap();
        match worker.replies.recv_timeout(Duration::from_secs(1)).unwrap() {
            Reply::InputsLoaded {
                request_id: 1,
                before_id: None,
                result,
            } => {
                let entries = result.unwrap();
                assert_eq!(
                    entries
                        .iter()
                        .map(|entry| entry.text.as_str())
                        .collect::<Vec<_>>(),
                    ["previous process"]
                );
            }
            _ => panic!("expected first input page"),
        }
        let entries = Store::open(&path)
            .unwrap()
            .load_inputs(&cwd, None, 100)
            .unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["previous process", "this process"]
        );
    }

    #[test]
    fn history_worker_reports_unavailable_without_blocking_construction() {
        let directory = tempfile::tempdir().unwrap();
        // The parent path is a regular file: the `history.db` directory can neither be created nor opened.
        let blocker = directory.path().join("blocked");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let mut worker = Worker::new(blocker.join("history.db")).unwrap();
        worker
            .requests
            .send(Request::LoadInputs {
                request_id: 1,
                cwd: blocker.clone(),
                before_id: None,
            })
            .unwrap();
        assert!(matches!(
            worker.replies.recv_timeout(Duration::from_secs(5)).unwrap(),
            Reply::Unavailable
        ));
        // After an open failure only drain requests and never fail again per page: the caller releases the worker on Unavailable.
        assert!(
            worker
                .requests
                .send(Request::LoadInputs {
                    request_id: 2,
                    cwd: blocker,
                    before_id: None,
                })
                .is_ok()
        );
        worker.shutdown().unwrap();
    }
}
