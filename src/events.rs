#[derive(Clone, Copy, Debug)]
pub(crate) enum OutputStream {
    Stdout,
    Stderr,
}

pub(crate) enum AgentEvent {
    AssistantStarted,
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
