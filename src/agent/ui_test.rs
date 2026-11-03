//! Debug-only slow mock UI stream.
//!
//! The stream uses fixed `AgentEvent` payloads and never touches the provider,
//! session journal, statistics, or real tools. Each chunk is separated by about
//! 80 ms so slow Markdown rendering and cancellation can be inspected.

use crate::{
    cancellation::{self, CancellationToken},
    events::AgentEvent,
};
use anyhow::Result;
use std::time::{Duration, Instant};

pub(crate) const TOKEN_DELAY: Duration = Duration::from_millis(80);
const CANCELLATION_POLL: Duration = Duration::from_millis(5);

const THINKING: &[&str] = &[
    "UI-TEST-THINKING-START ",
    "mock thinking token one. ",
    "mock thinking token two. ",
    "mock thinking token three. ",
    "UI-TEST-THINKING-END",
];

const ASSISTANT: &[&str] = &[
    "UI-TEST-ASSISTANT-START\n\n",
    "## Mock Markdown\n\n",
    "This response comes from fixed AgentEvent data.\n\n",
    "- first list item\n",
    "- second list item with `inline code`\n\n",
    "```rust\n",
    "fn mock_ui() -> &'static str {\n",
    "    \"UI-TEST-CODE-MARKER\"\n",
    "}\n",
    "```\n\n",
    "UI-TEST-ASSISTANT-END\n",
];

impl crate::agent::Agent {
    pub(crate) fn run_ui_test(
        &self,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<()> {
        stream(cancellation, emit, TOKEN_DELAY)
    }
}

fn stream(
    cancellation: &CancellationToken,
    emit: &mut dyn FnMut(AgentEvent),
    delay: Duration,
) -> Result<()> {
    cancellation::check(cancellation)?;
    let started = Instant::now();
    emit(AgentEvent::ModelRequestStarted);
    cancellation::check(cancellation)?;
    emit(AgentEvent::AssistantStarted);
    let mut output_bytes = 0u64;
    for chunk in THINKING.iter().chain(ASSISTANT.iter()) {
        cancellation::check(cancellation)?;
        if THINKING.contains(chunk) {
            emit(AgentEvent::ThinkingDelta {
                text: (*chunk).to_owned(),
            });
        } else {
            emit(AgentEvent::AssistantDelta {
                text: (*chunk).to_owned(),
                refusal: false,
            });
        }
        output_bytes += chunk.len() as u64;
        emit(AgentEvent::ModelOutputProgress { output_bytes });
        pause(cancellation, delay)?;
    }
    cancellation::check(cancellation)?;
    emit(AgentEvent::AssistantFinished {
        text: ASSISTANT.concat(),
    });
    emit(AgentEvent::ModelRequestFinished {
        elapsed_ms: started.elapsed().as_millis() as u64,
        usage: None,
        success: true,
    });
    Ok(())
}

fn pause(cancellation: &CancellationToken, duration: Duration) -> Result<()> {
    let deadline = Instant::now() + duration;
    loop {
        cancellation::check(cancellation)?;
        let now = Instant::now();
        if now >= deadline {
            return Ok(());
        }
        std::thread::sleep((deadline - now).min(CANCELLATION_POLL));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_stream_emits_ordered_deltas_and_one_complete_finish() {
        let cancellation = CancellationToken::new();
        let mut events = Vec::new();
        stream(
            &cancellation,
            &mut |event| events.push(event),
            Duration::ZERO,
        )
        .unwrap();
        assert!(matches!(events[0], AgentEvent::ModelRequestStarted));
        assert!(matches!(events[1], AgentEvent::AssistantStarted));
        let deltas = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::AssistantDelta {
                    text,
                    refusal: false,
                } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        let finished = events
            .iter()
            .find_map(|event| match event {
                AgentEvent::AssistantFinished { text } => Some(text),
                _ => None,
            })
            .expect("finished mock assistant text");
        assert_eq!(&deltas, finished);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, AgentEvent::AssistantFinished { .. }))
                .count(),
            1
        );
        let bytes = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ModelOutputProgress { output_bytes } => Some(*output_bytes),
                _ => None,
            })
            .max()
            .expect("mock output progress");
        assert_eq!(
            bytes,
            THINKING
                .iter()
                .chain(ASSISTANT.iter())
                .map(|chunk| chunk.len() as u64)
                .sum::<u64>()
        );
        assert!(matches!(
            events.last(),
            Some(AgentEvent::ModelRequestFinished {
                usage: None,
                success: true,
                ..
            })
        ));
    }

    #[test]
    fn mock_stream_pre_cancellation_emits_nothing() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let mut events = Vec::new();
        let error = stream(
            &cancellation,
            &mut |event| events.push(event),
            Duration::ZERO,
        )
        .unwrap_err();
        assert!(error.is::<cancellation::Cancelled>());
        assert!(events.is_empty());
    }
}
