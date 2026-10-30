use super::Agent;
use crate::{
    cancellation::CancellationToken,
    mcp::{Action, View},
    plan::WorkMode,
};
use anyhow::Result;

impl Agent {
    pub(crate) fn mcp_view(&self) -> View {
        self.tools.mcp.view(self.workflow.mode == WorkMode::Execute)
    }
    pub(crate) fn manage_mcp(
        &mut self,
        action: &Action,
        token: &CancellationToken,
    ) -> Result<View> {
        if !matches!(action, Action::Help | Action::List) {
            self.stop_subagents()?;
        }
        let cwd = self.tools.cwd().to_owned();
        let result =
            self.tools
                .mcp
                .manage(action, &cwd, token, self.workflow.mode == WorkMode::Execute);
        // 部分成功、失败和取消均同步真实工具表；管理命令不追加用户历史。
        self.session
            .set_tools(self.tools.all_definitions(self.workflow.mode));
        if !matches!(action, Action::Help | Action::List) {
            self.journal.tools_changed();
        }
        if result
            .as_ref()
            .is_err_and(|error| error.is::<crate::cancellation::CleanupError>())
        {
            self.plan_cleanup_error =
                Some("Failed to reap MCP processes; exit and restart pp".into());
        }
        result?;
        Ok(self.mcp_view())
    }
}
