//! `/mcp` 管理命令入口：持有 control 后交给 lifecycle。

use anyhow::Result;
use pp_core::cancellation::CancellationToken;

use super::{Access, Action, McpManager};

impl McpManager {
    pub(crate) fn manage(
        &self,
        action: &Action,
        token: &CancellationToken,
        access: Access,
    ) -> Result<()> {
        let _control = self.lock_control();
        self.ensure_loaded()?;
        self.manage_locked(action, token, access)
    }
}
