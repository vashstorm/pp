//! 由 Agent 线程生成报告，UI 只读取最近一个稳定边界的内存快照。

use super::{Agent, ContextReport};
use std::sync::{Arc, Mutex};

type Report = Result<Arc<ContextReport>, String>;

#[derive(Clone, Default)]
pub struct Snapshot {
    report: Arc<Mutex<Option<Report>>>,
}

impl Snapshot {
    /// 只复制报告句柄；不等待主任务，也不进行诊断计算或 I/O。
    pub fn read(&self) -> Option<Report> {
        self.report
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Agent {
    /// 为 TUI 启用快照；行模式与子 Agent 默认不生成额外诊断报告。
    pub fn observe_context(&mut self) -> Snapshot {
        let snapshot = Snapshot::default();
        self.context_snapshot = Some(snapshot.clone());
        snapshot
    }

    /// 只在 Agent 所在线程调用；诊断失败不改变任务结果。
    pub fn publish_context_snapshot(&self) {
        let Some(snapshot) = &self.context_snapshot else {
            return;
        };
        let report = self
            .diagnose_context()
            .map(Arc::new)
            .map_err(|error| pp_core::text::display_text(&format!("{error:#}")).into_owned());
        *snapshot.report.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
    }
}
