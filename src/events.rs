use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum FileDiff {
    Text {
        created: bool,
        added: usize,
        removed: usize,
        hunks: Vec<DiffHunk>,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DiffHunk {
    pub(crate) lines: Vec<DiffLine>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct DiffLine {
    pub(crate) kind: DiffLineKind,
    pub(crate) old_line: Option<usize>,
    pub(crate) new_line: Option<usize>,
    pub(crate) text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RequestRetryReason {
    RateLimited,
    HttpStatus(u16),
    Timeout,
    Connection,
}

impl std::fmt::Display for RequestRetryReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimited => formatter.write_str("rate limited"),
            Self::HttpStatus(status) => write!(formatter, "HTTP status {status}"),
            Self::Timeout => formatter.write_str("request timed out"),
            Self::Connection => formatter.write_str("connection failed"),
        }
    }
}

/// 仅供运行时展示的任务快照，不包含计划正文，也不是新的持久化权威。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TodoState {
    pub phases: Vec<crate::tools::todo::TodoPhase>,
    pub plan: Option<TodoPlanState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TodoPlanState {
    pub id: String,
    pub revision: u64,
    pub status: crate::plan::PlanStatus,
}

impl TodoState {
    pub(crate) fn from_workflow(workflow: &crate::plan::WorkflowState) -> Self {
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

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentEvent {
    TodoStateUpdated {
        state: TodoState,
    },
    TodoReminder {
        todos: Vec<crate::tools::todo::TodoItem>,
        attempt: u8,
        max_attempts: u8,
    },
    TodoSnapshot {
        plan_id: String,
        revision: u64,
        phases: Vec<crate::tools::todo::TodoPhase>,
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
    ToolFinished {
        id: String,
        result: serde_json::Value,
    },
}
