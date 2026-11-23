//! 启动期后台建连的进度快照与提示文案。
//!
//! 建连在 `McpManager` 的后台线程里进行，UI 与 CLI 只读取这里的快照，
//! 不在绘制或事件循环中触网；提示文案由 runtime 唯一实现，避免两处漂移。

use pp_core::text::display_text;
use std::time::{Duration, Instant};

/// 单个服务的建连阶段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectPhase {
    Pending,
    Connecting,
    Connected,
    Failed,
}

/// 单个服务的建连结果；`elapsed` 在建连结束后冻结。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerConnect {
    pub name: String,
    pub phase: ConnectPhase,
    pub elapsed: Option<Duration>,
    pub error: Option<String>,
}

impl ServerConnect {
    /// 单服务结果行（提示行与行模式逐条输出共用）：`context7 connected (874ms) ✓`。
    pub fn describe(&self) -> String {
        let elapsed = self
            .elapsed
            .map_or_else(|| "(?)".to_string(), duration_text);
        match self.phase {
            ConnectPhase::Pending => format!("{} pending", self.name),
            ConnectPhase::Connecting => format!("{} connecting…", self.name),
            ConnectPhase::Connected => format!("{} connected ({elapsed}) ✓", self.name),
            ConnectPhase::Failed => format!("{} failed ({elapsed}) ✗", self.name),
        }
    }

    /// 失败服务的脱敏原因；未失败或无具体原因时返回 `None`。
    pub fn reason(&self) -> Option<&str> {
        (self.phase == ConnectPhase::Failed)
            .then_some(self.error.as_deref())
            .flatten()
            .filter(|reason| !reason.is_empty())
    }
}

/// `McpManager::connect_snapshot` 的只读投影：不持锁、不触发连接。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectSnapshot {
    pub servers: Vec<ServerConnect>,
    pub finished: bool,
    pub elapsed: Duration,
    pub error: Option<String>,
}

impl ConnectSnapshot {
    /// 单行提示：建连中显示已完成服务的耗时与当前服务，结束后显示总状态。
    /// 没有配置任何服务时返回 `None`，调用方不保留空行。
    pub fn tip(&self) -> Option<String> {
        if let Some(error) = &self.error {
            return Some(format!("MCP unavailable: {error}"));
        }
        if self.servers.is_empty() {
            return None;
        }
        if !self.finished {
            let parts: Vec<String> = self.servers.iter().map(ServerConnect::describe).collect();
            return Some(format!("MCP: {}", parts.join(", ")));
        }
        let succeeded = self
            .servers
            .iter()
            .filter(|server| server.phase == ConnectPhase::Connected)
            .count();
        let failed = self
            .servers
            .iter()
            .filter(|server| server.phase == ConnectPhase::Failed)
            .count();
        let total = duration_text(self.elapsed);
        if failed == 0 {
            Some(format!(
                "Connected to MCP servers: {succeeded} succeeded ({total})"
            ))
        } else {
            Some(format!(
                "Connected to MCP servers: {succeeded} succeeded, {failed} failed ({total})"
            ))
        }
    }

    /// 多行提示：每个服务单独占一行（不覆盖前序状态），失败服务在下一行缩进显示原因，全部结束后追加总状态行。
    /// 没有配置任何服务时返回空列表，调用方不保留空行。
    pub fn tip_lines(&self) -> Vec<String> {
        if let Some(error) = &self.error {
            return vec![format!("MCP unavailable: {error}")];
        }
        if self.servers.is_empty() {
            return Vec::new();
        }
        let mut lines = Vec::new();
        for server in &self.servers {
            lines.push(format!("MCP: {}", server.describe()));
            if let Some(reason) = server.reason() {
                lines.push(format!("  {reason}"));
            }
        }
        if self.finished
            && let Some(summary) = self.tip()
        {
            lines.push(summary);
        }
        lines
    }
}

fn sanitize_reason(message: &str) -> Option<String> {
    let cleaned = display_text(message);
    let collapsed: String = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

fn duration_text(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{}ms", duration.as_millis())
    } else {
        format!("{:.1}s", duration.as_secs_f64())
    }
}

/// 后台建连的可变进度；只在 `State` 锁内更新，快照在锁外使用。
#[derive(Debug)]
pub(crate) struct Progress {
    servers: Vec<ServerConnect>,
    started: Option<Instant>,
    total: Option<Duration>,
    finished: bool,
    // 已向行模式输出过总状态：同一进程的后续任务不重复打印。
    reported: bool,
    // 逐服务已报告游标：只前进不后退，新一轮 begin/reserve 重置。
    claimed: usize,
    error: Option<String>,
}

impl Default for Progress {
    // 未启动建连的实例（测试、mock、无配置）视为已完成，`await_connect` 立即返回。
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            started: None,
            total: None,
            finished: true,
            reported: false,
            claimed: 0,
            error: None,
        }
    }
}

impl Progress {
    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    /// 起线程前同步登记“本轮进行中”：`await_connect` 不能在 worker 还没登记时穿透。
    pub(crate) fn reserve(&mut self) {
        self.servers.clear();
        self.started = Some(Instant::now());
        self.total = None;
        self.reported = false;
        self.claimed = 0;
        self.error = None;
        self.finished = false;
    }

    /// 登记本轮目标服务并清空上一轮结果；空列表立即结束。
    pub(crate) fn begin(&mut self, names: Vec<String>) {
        self.servers = names
            .into_iter()
            .map(|name| ServerConnect {
                name,
                phase: ConnectPhase::Pending,
                elapsed: None,
                error: None,
            })
            .collect();
        self.started = Some(Instant::now());
        self.total = None;
        self.reported = false;
        self.claimed = 0;
        self.error = None;
        self.finished = self.servers.is_empty();
        if self.finished {
            self.total = Some(Duration::ZERO);
        }
    }

    pub(crate) fn connecting(&mut self, name: &str) {
        if let Some(server) = self.servers.iter_mut().find(|server| server.name == name) {
            server.phase = ConnectPhase::Connecting;
        }
    }

    pub(crate) fn complete(
        &mut self,
        name: &str,
        connected: bool,
        elapsed: Duration,
        error: Option<&str>,
    ) {
        if let Some(server) = self.servers.iter_mut().find(|server| server.name == name) {
            server.phase = if connected {
                ConnectPhase::Connected
            } else {
                ConnectPhase::Failed
            };
            server.elapsed = Some(elapsed);
            server.error = if connected {
                None
            } else {
                error.and_then(sanitize_reason)
            };
        }
    }

    /// 配置加载失败等整体故障：结束本轮，提示行显示脱敏原因。
    pub(crate) fn fail(&mut self, message: &str) {
        self.error = Some(display_text(message).into_owned());
        self.finish();
    }

    /// 尚未开始建连的目标：调用方据此把条目状态复位为可重试。
    pub(crate) fn unfinished(&self) -> Vec<String> {
        self.servers
            .iter()
            .filter(|server| {
                matches!(
                    server.phase,
                    ConnectPhase::Pending | ConnectPhase::Connecting
                )
            })
            .map(|server| server.name.clone())
            .collect()
    }

    pub(crate) fn finish(&mut self) {
        for server in &mut self.servers {
            if matches!(
                server.phase,
                ConnectPhase::Pending | ConnectPhase::Connecting
            ) {
                server.phase = ConnectPhase::Failed;
            }
        }
        self.total = Some(
            self.started
                .map_or(Duration::ZERO, |started| started.elapsed()),
        );
        self.finished = true;
    }

    pub(crate) fn snapshot(&self) -> ConnectSnapshot {
        let elapsed = self.total.unwrap_or_else(|| {
            self.started
                .map_or(Duration::ZERO, |started| started.elapsed())
        });
        ConnectSnapshot {
            servers: self.servers.clone(),
            finished: self.finished,
            elapsed,
            error: self.error.clone(),
        }
    }

    /// 行模式一次性领取新完成的连续服务：已领取的不再重复，被取消打断后继续领取
    /// 也不会重打已输出过的行。遇到尚未结束的服务即停，保持输出顺序连续。
    pub(crate) fn take_claimed(&mut self) -> Vec<ServerConnect> {
        let mut claimed = Vec::new();
        while let Some(server) = self.servers.get(self.claimed) {
            if server.elapsed.is_none() {
                break;
            }
            claimed.push(server.clone());
            self.claimed += 1;
        }
        claimed
    }

    /// 行模式一次性取走总状态：同一进程的后续任务不重复打印。
    pub(crate) fn take_report(&mut self) -> Option<String> {
        if !self.finished || self.reported {
            return None;
        }
        self.reported = true;
        self.snapshot().tip()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn tip_reports_each_server_elapsed_and_the_final_total() {
        let mut progress = Progress::default();
        assert_eq!(progress.snapshot().tip(), None);
        assert!(progress.snapshot().tip_lines().is_empty());
        progress.begin(names(&["context7", "exa"]));
        progress.connecting("context7");
        assert_eq!(
            progress.snapshot().tip().as_deref(),
            Some("MCP: context7 connecting…, exa pending")
        );
        assert_eq!(
            progress.snapshot().tip_lines(),
            vec![
                "MCP: context7 connecting…".to_string(),
                "MCP: exa pending".to_string(),
            ]
        );
        progress.complete("context7", true, Duration::from_millis(874), None);
        progress.connecting("exa");
        assert_eq!(
            progress.snapshot().tip().as_deref(),
            Some("MCP: context7 connected (874ms) ✓, exa connecting…")
        );
        assert_eq!(
            progress.snapshot().tip_lines(),
            vec![
                "MCP: context7 connected (874ms) ✓".to_string(),
                "MCP: exa connecting…".to_string(),
            ]
        );
        progress.complete(
            "exa",
            false,
            Duration::from_millis(1200),
            Some("starting MCP\n\x1b[31mprocess failed"),
        );
        progress.finish();
        let tip = progress.snapshot().tip().expect("已配置服务必须有总状态");
        assert!(
            tip.starts_with("Connected to MCP servers: 1 succeeded, 1 failed (")
                && tip.ends_with(')'),
            "{tip}"
        );
        let lines = progress.snapshot().tip_lines();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "MCP: context7 connected (874ms) ✓");
        assert_eq!(lines[1], "MCP: exa failed (1.2s) ✗");
        assert_eq!(lines[2], "  starting MCP \u{fffd}[31mprocess failed");
        assert_eq!(lines[3], tip);
    }

    #[test]
    fn tip_omits_failed_count_when_all_servers_succeed_and_handles_all_failed() {
        let mut progress = Progress::default();
        progress.begin(names(&["context7", "exa"]));
        progress.complete("context7", true, Duration::from_millis(20), None);
        progress.complete("exa", true, Duration::from_millis(900), None);
        progress.finish();
        let tip = progress.snapshot().tip().expect("已配置服务必须有总状态");
        assert!(
            tip.starts_with("Connected to MCP servers: 2 succeeded (") && !tip.contains("failed"),
            "{tip}"
        );

        progress.begin(names(&["local"]));
        progress.connecting("local");
        progress.complete(
            "local",
            false,
            Duration::from_millis(30),
            Some("MCP request timed out"),
        );
        progress.finish();
        assert_eq!(
            progress.snapshot().tip().as_deref(),
            Some("Connected to MCP servers: 0 succeeded, 1 failed (0ms)")
        );
        assert_eq!(
            progress.snapshot().tip_lines(),
            vec![
                "MCP: local failed (30ms) ✗".to_string(),
                "  MCP request timed out".to_string(),
                "Connected to MCP servers: 0 succeeded, 1 failed (0ms)".to_string(),
            ]
        );

        progress.begin(names(&["local"]));
        progress.fail("invalid MCP config");
        assert_eq!(
            progress.snapshot().tip().as_deref(),
            Some("MCP unavailable: invalid MCP config")
        );
        assert_eq!(
            progress.snapshot().tip_lines(),
            vec!["MCP unavailable: invalid MCP config".to_string()]
        );
        assert!(progress.is_finished());
    }

    #[test]
    fn unfinished_targets_are_reported_for_retry() {
        let mut progress = Progress::default();
        progress.begin(names(&["a", "b"]));
        progress.connecting("a");
        progress.complete("a", true, Duration::from_millis(10), None);
        assert_eq!(progress.unfinished(), vec!["b".to_string()]);
        progress.finish();
        assert!(progress.unfinished().is_empty());
    }

    #[test]
    fn claimed_services_are_reported_once_and_only_while_contiguous() {
        let mut progress = Progress::default();
        progress.begin(names(&["a", "b", "c"]));
        // 后一个服务先完成也不越过尚未完成的前一个：输出顺序与配置顺序一致。
        progress.complete("b", true, Duration::from_millis(20), None);
        assert!(progress.take_claimed().is_empty());
        progress.complete("a", true, Duration::from_millis(10), None);
        let claimed: Vec<String> = progress
            .take_claimed()
            .into_iter()
            .map(|server| server.name)
            .collect();
        assert_eq!(claimed, names(&["a", "b"]));
        // 已领取的不再重复：取消后继续领取同一轮不会重打。
        assert!(progress.take_claimed().is_empty());
        progress.complete("c", true, Duration::from_millis(30), None);
        progress.finish();
        let claimed: Vec<String> = progress
            .take_claimed()
            .into_iter()
            .map(|server| server.name)
            .collect();
        assert_eq!(claimed, names(&["c"]));
        assert!(progress.take_claimed().is_empty());
        // 新一轮 begin 重置游标：新完成的服务重新可领取。
        progress.begin(names(&["a"]));
        progress.complete("a", true, Duration::from_millis(5), None);
        assert_eq!(progress.take_claimed().len(), 1);
        assert!(progress.take_claimed().is_empty());
    }
}
