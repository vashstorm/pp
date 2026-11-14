pub mod agent;
pub mod history;
mod hooks;
pub mod host_defaults;
pub mod lsp;
pub mod mcp;
#[cfg(debug_assertions)]
pub mod mock_ai;
pub mod model_control;
pub mod project_instructions;
pub mod subagent;
pub mod system_prompt;
pub mod tools;
pub mod user_input;

/// 子 Agent、MCP 与 LSP 会同时持有多个文件描述符；macOS 默认 soft 上限只有 256，
/// 32 个并发子 Agent 会以 `Too many open files` 失败。启动时把 soft 提高到硬限
/// （最多 [`FILE_LIMIT`]），失败不影响运行。
pub const FILE_LIMIT: libc::rlim_t = 8192;

pub fn raise_file_limit() {
    // SAFETY: getrlimit/setrlimit 只读写本进程的 rlimit 结构，参数均为有效指针。
    unsafe {
        let mut limits = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) != 0 {
            return;
        }
        let target = if limits.rlim_max == libc::RLIM_INFINITY {
            FILE_LIMIT
        } else {
            limits.rlim_max.min(FILE_LIMIT)
        };
        if limits.rlim_cur >= target {
            return;
        }
        limits.rlim_cur = target;
        let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &limits);
    }
}
