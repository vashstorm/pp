//! 行模式与 `-p` 的启动期 MCP 建连提示：逐服务滚动一行，最后给总状态。
//!
//! 建连由 runtime 在后台线程完成，这里只轮询内存快照；文案与耗时同样来自 runtime。
use pp_runtime::agent::Agent;
use std::io::Write;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(25);

/// 等启动期建连结束，期间把新完成的服务逐条写到 stderr。
/// 提示不影响任务结果：写失败（例如管道断开）被忽略，不改变任务输出。
pub(crate) fn wait(agent: &Agent, stderr: &mut impl Write) {
    let manager = agent.mcp();
    let mut printed = 0usize;
    loop {
        let snapshot = manager.connect_snapshot();
        // 结束时也要把最后一批已完成的服务排完，再输出总状态。
        for server in &snapshot.servers[printed..] {
            if server.elapsed.is_none() {
                break;
            }
            let _ = writeln!(stderr, "MCP: {}", server.describe());
            if let Some(reason) = server.reason() {
                let _ = writeln!(stderr, "  {reason}");
            }
            printed += 1;
        }
        if snapshot.finished {
            break;
        }
        std::thread::sleep(POLL);
    }
    // 总状态只输出一次：同一进程的后续任务不再重复。
    if let Some(tip) = manager.take_connect_report() {
        let _ = writeln!(stderr, "{tip}");
    }
}
