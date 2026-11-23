pub use tokio_util::sync::CancellationToken;

#[derive(Debug, PartialEq, Eq)]
pub enum RunOutcome {
    Completed,
    AwaitingPlanReview,
    Cancelled,
}

#[derive(Debug)]
pub struct Cancelled;

impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Task cancelled.")
    }
}
impl std::error::Error for Cancelled {}

#[derive(Debug)]
pub struct CleanupError(pub anyhow::Error);

impl std::fmt::Display for CleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Failed to stop: {:#}", self.0)
    }
}
impl std::error::Error for CleanupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

/// CLI 参数、子命令搭配或运行前提使用错误（退出码 2）。
#[derive(Debug)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for UsageError {}

/// 非交互运行达到 `--max-turns` 轮次上限（退出码 3）。
#[derive(Debug)]
pub struct MaxTurnsExceeded {
    pub limit: usize,
}

impl std::fmt::Display for MaxTurnsExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Reached --max-turns limit ({}).", self.limit)
    }
}
impl std::error::Error for MaxTurnsExceeded {}

/// 非交互运行达到 `--timeout` 时限（退出码 124）。
#[derive(Debug)]
pub struct TimedOut {
    pub duration: std::time::Duration,
}

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let millis = self.duration.as_millis();
        if millis.is_multiple_of(1000) {
            write!(f, "Timed out after {}s.", self.duration.as_secs())
        } else {
            write!(f, "Timed out after {millis}ms.")
        }
    }
}
impl std::error::Error for TimedOut {}

/// 非交互运行收到 `SIGINT` / `SIGTERM` 并完成优雅取消（退出码 128 + signal）。
#[derive(Debug)]
pub struct SignalCancelled {
    pub signal: i32,
}

impl std::fmt::Display for SignalCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.signal {
            2 => "SIGINT",
            15 => "SIGTERM",
            other => return write!(f, "Interrupted by signal {other}."),
        };
        write!(f, "Interrupted by {name}.")
    }
}
impl std::error::Error for SignalCancelled {}

/// 将运行时或 CLI 错误映射为语义化进程退出码。
pub fn exit_code_for_error(error: &anyhow::Error) -> u8 {
    if error.chain().any(|cause| cause.is::<UsageError>()) {
        return 2;
    }
    if error.chain().any(|cause| cause.is::<MaxTurnsExceeded>()) {
        return 3;
    }
    if error.chain().any(|cause| cause.is::<TimedOut>()) {
        return 124;
    }
    if let Some(signal) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<SignalCancelled>())
    {
        return 128u8.saturating_add(signal.signal.clamp(1, 64) as u8);
    }
    1
}

pub fn check(token: &CancellationToken) -> anyhow::Result<()> {
    if token.is_cancelled() {
        return Err(Cancelled.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn maps_typed_errors_to_semantic_exit_codes() {
        assert_eq!(
            exit_code_for_error(&anyhow::Error::new(UsageError("bad arg".into()))),
            2
        );
        assert_eq!(
            exit_code_for_error(&anyhow::Error::new(MaxTurnsExceeded { limit: 4 })),
            3
        );
        assert_eq!(
            exit_code_for_error(
                &anyhow::Error::new(TimedOut {
                    duration: Duration::from_secs(5),
                })
                .context("outer"),
            ),
            124
        );
        assert_eq!(
            exit_code_for_error(&anyhow::Error::new(SignalCancelled { signal: 2 })),
            130
        );
        assert_eq!(
            exit_code_for_error(&anyhow::Error::new(SignalCancelled { signal: 15 })),
            143
        );
        assert_eq!(exit_code_for_error(&anyhow::anyhow!("boom")), 1);
    }
}
