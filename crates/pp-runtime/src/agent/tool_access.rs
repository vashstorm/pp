//! 工具侧运行时句柄（LSP、后台清理、临时管道）与用户 shell 入口。
use super::Agent;
use anyhow::{Result, ensure};
use pp_core::cancellation::CancellationToken;
use pp_core::events::AgentEvent;
use pp_tools::web_search::{WebAction, WebOutcome};

pub(in crate::agent) fn execute_web_action(
    client: &mut pp_llm::OpenAiClient,
    action: WebAction<'_>,
    token: &CancellationToken,
) -> Result<WebOutcome> {
    match action {
        WebAction::Read(url) => client.fetch_read_url(url, token).map(WebOutcome::Read),
        WebAction::Search {
            request,
            settings,
            allow_external,
        } => client
            .search_web(request, settings, allow_external, token)
            .map(WebOutcome::Search),
    }
}

impl Agent {
    pub fn bash_timeout_seconds(&self) -> u64 {
        self.tools.limits.bash_timeout_seconds
    }

    pub fn read_tool_result_preview(&self) -> bool {
        self.catalog.read_tool_result_preview()
    }

    /// TUI 后台预热使用的 Registry 克隆；`enabled=false` 时为 `None`。
    /// 克隆共享同一批受管理进程，不转移任何清理权。
    pub fn lsp_registry(&self) -> Option<pp_lsp::Registry> {
        self.tools.lsp_registry().cloned()
    }

    /// TUI/CLI 可持有的共享清理句柄；每次调用返回新的 armed 句柄，
    /// supersede 只能解除同一 Registry 的旧句柄。
    pub fn lsp_cleanup(&self) -> Option<pp_lsp::Cleanup> {
        self.tools.lsp_cleanup()
    }

    /// 显式回收全部 language server；仅 Registry owner 生效，借用父 Registry
    /// 的子 Agent 不会停掉父进程。错误保留 `CleanupError` 类型，调用方必须
    /// 合并进退出错误而不能降级为普通工具失败。
    pub fn stop_lsp(&self) -> Result<()> {
        match self.tools.lsp_cleanup() {
            Some(mut cleanup) => cleanup.stop(),
            None => Ok(()),
        }
    }

    pub fn background_cleanup(&self) -> pp_bash::BackgroundCleanup {
        self.tools.background_cleanup()
    }

    /// 根会话临时管道 registry；子 Agent 共享同一实例。
    pub fn pipe_store(&self) -> pp_bash::PipeStore {
        self.tools.pipe_store()
    }

    pub fn workflow(&self) -> &pp_core::plan::WorkflowState {
        &self.workflow
    }

    /// 进程级 MCP 单例的只读句柄：UI 只看建连进度与 `/mcp` 快照，不拥有回收权。
    pub fn mcp(&self) -> std::sync::Arc<pp_mcp::McpManager> {
        std::sync::Arc::clone(&self.tools.mcp)
    }

    pub fn run_shell(
        &mut self,
        command: &str,
        call_id: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        ensure!(
            self.workflow.mode == pp_core::plan::WorkMode::Execute,
            "Shell commands are unavailable in Plan mode."
        );
        let arguments = serde_json::json!({"command": command}).to_string();
        let result = self.tools.execute_with_fetch(
            "bash",
            &arguments,
            call_id,
            pp_tools::ToolPolicy::Execute {
                workflow: &self.workflow,
            },
            // 人工 shell 不是模型工具调用，永不触发 hook。
            None,
            cancellation,
            emit,
            &mut |url, token| self.client.fetch_read_url(url, token),
        );
        match result.stop_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}
