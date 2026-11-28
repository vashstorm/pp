//! `/mcp` management command entry point: takes control and hands off to lifecycle.

use anyhow::Result;
use pp_core::cancellation::CancellationToken;

use super::{Access, Action, McpManager};

impl McpManager {
    pub fn manage(&self, action: &Action, token: &CancellationToken, access: Access) -> Result<()> {
        let _control = self.lock_control();
        self.ensure_loaded()?;
        self.manage_locked(action, token, access)
    }
}
