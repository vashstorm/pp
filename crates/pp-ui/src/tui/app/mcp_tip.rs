//! 启动期 MCP 建连的多行提示状态：逐服务进度与最终总状态，由欢迎页渲染。
//!
//! 建连由 runtime 在后台线程完成，这里只轮询内存快照：绘制与事件循环不触网。
use crate::tui::App;

impl App {
    /// 建连进度变化时返回 `true`，让主循环重绘；文案与耗时由 runtime 唯一提供。
    pub(in crate::tui) fn mcp_tip_tick(&mut self) -> bool {
        if self.mcp_tip_dismissed {
            if self.mcp_tip.is_empty() {
                return false;
            }
            self.mcp_tip.clear();
            return true;
        }
        let lines = self
            .bootstrap
            .as_ref()
            .map(|bootstrap| bootstrap.mcp.connect_snapshot().tip_lines())
            .unwrap_or_default();
        if lines == self.mcp_tip {
            return false;
        }
        self.mcp_tip = lines;
        true
    }

    /// 首个任务提交后让出：总状态只作为启动提示，不常驻占用界面。
    pub(in crate::tui) fn dismiss_mcp_tip(&mut self) {
        self.mcp_tip_dismissed = true;
        self.mcp_tip.clear();
    }
}
