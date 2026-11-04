use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    /// 服务器报告了问题。
    ReportedIssues,
    /// 服务器提供了新鲜报告且没有问题。
    ReportedClean,
    /// 未收到足够新的报告。
    #[default]
    Unreported,
    /// 请求或解析失败。
    Failed,
    /// 服务器没有该能力。
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
    /// 1..=4（error/warning/info/hint）。
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

/// 一次保存/编辑后的诊断报告，供工具结果、journal 与 TUI 共用。
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
    /// 自动报告超过消息展示上限的数量；总问题数不能因截断变成零。
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
