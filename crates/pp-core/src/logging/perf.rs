//! 默认关闭的启动阶段计时。
//!
//! `PP_PERF_TRACE=1` 时每个采样边界向 stderr 写一行 `phase=<name> micros=<n>`，
//! 用于 release 构建的可重复基线；未设置时零输出、零分配，只做一次环境判定。
//! 这里只持有环境判定与起点锚点（`OnceLock` 不承载业务状态），也不写入
//! `~/.pp/logs` 的诊断日志契约。

use std::{
    io::{self, Write},
    sync::OnceLock,
    time::Instant,
};

const TRACE_ENV: &str = "PP_PERF_TRACE";

fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED
        .get_or_init(|| std::env::var_os(TRACE_ENV).is_some_and(|value| value.as_os_str() == "1"))
}

/// 进程起点锚点：`main` 的第一条语句提交 `mark("start")`，之后的
/// `micros` 都是相对该点的耗时。
fn start() -> &'static Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now)
}

/// 记录一个阶段边界；同名阶段重复出现时按调用顺序各写一行，不做聚合。
pub fn mark(phase: &'static str) {
    if !enabled() {
        return;
    }
    let micros = start().elapsed().as_micros();
    let _ = writeln!(io::stderr().lock(), "phase={phase} micros={micros}");
}
