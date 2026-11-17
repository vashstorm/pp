//! 由 Agent 线程生成报告，UI 只读取最近一个稳定边界的内存快照。
//!
//! 分类缓存与报告句柄一起放在 `Snapshot` 内（Agent 持有，不使用进程级静态）：
//! 发布路径按指纹复用分类前缀，`invalidate`／诊断失败丢弃缓存并强制重算。

use super::{Agent, ContextReport, DiagnosisCache};
use pp_core::context::ContextUsage;
use std::sync::{Arc, Mutex};

type Report = Result<Arc<ContextReport>, String>;

#[derive(Clone, Default)]
pub struct Snapshot {
    report: Arc<Mutex<Option<Report>>>,
    cache: Arc<Mutex<DiagnosisCache>>,
}

impl Snapshot {
    /// 只复制报告句柄；不等待主任务，也不进行诊断计算或 I/O。
    pub fn read(&self) -> Option<Report> {
        self.report
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 丢弃已缓存的分类前缀；下一次发布强制全量重算。
    pub fn invalidate(&self) {
        self.cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .invalidate();
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
        self.publish_context_snapshot_with_usage(None);
    }

    /// 复用本轮请求前已算好的用量；`None` 时在发布路径重新估算。
    ///
    /// 历史在用量估算之后发生变化（例如补充 developer 消息）时调用方必须传 `None`。
    pub fn publish_context_snapshot_with_usage(&self, usage: Option<ContextUsage>) {
        let Some(snapshot) = &self.context_snapshot else {
            return;
        };
        let report = {
            let mut cache = snapshot.cache.lock().unwrap_or_else(|e| e.into_inner());
            self.diagnose_context_with(&mut cache, usage)
        }
        .map(Arc::new)
        .map_err(|error| {
            // 失败不保留半份缓存，避免下一次发布复用与错误相关的旧前缀。
            snapshot.invalidate();
            pp_core::text::display_text(&format!("{error:#}")).into_owned()
        });
        *snapshot.report.lock().unwrap_or_else(|e| e.into_inner()) = Some(report);
    }
}
