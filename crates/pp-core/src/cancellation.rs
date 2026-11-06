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

pub fn check(token: &CancellationToken) -> anyhow::Result<()> {
    if token.is_cancelled() {
        return Err(Cancelled.into());
    }
    Ok(())
}
