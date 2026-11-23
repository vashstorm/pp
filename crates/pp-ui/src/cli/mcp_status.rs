//! 行模式与 `-p` 的启动期 MCP 建连提示：逐服务滚动一行，最后给总状态。
//!
//! 建连由 runtime 在后台线程完成，这里只轮询内存快照；文案与耗时同样来自 runtime。
use pp_runtime::agent::Agent;
use std::io::Write;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(25);

// 逐条输出本进程尚未打印过的已完成服务；由 manager 的快照游标去重。
fn write_claimed(manager: &pp_mcp::McpManager, stderr: &mut impl Write) {
    for server in manager.take_connect_services() {
        let _ = writeln!(stderr, "MCP: {}", server.describe());
        if let Some(reason) = server.reason() {
            let _ = writeln!(stderr, "  {reason}");
        }
    }
}

/// 等启动期建连结束（或在收到取消信号时提前退出），期间把新完成的服务逐条写到 stderr。
/// 已打印的服务由 manager 的快照游标去重：同一进程的后续任务与取消后的重试都不会重打。
/// 提示不影响任务结果：写失败（例如管道断开）被忽略，不改变任务输出。
pub(crate) fn wait(
    agent: &Agent,
    stderr: &mut impl Write,
    cancellation: Option<&pp_core::cancellation::CancellationToken>,
) {
    let manager = agent.mcp();
    loop {
        if cancellation.is_some_and(pp_core::cancellation::CancellationToken::is_cancelled) {
            return;
        }
        write_claimed(&manager, stderr);
        if manager.connect_snapshot().finished {
            break;
        }
        std::thread::sleep(POLL);
    }
    // 结束与新服务完成之间存在窗口：收尾时再排一次，保证最后一批也输出完整。
    write_claimed(&manager, stderr);
    // 总状态只输出一次：同一进程的后续任务不再重复。
    if let Some(tip) = manager.take_connect_report() {
        let _ = writeln!(stderr, "{tip}");
    }
}
