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

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentEvent {
    PlanProgressUpdated {
        plan_id: String,
        revision: u64,
        content: String,
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
