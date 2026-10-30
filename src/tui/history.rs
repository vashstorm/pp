//! 输入回忆和派生搜索使用独立线程，不被模型请求 worker 占用。
use crate::{
    agent::persistence::SessionListing,
    cancellation::CancellationToken,
    history::{InputEntry, Store},
};
use anyhow::Result;
use std::{
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle},
};

const UNAVAILABLE: &str = "History database unavailable.";
const INPUT_PAGE_SIZE: usize = 100;

pub(super) enum Request {
    AppendInput {
        cwd: PathBuf,
        text: String,
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
    Shutdown,
}

pub(super) enum Reply {
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
}

pub(super) struct Worker {
    pub(super) requests: Sender<Request>,
    pub(super) replies: Receiver<Reply>,
    scan_cancellation: CancellationToken,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub(super) fn new(path: PathBuf) -> Result<Self> {
        let (requests, receiver) = mpsc::channel();
        let (sender, replies) = mpsc::channel();
        let (ready_sender, ready) = mpsc::sync_channel(1);
        let scan_cancellation = CancellationToken::new();
        let cancellation = scan_cancellation.clone();
        let thread = thread::Builder::new()
            .name("pp-history".into())
            .spawn(move || {
                let opened = Store::open(&path);
                let mut store = match opened {
                    Ok(store) => {
                        let _ = ready_sender.send(Ok(()));
                        store
                    }
                    Err(error) => {
                        let _ = ready_sender.send(Err(error));
                        return;
                    }
                };
                let sessions = path.with_file_name("sessions");
                while let Ok(request) = receiver.recv() {
                    let reply = match request {
                        Request::AppendInput {
                            cwd,
                            text,
                            created_at_ms,
                        } => Reply::InputAppended(store.append_input(&cwd, &text, created_at_ms)),
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
                            let result = store.sync_sessions(&sessions, &cancellation).and_then(
                                |diagnostics| {
                                    Ok(SessionListing {
                                        sessions: store.search_sessions(cwd.as_deref(), &query)?,
                                        diagnostics,
                                    })
                                },
                            );
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
                        Request::Shutdown => break,
                    };
                    // 回复接收者退出也不能丢弃已接受、排在 Shutdown 前的 append。
                    let _ = sender.send(reply);
                }
            })
            .map_err(|_| anyhow::anyhow!(UNAVAILABLE))?;
        match ready.recv() {
            Ok(Ok(())) => Ok(Self {
                requests,
                replies,
                scan_cancellation,
                thread: Some(thread),
            }),
            result => {
                let _ = thread.join();
                match result {
                    Ok(Err(error)) => Err(error),
                    _ => Err(anyhow::anyhow!(UNAVAILABLE)),
                }
            }
        }
    }

    pub(super) fn shutdown(&mut self) -> Result<()> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        // 取消扫描但不终止 recv；FIFO 的 Shutdown 之前所有 append 仍会落盘。
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cancellation::Cancelled;
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
            .append_input(&cwd, "previous process", 0)
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
}
