use crate::tui::{
    App, Level,
    history::{self, Reply, Request},
};
use anyhow::Result;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 固定每页 100 条；与持久输入回忆的翻页约定一致，不新增配置项。
const HISTORY_PAGE: usize = 100;

impl App {
    /// 构造期启动 history worker 并投递第一页 LoadInputs。
    ///
    /// 必须在任何 `remember_text` 之前调用，FIFO 保证启动加载不包含本进程追加。
    /// 返回启动提示：失败时明确只保留进程内回忆，绝不改用其他 HOME。
    pub(in crate::tui) fn start_history(&mut self, path: Option<PathBuf>) -> Option<String> {
        let Some(path) = path else {
            return Some("Input history is not persisted; recall is limited to this run.".into());
        };
        let worker = match history::Worker::new(path) {
            Ok(worker) => worker,
            Err(_) => {
                return Some("Input history is unavailable; recall is limited to this run.".into());
            }
        };
        self.history_request_id = 1;
        let request = Request::LoadInputs {
            request_id: 1,
            cwd: self.history_cwd.clone(),
            before_id: None,
        };
        if worker.requests.send(request).is_err() {
            return Some("Input history is unavailable; recall is limited to this run.".into());
        }
        self.history_page_pending = Some(1);
        self.history_worker = Some(worker);
        None
    }

    /// 关闭 history worker：取消未完成的索引扫描并 join 已排队的追加。
    /// join 后继续排空 FIFO 中的回复：退出前才发现的追加/加载失败也要提示。
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

    /// 排空 history worker 的全部回复；会话搜索结果交给搜索面板安装。
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
            match reply {
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
            }
        }
        true
    }

    /// 发往 history worker 的会话搜索；worker 不可用时只提示，不阻塞 UI。
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

    /// 打开/删除后先同步 JSONL 索引再搜索。
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

    /// 会话面板代次单调递增；关闭后的迟到回复因代次过期被丢弃。
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

    /// 安装一页持久输入；启动页/翻页均前置到本地追加的尾部。
    fn apply_history_page(
        &mut self,
        request_id: u64,
        _before_id: Option<i64>,
        result: Result<Vec<crate::history::InputEntry>>,
    ) {
        // 同一时刻只允许一个加载请求；非当前请求的回复不得重复安装同一页。
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
                let texts: Vec<String> = entries.into_iter().map(|entry| entry.text).collect();
                // 前置会整体右移既有条目，修正当前导航下标。
                if let Some(index) = &mut self.history_index {
                    *index += count;
                }
                self.input_history.splice(0..0, texts);
                if let Some(oldest) = oldest {
                    self.history_oldest_id = Some(oldest);
                }
                self.history_has_more = count == HISTORY_PAGE;
                if self.history_waiting_up {
                    // 完成这次被挂起的 Up：显示刚载入页中最新的那一条。
                    self.history_waiting_up = false;
                    self.show_history(count - 1);
                }
            }
        }
    }

    /// 请求下一页（更老的 100 条）；没有 pending 页时才会发起。
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

    /// Up 触底时等待下一页；重复 Up 合并成同一次等待。
    pub(in crate::tui) fn wait_for_history_page(&mut self) {
        if self.history_waiting_up {
            return;
        }
        // 已经开始浏览历史时 draft 早已捕获，不能被最老一条覆盖。
        if self.history_index.is_none() {
            self.history_draft = Some((self.input.clone(), self.shell_mode));
        }
        self.history_waiting_up = true;
        self.request_history_page();
    }

    /// 回填内存历史中的一条，保留多行与 shell_mode 判定。
    pub(in crate::tui) fn show_history(&mut self, index: usize) {
        if index >= self.input_history.len() {
            return;
        }
        self.file_query = None;
        self.input = crate::tui::editor();
        self.shell_mode = false;
        let text = self.input_history[index].clone();
        self.insert_input(text);
        self.history_index = Some(index);
        // 回填命令时先保持菜单关闭，让上下键可以连续浏览历史。
        self.menu = None;
        self.menu_dismissed = true;
    }
}

/// 排空已结束 worker 的回复；只报告输入追加/加载失败（会话搜索退出后无需提示）。
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

/// 正常退出与析构都保证 join；扫描取消后仍会处理已排队的追加。
impl Drop for App {
    fn drop(&mut self) {
        if let Some(mut worker) = self.history_worker.take() {
            let _ = worker.shutdown();
            // 退出路径不绘制：丢弃剩余回复，但必须等 worker 处理完队列再释放连接。
            let _ = drain_input_failures(&mut worker);
        }
    }
}

/// 时间戳使用整数 UTC 毫秒；系统时钟异常时退化为 0，不伪造未来时间。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

impl App {
    /// 已接受的原始输入统一入口：追加内存历史并投递持久追加请求。
    pub(in crate::tui) fn remember_text(&mut self, text: String) {
        if text.trim().is_empty() {
            return;
        }
        self.history_index = None;
        self.history_draft = None;
        self.history_waiting_up = false;
        self.input_history.push(text.clone());
        let Some(worker) = &self.history_worker else {
            return;
        };
        let request = Request::AppendInput {
            cwd: self.history_cwd.clone(),
            text,
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
    use crate::tui::{App, editor};

    fn page(start: i64, count: i64) -> Vec<crate::history::InputEntry> {
        (start..start + count)
            .map(|id| crate::history::InputEntry {
                id,
                text: format!("prompt-{id}"),
            })
            .collect()
    }

    #[test]
    fn sqlite_input_history_page_prepend_finishes_waiting_up() {
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
        assert_eq!(app.input_history.len(), 101);
        assert_eq!(app.input_history[100], "local oldest");
        assert_eq!(app.history_index, Some(99));
        assert_eq!(app.input.lines(), &["prompt-499"]);
        assert!(!app.history_waiting_up);
        assert!(app.history_page_pending.is_none());
        assert_eq!(app.history_oldest_id, Some(400));
        assert!(app.history_has_more);

        // Down 回到此前显示的最老本地条目；再 Down 恢复空草稿。
        app.navigate_history(false);
        assert_eq!(app.input.lines(), &["local oldest"]);
        app.navigate_history(false);
        assert_eq!(app.input.lines(), &[""]);
    }

    #[test]
    fn sqlite_input_history_initial_pending_up_completes_after_first_page() {
        let mut app = App::new(Err(anyhow::anyhow!("fixture"))).unwrap();
        app.input = editor();
        app.insert_input("draft".into());
        assert_eq!(app.history_page_pending, Some(1));

        // 初始页仍在路上时 Up 挂起一次导航，不为同一页重复发请求。
        app.navigate_history(true);
        assert!(app.history_waiting_up);
        assert!(app.input_history.is_empty());
        assert_eq!(app.history_request_id, 1);

        app.apply_history_page(1, None, Ok(page(0, 3)));
        assert_eq!(app.history_index, Some(2));
        assert_eq!(app.history_oldest_id, Some(0));
        assert_eq!(app.input.lines(), &["prompt-2"]);
        assert!(!app.history_waiting_up);

        // 已到最新一条：Down 还原 Up 前的草稿。
        app.navigate_history(false);
        assert_eq!(app.input.lines(), &["draft"]);
        assert!(app.history_index.is_none());
    }

    #[test]
    fn sqlite_input_history_ignores_stale_pages_and_ends_on_empty_page() {
        let mut app = App::new(Err(anyhow::anyhow!("fixture"))).unwrap();
        app.history_page_pending = Some(9);
        app.history_waiting_up = true;
        // 非当前请求的迟到页不得重复安装。
        app.apply_history_page(8, None, Ok(page(0, 100)));
        assert!(app.input_history.is_empty());
        assert_eq!(app.history_page_pending, Some(9));

        // 空页把 has_more 收尾并结束等待导航。
        app.apply_history_page(9, None, Ok(Vec::new()));
        assert!(!app.history_has_more);
        assert!(!app.history_waiting_up);
        assert!(app.history_page_pending.is_none());
        assert!(app.input_history.is_empty());
    }

    #[test]
    fn sqlite_input_history_load_failure_notifies_once_and_keeps_loaded_pages() {
        let mut app = App::new(Err(anyhow::anyhow!("fixture"))).unwrap();
        app.input_history = vec!["kept".into()];
        app.history_page_pending = Some(3);
        app.history_waiting_up = true;
        app.apply_history_page(3, None, Err(anyhow::anyhow!("fixture unavailable")));
        assert!(app.history_failure_notified);
        assert!(!app.history_waiting_up);
        assert_eq!(app.input_history, ["kept"]);

        app.history_page_pending = Some(4);
        app.apply_history_page(4, None, Err(anyhow::anyhow!("fixture unavailable")));
        assert_eq!(app.input_history, ["kept"]);
    }

    #[test]
    fn sqlite_input_history_remembers_duplicates_and_advances_generations() {
        let mut app = App::new(Err(anyhow::anyhow!("fixture"))).unwrap();
        app.remember_text("repeat".into());
        app.remember_text("repeat".into());
        app.remember_text("  ".into());
        assert_eq!(app.input_history, ["repeat", "repeat"]);

        let first = app.next_session_generation();
        let second = app.next_session_generation();
        assert_eq!(second, first + 1);
        assert!(second > first);
    }
}
