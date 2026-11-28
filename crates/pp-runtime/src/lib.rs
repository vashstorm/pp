pub mod agent;
pub mod history;
mod hooks;
pub mod model_control;
pub mod subagent;
pub mod system_prompt;
pub mod user_input;

/// Subagents, MCP and LSP hold several file descriptors at once; the macOS default soft limit is only 256,
/// so 32 concurrent subagents would fail with `Too many open files`. Raise the soft limit to the hard limit at
/// startup (capped at [`FILE_LIMIT`]); failure does not affect operation.
pub const FILE_LIMIT: libc::rlim_t = 8192;

pub fn raise_file_limit() {
    // SAFETY: getrlimit/setrlimit only read and write this process's rlimit struct, and all pointers are valid.
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
