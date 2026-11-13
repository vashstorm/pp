//! 变更记录、撤销与任务边界 glue。
use anyhow::Result;
use pp_core::cancellation::CancellationToken;
use pp_core::events::AgentEvent;
use std::path::PathBuf;
use std::time::Instant;

use super::{ChangesView, POST_SAVE_SYNC, Tools, UndoResult, UndoToken, changes, diff};

impl Tools {
    /// 调用者已完成安全提交，并传入工作区规范相对路径；此处不再读盘。
    pub(super) fn record_committed_file(
        &self,
        call_id: &str,
        path: PathBuf,
        before: changes::Original,
        after: Option<Vec<u8>>,
        resource: bool,
        emit: &mut dyn FnMut(AgentEvent),
    ) {
        let diff = diff::committed(&before, after.as_deref());
        let mut changes = self.changes.lock().unwrap_or_else(|e| e.into_inner());
        if resource {
            changes.record_resource(path.clone(), before, after);
        } else if let Some(after) = after {
            changes.record(path.clone(), before, after);
        }
        changes.source(
            &path,
            self.subagent.as_ref().map_or("Main", |b| b.owner.as_str()),
        );
        drop(changes);
        self.snapshots.borrow_mut().invalidate(&path);
        emit(AgentEvent::FileChanged {
            id: call_id.to_owned(),
            path: path.display().to_string(),
            diff,
        });
    }

    pub(crate) fn begin_changes_task(&self) {
        if self.subagent.as_ref().is_none_or(|b| b.depth == 0) {
            self.changes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .begin();
        }
    }

    pub(crate) fn clear_changes(&self) {
        self.web_cache.borrow_mut().clear();
        self.snapshots.borrow_mut().clear();
        self.changes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub(crate) fn changes_view(&self) -> ChangesView {
        self.changes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .view(&self.workspace)
    }

    pub(crate) fn undo_file(&self, token: &UndoToken) -> Result<UndoResult> {
        let registry = self.lsp_save_registry();
        let absolute = registry.map(|registry| registry.cwd().join(&token.path));
        // 撤销也是一次路径 mutation：旧诊断先失效，且必须在写锁之前完成。
        if let (Some(registry), Some(absolute)) = (registry, &absolute) {
            registry.diagnostics().mutate(absolute);
        }
        let mut result = {
            let _write = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
            let result = self
                .changes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .undo(&self.workspace, token)?;
            self.snapshots.borrow_mut().invalidate(&token.path);
            result
        };
        // 锁已释放；撤销不格式化、不重新诊断，只让已启动客户端看到恢复后的磁盘。
        if let (Some(registry), Some(absolute)) = (registry, &absolute) {
            let warnings = crate::lsp::writethrough::sync_saved(
                registry,
                absolute,
                false,
                Instant::now() + POST_SAVE_SYNC,
                &CancellationToken::new(),
            );
            if !warnings.is_empty() {
                let merged = warnings.join("; ");
                result.warning = Some(match result.warning.take() {
                    Some(existing) => format!("{existing}; {merged}"),
                    None => merged,
                });
            }
        }
        Ok(result)
    }
}
