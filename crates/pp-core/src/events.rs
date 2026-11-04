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

/// 仅供运行时展示的任务快照，不包含计划正文，也不是新的持久化权威。
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
        /// 归档等纯状态变化的当前值；旧事件缺字段时接收端保留当前值。
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
        fallback: bool,
        temporary: bool,
    },
    AssistantStarted,
    ThinkingDelta {
        text: String,
    },
    AssistantDelta {
        text: String,
        refusal: bool,
    },
    AssistantFinished {
        text: String,
    },
    /// 模型生成中的文件工具参数，仅用于临时预览，不代表工具已执行或文件已保存。
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
    /// 运行中的管道阶段预览。瞬时正文只用于卡片，不写 journal 或工具结果；
    /// 调用完成后的 `result.pipes` 才是恢复权威。
    ToolPipelineSnapshot {
        id: String,
        pipes: serde_json::Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<String>,
    },
    ToolFinished {
        id: String,
        result: serde_json::Value,
    },
    /// LSP 诊断的展示副本。`id` 为当前工具调用时是内联报告；`None` 表示
    /// 保存后迟到完成的后台报告。事件只冻结展示，不触发新的模型请求。
    LspDiagnostics {
        #[serde(default)]
        id: Option<String>,
        report: crate::lsp_report::DiagnosticReport,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsp_diagnostics_event_round_trips_inline_and_late() {
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
        let value = serde_json::to_value(AgentEvent::LspDiagnostics {
            id: Some("call-1".into()),
            report: report.clone(),
        })
        .unwrap();
        let restored: AgentEvent = serde_json::from_value(value).unwrap();
        match restored {
            AgentEvent::LspDiagnostics {
                id: Some(id),
                report: restored,
            } => {
                assert_eq!(id, "call-1");
                assert_eq!(restored.path, "src/a.rs");
                assert!(restored.has_issues());
            }
            _ => panic!("inline LSP diagnostics must round trip"),
        }
        let value = serde_json::to_value(AgentEvent::LspDiagnostics { id: None, report }).unwrap();
        assert!(matches!(
            serde_json::from_value::<AgentEvent>(value).unwrap(),
            AgentEvent::LspDiagnostics { id: None, .. }
        ));
    }

    #[test]
    fn lsp_edits_old_journal_text_diff_preserves_non_deleted_file() {
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
