use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileDiff {
    Text {
        created: bool,
        #[serde(default)]
        deleted: bool,
        added: usize,
        removed: usize,
        hunks: Vec<DiffHunk>,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffHunk {
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestRetryReason {
    RateLimited,
    HttpStatus(u16),
    Timeout,
    Connection,
    ModelError,
}

impl std::fmt::Display for RequestRetryReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited => formatter.write_str("rate limited"),
            Self::HttpStatus(status) => write!(formatter, "HTTP status {status}"),
            Self::Timeout => formatter.write_str("request timed out"),
            Self::Connection => formatter.write_str("connection failed"),
            Self::ModelError => formatter.write_str("model response failed"),
        }
    }
}

/// Runtime-only display snapshot of the task list; it carries no plan body and is not a new
/// persistence authority.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoState {
    pub phases: Vec<crate::todo::TodoPhase>,
    pub plan: Option<TodoPlanState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoPlanState {
    pub id: String,
    pub revision: u64,
    pub status: crate::plan::PlanStatus,
}

impl TodoState {
    pub fn from_workflow(workflow: &crate::plan::WorkflowState) -> Self {
        if workflow.mode == crate::plan::WorkMode::Plan {
            return Self::default();
        }
        Self {
            phases: workflow.todos.clone(),
            plan: workflow
                .plan
                .as_ref()
                .filter(|plan| workflow.todo_plan_id.as_deref() == Some(plan.id.as_str()))
                .map(|plan| TodoPlanState {
                    id: plan.id.clone(),
                    revision: plan.revision,
                    status: plan.status,
                }),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentEvent {
    TodoStateUpdated {
        state: TodoState,
    },
    TodoReminder {
        todos: Vec<crate::todo::TodoItem>,
        attempt: u8,
        max_attempts: u8,
    },
    TodoSnapshot {
        plan_id: String,
        revision: u64,
        phases: Vec<crate::todo::TodoPhase>,
        status: crate::plan::PlanStatus,
    },
    PlanProgressUpdated {
        plan_id: String,
        revision: u64,
        content: String,
        /// Current value for pure state changes such as archiving; when an older event lacks the
        /// field the receiver keeps its current value.
        #[serde(default)]
        file_name: Option<String>,
        #[serde(default)]
        location: Option<crate::plan::PlanLocation>,
        #[serde(default)]
        status: Option<crate::plan::PlanStatus>,
    },
    ContextUpdated {
        usage: crate::context::ContextUsage,
    },
    CompactionStarted {
        automatic: bool,
    },
    CompactionFinished {
        before_tokens: u64,
        after_tokens: u64,
    },
    UserInputAccepted {
        id: u64,
        text: String,
    },
    /// A pre-submit input hook rejected the input explicitly: nothing enters history, no model
    /// request is made, and `text` is only for displaying the raw text.
    UserInputRejected {
        id: u64,
        text: String,
        reason: String,
    },
    /// Hook diagnostic: for display and logs only, never part of the model context, never
    /// persisted.
    HookDiagnostic {
        event: String,
        message: String,
    },
    ModelRequestStarted,
    RequestRetrying {
        reason: RequestRetryReason,
        attempt: u32,
        wait_ms: u64,
    },
    ModelOutputProgress {
        output_bytes: u64,
    },
    ModelRequestFinished {
        elapsed_ms: u64,
        usage: Option<crate::context::TokenUsage>,
        success: bool,
    },
    ModelAttemptFailed {
        attempt: u32,
        #[serde(default)]
        model: String,
        #[serde(default)]
        error: String,
    },
    ModelRouteChanged {
        model: String,
        effort: crate::model::ThinkLevel,
        /// Switch source: manual override, config default, role main model or in-task fallback
        /// model.
        #[serde(default)]
        source: crate::model::ModelSource,
        temporary: bool,
    },
    AssistantStarted,
    ThinkingDelta {
        text: String,
    },
    ReasoningSummaryDelta {
        text: String,
    },
    AssistantDelta {
        text: String,
        refusal: bool,
    },
    AssistantFinished {
        text: String,
    },
    /// File tool arguments while the model is generating; for temporary preview only and neither
    /// proof that the tool ran nor that the file was saved.
    ToolArgumentsDelta {
        tool: String,
        index: u32,
        text: String,
    },
    ToolStarted {
        id: String,
        name: String,
        arguments: String,
    },
    ProjectInstructionsLoaded {
        path: String,
        content: String,
    },
    FileChanged {
        id: String,
        path: String,
        diff: FileDiff,
    },
    ToolOutput {
        id: String,
        stream: OutputStream,
        text: String,
    },
    ToolOutputSnapshot {
        id: String,
        output: String,
    },
    /// Subagent results already committed to the parent session's native context.
    SubagentResultsInjected {
        run_ids: Vec<String>,
    },
    /// Transient command display; the final snapshot is frozen by the tool result's shell field.
    ToolShellSnapshot {
        id: String,
        snapshot: crate::shell_display::ShellPresentation,
    },
    ToolFinished {
        id: String,
        result: serde_json::Value,
    },
    /// Display copy of LSP diagnostics. When `id` is the current tool call this is an inline report;
    /// `None` means a background report that finished late after saving. The event only freezes
    /// display and triggers no new model request.
    LspDiagnostics {
        #[serde(default)]
        id: Option<String>,
        report: crate::lsp_report::DiagnosticReport,
    },
    /// Transient event for the independent security review performed by the REVIEW role under
    /// `auto_review` mode; not written to the journal.
    AutoReviewStarted {
        tool: String,
    },
    AutoReviewFinished {
        tool: String,
        passed: bool,
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_diagnostics_and_text_diff_serde_contracts() {
        {
            let report = crate::lsp_report::DiagnosticReport {
                path: "src/a.rs".into(),
                origin: "write".into(),
                owner: "root".into(),
                servers: vec![crate::lsp_report::ServerReport {
                    server: "rust-analyzer".into(),
                    status: crate::lsp_report::ReportStatus::ReportedIssues,
                    note: None,
                    diagnostics: vec![crate::lsp_report::ReportDiagnostic {
                        line: 3,
                        column: 2,
                        end_line: 3,
                        end_column: 4,
                        severity: 1,
                        message: "expected i32".into(),
                        code: None,
                        source: None,
                        line_only: false,
                    }],
                }],
                formatter: None,
                deduplicated: 0,
                omitted: 0,
                at_ms: 7,
            };
            let event = AgentEvent::LspDiagnostics {
                id: Some("call-1".into()),
                report,
            };
            let AgentEvent::LspDiagnostics { report, .. } = event else {
                unreachable!("a just-constructed event must be LspDiagnostics");
            };
            assert_eq!(report.path, "src/a.rs");
            assert!(report.has_issues());
        }
        {
            let event: AgentEvent = serde_json::from_value(serde_json::json!({
                "file_changed": {"id":"old","path":"empty","diff":{
                    "kind":"text","created":false,"added":0,"removed":0,"hunks":[]
                }}
            }))
            .unwrap();
            assert!(matches!(
                event,
                AgentEvent::FileChanged {
                    diff: FileDiff::Text { deleted: false, .. },
                    ..
                }
            ));
        }
    }
}
