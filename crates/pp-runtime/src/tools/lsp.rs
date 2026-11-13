//! `Tools` 的 LSP 接线与保存联动。
use anyhow::Result;
use async_openai::types::chat::FunctionObject;
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::Cancelled;
use pp_core::cancellation::CleanupError;
use pp_core::events::AgentEvent;
use serde_json::{Value, json};
use std::path::Path;
use std::time::Instant;

use super::POST_SAVE_SYNC;
use super::Tools;

impl Tools {
    pub(crate) fn lsp_registry(&self) -> Option<&crate::lsp::Registry> {
        self.lsp.as_ref()
    }

    /// 根/独立 owner 安装 Registry：保留退出清理权。`enabled=false` 时既
    /// 不暴露工具也不持有清理，完全跳过发现与投递。
    pub(crate) fn install_lsp(&mut self, registry: crate::lsp::Registry) {
        if !registry.enabled() {
            self.lsp = None;
            self.lsp_cleanup = None;
            return;
        }
        let cleanup = registry.cleanup();
        // 同一 Registry 重装（Plan↔Execute 保留客户端）只移交清理权；
        // 换成另一个 Registry 时旧句柄在离开作用域时回收旧进程。
        if let Some(mut previous) = self.lsp_cleanup.take() {
            previous.supersede(&cleanup);
        }
        self.lsp_cleanup = Some(cleanup);
        self.lsp = Some(registry);
    }

    /// 子 Agent 借用父 Registry：共享同一批客户端，但永不取得停止父进程的
    /// 清理权，Drop 也不会回收父服务器。
    pub(crate) fn install_shared_lsp(&mut self, registry: crate::lsp::Registry) {
        debug_assert!(
            self.lsp_cleanup.is_none(),
            "child tools must not own an LSP cleanup handle"
        );
        self.lsp = registry.enabled().then_some(registry);
    }

    /// 向 TUI 等外部持有者发放新的清理句柄；只有 owner 安装后才能取得。
    /// 同一 Registry 的多个句柄由 `Cleanup::supersede` 移交，不会重复回收。
    pub(crate) fn lsp_cleanup(&self) -> Option<crate::lsp::Cleanup> {
        self.lsp_cleanup.as_ref()?;
        self.lsp.as_ref().map(crate::lsp::Registry::cleanup)
    }

    pub(crate) fn set_lsp_readonly(&mut self, readonly: bool) {
        self.lsp_readonly = readonly;
    }

    /// LSP 工具 schema：只有已安装 Registry 时才出现；受限模式同时裁掉
    /// schema 与执行层动作，隐藏 schema 从不作为授权边界。
    pub(super) fn lsp_definition(&self, read_only: bool) -> Option<FunctionObject> {
        self.lsp
            .as_ref()
            .map(|_| crate::lsp::tool::definition(read_only))
    }

    /// 本会话可自动联动的 LSP Registry：禁用与只读 session 都不介入普通写入。
    pub(super) fn lsp_save_registry(&self) -> Option<&crate::lsp::Registry> {
        self.lsp
            .as_ref()
            .filter(|registry| registry.enabled() && !self.lsp_readonly)
    }

    /// 提交之后同步已启动客户端：只更新 overlay/通知，失败只作为警告。
    fn sync_after_save(
        &self,
        path: &Path,
        created: bool,
        cancellation: &CancellationToken,
    ) -> Vec<String> {
        let Some(registry) = self.lsp_save_registry() else {
            return Vec::new();
        };
        crate::lsp::writethrough::sync_saved(
            registry,
            path,
            created,
            Instant::now() + POST_SAVE_SYNC,
            cancellation,
        )
    }

    /// 最终提交且 diff 已发布之后：同步客户端并按设置采集内联诊断。
    ///
    /// 返回普通警告；`Cancelled`/`CleanupError` 仍保留类型向上传播。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_lsp_save(
        &self,
        call_id: &str,
        origin: &str,
        path: &Path,
        created: bool,
        ticket: Option<crate::lsp::writethrough::Ticket>,
        formatter: Option<pp_core::lsp_report::FormatterStatus>,
        diagnostics: bool,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
        result: &mut Value,
    ) -> Result<Vec<String>> {
        let Some(registry) = self.lsp_save_registry() else {
            return Ok(Vec::new());
        };
        let (Some(ticket), true) = (ticket, diagnostics) else {
            return Ok(self.sync_after_save(path, created, cancellation));
        };
        let mut warnings = Vec::new();
        match registry.diagnostics().collect_after_save(
            registry,
            &self.lsp_owner,
            ticket,
            origin,
            created,
            formatter,
        ) {
            Ok(Some(report)) => {
                // diff 已在上一步发布；内联诊断事件只负责展示同一份报告。
                emit(AgentEvent::LspDiagnostics {
                    id: Some(call_id.to_owned()),
                    report: report.clone(),
                });
                result["diagnostics"] = json!(&report);
                result["content"] = json!(crate::lsp::diagnostics::render(
                    &report,
                    Some(crate::lsp::diagnostics::AUTO_MESSAGE_LIMIT),
                ));
            }
            Ok(None) => warnings.push(format!(
                "Diagnostics for {} are still running in the background; a late report will follow.",
                path.display()
            )),
            Err(error) if is_control_lsp_error(&error) => return Err(error),
            Err(error) => warnings.push(format!("Diagnostics were not collected: {error:#}")),
        }
        Ok(warnings)
    }
}

/// 控制流错误必须保留类型；普通诊断失败只作为状态说明。
fn is_control_lsp_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<Cancelled>() || cause.is::<CleanupError>())
}
