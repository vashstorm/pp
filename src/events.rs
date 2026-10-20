use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AgentEvent {
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
