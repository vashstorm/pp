//! 输入回忆和派生搜索使用独立线程，不被模型请求 worker 占用。
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
    /// `history.db` 打开失败：主线程不再阻塞等待握手，改由本条回复告知并停用持久回忆。
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
    /// 只构造通道与线程后立即返回：`history.db` 在 worker 内打开。
    ///
    /// 打开失败经 [`Reply::Unavailable`] 回报，主线程不再为首帧等待握手。
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

/// worker 主体：先打开数据库，再按 FIFO 逐条处理请求。
///
/// 打开失败经 `Reply::Unavailable` 报告一次，然后只排空请求直到 `Shutdown`：
/// 调用方收到该回复即释放 worker，不会有请求悬空等待，也不会有后续追加被排队。
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
        // 回复接收者退出也不能丢弃已接受、排在 Shutdown 前的 append。
        let _ = sender.send(reply);
    }
}

/// 单条请求 → 单条回复；`Shutdown` 返回 `None` 结束循环。
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
        // 父路径是普通文件：`history.db` 的目录既不能创建也不能打开。
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
        // 打开失败后只排空请求、不再回失败页：调用方收到 Unavailable 即释放 worker。
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
