//! Multi-line startup MCP connect hint state: per-server progress plus the final overall
//! status, rendered by the welcome page.
//!
//! The runtime connects on a background thread; this only polls an in-memory snapshot, so
//! drawing and the event loop never touch the network.
use crate::tui::App;

impl App {
    /// Returns `true` when the connect progress changed so the main loop redraws; the wording
    /// and timings come solely from the runtime.
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

    /// Yields after the first task is submitted: the overall status is a startup hint only and
    /// never occupies the interface permanently.
    pub(in crate::tui) fn dismiss_mcp_tip(&mut self) {
        self.mcp_tip_dismissed = true;
        self.mcp_tip.clear();
    }
}
