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

/// 合并一次运行的结论与退出时的清理结果，`label` 是清理侧的英文领域提示。
///
/// 清理失败若是 `CleanupError`，合并后仍以它作为外层类型（外层 `downcast_ref` 可见），
/// 原始失败作为内层上下文留在同一 anyhow 类型链里，`exit_code_for_error` 仍能识别
/// `UsageError`／`MaxTurnsExceeded`／`TimedOut`／`SignalCancelled`；清理失败不是
/// `CleanupError` 时原始错误保持外层，只追加清理上下文。合并文本同时包含原始失败与
/// 清理失败的正文。
pub fn merge_cleanup_result(
    result: anyhow::Result<()>,
    cleanup: anyhow::Result<()>,
    label: &str,
) -> anyhow::Result<()> {
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) | (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => match cleanup.downcast_ref::<CleanupError>() {
            // 清理失败保持 `CleanupError` 外层类型；原始失败作为内层上下文留在同一
            // anyhow 类型链上，`exit_code_for_error` 仍能识别语义错误。只借用内层正文，
            // 避免在外层重复 `Failed to stop:` 前缀。
            Some(CleanupError(inner)) => {
                Err(CleanupError(error.context(format!("{label}: {inner:#}"))).into())
            }
            None => Err(error.context(format!("{label}: {cleanup:#}"))),
        },
    }
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
    fn exit_codes_and_cleanup_merge_semantics() {
        {
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

    fn cleanup_failure() -> anyhow::Error {
        CleanupError(anyhow::anyhow!("mcp client did not exit")).into()
    }

    #[test]
    fn cleanup_failure_precedence_and_single_sided_passthrough() {
        {
            let cases: [(&str, anyhow::Error, u8); 5] = [
                ("usage", anyhow::Error::new(UsageError("bad arg".into())), 2),
                (
                    "max turns",
                    anyhow::Error::new(MaxTurnsExceeded { limit: 4 }),
                    3,
                ),
                (
                    "timeout",
                    anyhow::Error::new(TimedOut {
                        duration: Duration::from_secs(5),
                    }),
                    124,
                ),
                (
                    "sigint",
                    anyhow::Error::new(SignalCancelled { signal: 2 }),
                    130,
                ),
                (
                    "sigterm",
                    anyhow::Error::new(SignalCancelled { signal: 15 }),
                    143,
                ),
            ];
            for (case, original, expected) in cases {
                let original_text = format!("{original:#}");
                let merged =
                    merge_cleanup_result(Err(original), Err(cleanup_failure()), "stopping")
                        .expect_err("cleanup failure must surface");
                assert_eq!(exit_code_for_error(&merged), expected, "case {case}");
                assert!(
                    merged.is::<CleanupError>(),
                    "case {case}: cleanup stays the outer error"
                );
                let rendered = format!("{merged:#}");
                assert!(rendered.contains(&original_text), "case {case}: {rendered}");
                assert!(
                    rendered.contains("mcp client did not exit"),
                    "case {case}: {rendered}"
                );
            }
        }
        {
            let merged = merge_cleanup_result(
                Err(anyhow::anyhow!("provider exploded")),
                Err(cleanup_failure()),
                "stopping MCP servers",
            )
            .expect_err("cleanup failure must surface");
            assert_eq!(exit_code_for_error(&merged), 1);
            assert!(merged.is::<CleanupError>());
            let rendered = format!("{merged:#}");
            assert!(rendered.contains("provider exploded"), "{rendered}");
            assert!(rendered.contains("mcp client did not exit"), "{rendered}");

            let plain = merge_cleanup_result(
                Err(anyhow::Error::new(TimedOut {
                    duration: Duration::from_secs(1),
                })),
                Err(anyhow::anyhow!("kill failed")),
                "stopping language servers",
            )
            .expect_err("cleanup failure must surface");
            assert_eq!(exit_code_for_error(&plain), 124);
            assert!(plain.downcast_ref::<CleanupError>().is_none());
            let rendered = format!("{plain:#}");
            assert!(rendered.contains("Timed out after 1s."), "{rendered}");
            assert!(
                rendered.contains("stopping language servers: kill failed"),
                "{rendered}"
            );
        }
        {
            assert!(merge_cleanup_result(Ok(()), Ok(()), "stopping").is_ok());

            let result_only = merge_cleanup_result(
                Err(anyhow::Error::new(TimedOut {
                    duration: Duration::from_secs(1),
                })),
                Ok(()),
                "stopping",
            )
            .expect_err("run failure must surface");
            assert_eq!(exit_code_for_error(&result_only), 124);

            let cleanup_only = merge_cleanup_result(Ok(()), Err(cleanup_failure()), "stopping")
                .expect_err("cleanup failure must surface");
            assert!(cleanup_only.is::<CleanupError>());
            assert_eq!(exit_code_for_error(&cleanup_only), 1);
            let rendered = format!("{cleanup_only:#}");
            assert!(
                rendered.starts_with("Failed to stop: mcp client did not exit"),
                "{rendered}"
            );

            let plain_cleanup_only =
                merge_cleanup_result(Ok(()), Err(anyhow::anyhow!("kill failed")), "stopping")
                    .expect_err("cleanup failure must surface");
            assert_eq!(format!("{plain_cleanup_only:#}"), "kill failed");
        }
    }
}
