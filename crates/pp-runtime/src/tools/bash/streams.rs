//! 管道阶段镜像与临时正文保留（`PipeStore`）。
//!
//! 该模块是 plan §2.4–2.6 与 §3.1–3.4 的存储层：每次 bash 调用创建一个 run，
//! run 内观察到的多阶段管道、每条边的阶段状态和真实流经字节都保存在根会话共享
//! 的一棵有界 registry 里。它不依赖解释器实现细节，只实现
//! [`brush_core::PipelineObserver`] 并通过 [`relay`](super::relay) 创建进程内管道。
//!
//! 有界规则（全部为内部常量，不暴露设置）：
//! - 每流只保留最新 1 MiB 原始字节；
//! - 每 run 最多 16 条缓存流，流关闭后只保留最近 4 条已关闭流；
//! - 根会话合计最多 64 条缓存流 / 64 MiB 正文，最近 8 个已完成且有管道的 run；
//! - 每 run 保留最近 16 个 pipeline occurrence，每个最多 32 个阶段明细；
//! - 明细被淘汰不会丢掉“观察到过非零阶段”这一事实，计数始终累计；
//! - 淘汰优先最老已完成 run / 最老已关闭流，永不淘汰 open 流；没有可淘汰槽位时
//!   该边直接不捕获（`untapped_edges`），命令仍按普通管道执行。

/// 单条流的保留正文上限。
pub(crate) const STREAM_RING_CAP: usize = 1024 * 1024;
const MAX_STREAMS_PER_RUN: usize = 16;
const MAX_CLOSED_STREAMS_PER_RUN: usize = 4;
const MAX_TOTAL_STREAMS: usize = 64;
const MAX_TOTAL_BODY: usize = 64 * 1024 * 1024;
/// 已完成仍有管道的 run 保留上限；后台任务回归也按此边界构造用例。
pub(super) const MAX_COMPLETED_RUNS: usize = 8;
/// 已完成但没有管道的 run 仍然可被 `/pipes`/状态查询看到（例如宿主启动失败），
/// 但单独限界，绝不挤占“最近 8 个有管道的 run”历史。
const MAX_EMPTY_RUNS: usize = 8;
const MAX_PIPELINES_PER_RUN: usize = 16;
const MAX_STAGES_PER_PIPELINE: usize = 32;
const MAX_LABEL_BYTES: usize = 512;
/// 卡片只复制最近一小段正文，避免每次刷新复制整个流缓存。
const PREVIEW_BYTES: u64 = 50 * 1024;

/// 临时管道 URI 的 scheme。
pub(crate) const PIPE_URI_PREFIX: &str = "pipe://";
/// 未知、已淘汰或来自另一会话时的固定提示；空流绝不能伪装成这个状态。
pub(crate) const PIPES_UNAVAILABLE: &str =
    "Temporary pipe output is unavailable (evicted or from another session).";

mod observer;
mod ring;
mod run;
mod store;
mod stream;

pub use observer::PipeRun;
pub(super) use ring::CloseReason;
pub(crate) use run::RunEnd;
pub use store::PipeStore;
pub(super) use stream::StreamState;
#[cfg(test)]
pub(super) use stream::detached_stream;

#[cfg(test)]
mod fixtures {
    use brush_core::openfiles::OpenFile;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::observer::PipeRun;
    use super::store::{PipeStore, PipeStoreError};

    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    static NEXT: AtomicU64 = AtomicU64::new(1);

    pub(super) fn unique_run(store: &PipeStore) -> PipeRun {
        loop {
            let id = format!("run{:032x}", NEXT.fetch_add(1, Ordering::Relaxed));
            match store.start_run(id, "test") {
                Ok(run) => return run,
                Err(PipeStoreError::DuplicateRun) => {}
                Err(error) => panic!("unexpected start_run error: {error}"),
            }
        }
    }

    pub(super) fn edge(run: &PipeRun, pipeline: u64, index: usize) -> (OpenFile, OpenFile) {
        run.observer()
            .pipe(pipeline, index, "printf", "cat")
            .expect("capture edge")
            .expect("edge must be captured")
    }

    pub(super) fn producer_writer(open: OpenFile) -> std::io::PipeWriter {
        let OpenFile::PipeWriter(writer) = open else {
            panic!("producer must be a pipe writer");
        };
        writer
    }

    pub(super) fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + TEST_TIMEOUT;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        false
    }
}
