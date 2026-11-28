use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    /// The server reported issues.
    ReportedIssues,
    /// The server supplied a fresh report with no issues.
    ReportedClean,
    /// No sufficiently recent report was received.
    #[default]
    Unreported,
    /// The request or parsing failed.
    Failed,
    /// The server lacks this capability.
    Unsupported,
}

impl ReportStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ReportedIssues => "issues",
            Self::ReportedClean => "clean",
            Self::Unreported => "unverified",
            Self::Failed => "failed",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReportDiagnostic {
    pub line: u32,
    pub column: u32,
    pub end_line: u32,
    pub end_column: u32,
    /// 1..=4 (error/warning/info/hint).
    pub severity: u8,
    pub message: String,
    pub code: Option<String>,
    pub source: Option<String>,
    #[serde(default)]
    pub line_only: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ServerReport {
    pub server: String,
    pub status: ReportStatus,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub diagnostics: Vec<ReportDiagnostic>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FormatterStatus {
    pub server: String,
    pub status: ReportStatus,
    #[serde(default)]
    pub note: Option<String>,
}

/// Diagnostic report for one save/edit, shared by tool results, the journal and the TUI.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DiagnosticReport {
    pub path: String,
    pub origin: String,
    pub owner: String,
    #[serde(default)]
    pub servers: Vec<ServerReport>,
    #[serde(default)]
    pub formatter: Option<FormatterStatus>,
    #[serde(default)]
    pub deduplicated: usize,
    /// Number of automatic reports beyond the message display limit; the total issue count must not
    /// drop to zero because of truncation.
    #[serde(default)]
    pub omitted: usize,
    #[serde(default)]
    pub at_ms: u64,
}

impl DiagnosticReport {
    pub fn has_issues(&self) -> bool {
        self.servers
            .iter()
            .any(|server| server.status == ReportStatus::ReportedIssues)
    }

    pub fn all_clean(&self) -> bool {
        let mut any = false;
        for server in &self.servers {
            match server.status {
                ReportStatus::ReportedClean => any = true,
                ReportStatus::ReportedIssues => return false,
                ReportStatus::Unsupported => {}
                ReportStatus::Unreported | ReportStatus::Failed => return false,
            }
        }
        any
    }

    pub fn total(&self) -> usize {
        self.deduplicated
            + self.omitted
            + self
                .servers
                .iter()
                .map(|server| server.diagnostics.len())
                .sum::<usize>()
    }
}
