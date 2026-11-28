//! Startup MCP connect hints for line mode and `-p`: one line per server as they
//! settle, then a final overall status.
//!
//! The runtime connects on a background thread; this only polls an in-memory snapshot,
//! and both the wording and the timings come from the runtime.
use pp_runtime::agent::Agent;
use std::io::Write;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(25);

// Emit completed servers this process has not printed yet; deduplicated by the
// manager's snapshot cursor.
fn write_claimed(manager: &pp_mcp::McpManager, stderr: &mut impl Write) {
    for server in manager.take_connect_services() {
        let _ = writeln!(stderr, "MCP: {}", server.describe());
        if let Some(reason) = server.reason() {
            let _ = writeln!(stderr, "  {reason}");
        }
    }
}

/// Waits for startup connects to finish (or exits early when cancellation is
/// signalled), writing each newly completed server to stderr as it arrives.
/// Already printed servers are deduplicated by the manager's snapshot cursor, so later
/// tasks in the same process and retries after cancellation never print them again.
/// The hints never affect the task result: write failures (for example a broken pipe)
/// are ignored and do not change task output.
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
    // There is a window between finishing and a new server completing: drain once more
    // on the way out so the last batch is fully printed.
    write_claimed(&manager, stderr);
    // The overall status prints once: later tasks in the same process do not repeat it.
    if let Some(tip) = manager.take_connect_report() {
        let _ = writeln!(stderr, "{tip}");
    }
}
