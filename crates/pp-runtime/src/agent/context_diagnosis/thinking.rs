//! Collection, replay extraction and diagnostic entry construction for LLM thinking (Thinking / Reasoning) records.

use super::DiagnosticItem;
use super::sections::{first_line_preview, truncate_preview};
use crate::agent::{Session, persistence::ReplayRecord};
use anyhow::Result;
use async_openai::types::responses::{
    InputItem, InputParam, Item, ReasoningItem, ReasoningItemContent, SummaryPart,
};
use pp_core::{context::estimate_bytes, events::AgentEvent};
use std::sync::Arc;

/// Committed LLM thinking records of single turns within the session (covers streaming and `/resume` replay).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ThinkingEntry {
    pub(crate) thinking: String,
    pub(crate) summary: String,
}

impl ThinkingEntry {
    pub(crate) fn is_empty(&self) -> bool {
        self.thinking.trim().is_empty() && self.summary.trim().is_empty()
    }
}

/// Extract the thinking content of committed turns from session `replay` records (excluding failed retries and uncommitted interruptions).
pub(crate) fn extract_thinking_from_replay(records: &[ReplayRecord]) -> Vec<ThinkingEntry> {
    let mut entries = Vec::new();
    let mut current_thinking = String::new();
    let mut current_summary = String::new();
    let mut has_assistant_commit = false;

    let flush_committed = |entries: &mut Vec<ThinkingEntry>,
                           thinking: &mut String,
                           summary: &mut String,
                           committed: &mut bool| {
        if *committed && (!thinking.trim().is_empty() || !summary.trim().is_empty()) {
            entries.push(ThinkingEntry {
                thinking: std::mem::take(thinking),
                summary: std::mem::take(summary),
            });
        } else {
            thinking.clear();
            summary.clear();
        }
        *committed = false;
    };

    for record in records {
        match record {
            ReplayRecord::User(_) => {
                flush_committed(
                    &mut entries,
                    &mut current_thinking,
                    &mut current_summary,
                    &mut has_assistant_commit,
                );
            }
            ReplayRecord::Event(event) => match event {
                AgentEvent::AssistantStarted => {
                    flush_committed(
                        &mut entries,
                        &mut current_thinking,
                        &mut current_summary,
                        &mut has_assistant_commit,
                    );
                }
                AgentEvent::ThinkingDelta { text } => {
                    current_thinking.push_str(text);
                }
                AgentEvent::ReasoningSummaryDelta { text } => {
                    current_summary.push_str(text);
                }
                AgentEvent::AssistantFinished { .. } | AgentEvent::ToolFinished { .. } => {
                    has_assistant_commit = true;
                }
                AgentEvent::ModelAttemptFailed { .. } => {
                    current_thinking.clear();
                    current_summary.clear();
                    has_assistant_commit = false;
                }
                _ => {}
            },
            ReplayRecord::Error(_) | ReplayRecord::Cancelled => {
                if !has_assistant_commit {
                    current_thinking.clear();
                    current_summary.clear();
                }
            }
        }
    }
    flush_committed(
        &mut entries,
        &mut current_thinking,
        &mut current_summary,
        &mut has_assistant_commit,
    );
    entries
}

/// Align the retained assistant turns after compaction and trim early thinking records already replaced by the summary.
pub(crate) fn trim_for_retained_turns(
    history: &mut Vec<ThinkingEntry>,
    retained_assistant_turns: usize,
) {
    if history.len() > retained_assistant_turns {
        let drop_count = history.len() - retained_assistant_turns;
        history.drain(..drop_count);
    }
}

pub(super) struct ThinkingBreakdown {
    pub items: Vec<Arc<DiagnosticItem>>,
    pub tokens: u64,
    pub bytes: u64,
    pub count: usize,
    pub in_context: bool,
}

pub(super) fn build_thinking_items(
    session: &Session,
    history: &[ThinkingEntry],
) -> Result<ThinkingBreakdown> {
    match session {
        Session::Response(request) => build_response_thinking(request, history),
        Session::Completion(_) => build_completion_thinking(history),
    }
}

fn build_response_thinking(
    request: &async_openai::types::responses::CreateResponse,
    history: &[ThinkingEntry],
) -> Result<ThinkingBreakdown> {
    let mut items = Vec::new();
    let mut total_tokens = 0u64;
    let mut total_bytes = 0u64;
    let mut count = 0usize;

    let InputParam::Items(input_items) = &request.input else {
        return Ok(ThinkingBreakdown {
            items,
            tokens: 0,
            bytes: 0,
            count: 0,
            in_context: true,
        });
    };

    let mut reasoning_idx = 0usize;
    for (idx, item) in input_items.iter().enumerate() {
        let InputItem::Item(Item::Reasoning(reasoning)) = item else {
            continue;
        };
        let seq = idx + 1;
        let bytes = estimate_bytes(item)?;
        let tokens = bytes.div_ceil(3);
        total_bytes = total_bytes.saturating_add(bytes);
        total_tokens = total_tokens.saturating_add(tokens);
        count += 1;

        let fallback_entry = history.get(reasoning_idx);
        reasoning_idx += 1;
        let (subtitle, preview) = format_reasoning_item(reasoning, fallback_entry);
        items.push(Arc::new(DiagnosticItem {
            kind_label: "Thinking".into(),
            title: format!("#{seq} Reasoning (turn {count})"),
            subtitle,
            tokens,
            bytes,
            preview,
        }));
    }

    // If the session has no native Reasoning items yet but streamed thinking was collected (some compatible gateways omit Reasoning items), add it for display.
    if count == 0 && !history.is_empty() {
        return build_completion_thinking(history);
    }

    Ok(ThinkingBreakdown {
        items,
        tokens: total_tokens,
        bytes: total_bytes,
        count,
        in_context: true,
    })
}

fn build_completion_thinking(history: &[ThinkingEntry]) -> Result<ThinkingBreakdown> {
    let mut items = Vec::new();
    let mut total_tokens = 0u64;
    let mut total_bytes = 0u64;
    let mut count = 0usize;

    for (idx, entry) in history.iter().enumerate() {
        if entry.is_empty() {
            continue;
        }
        let mut sections = Vec::new();
        if !entry.summary.trim().is_empty() {
            sections.push(format!("[Summary]\n{}", entry.summary.trim()));
        }
        if !entry.thinking.trim().is_empty() {
            sections.push(entry.thinking.trim().to_owned());
        }
        let combined = sections.join("\n\n");
        let bytes = estimate_bytes(&combined)?;
        let tokens = bytes.div_ceil(3);
        total_bytes = total_bytes.saturating_add(bytes);
        total_tokens = total_tokens.saturating_add(tokens);
        count += 1;

        items.push(Arc::new(DiagnosticItem {
            kind_label: "Thinking".into(),
            title: format!("Turn #{} Thinking", idx + 1),
            subtitle: first_line_preview(&combined),
            tokens,
            bytes,
            preview: truncate_preview(&combined),
        }));
    }

    Ok(ThinkingBreakdown {
        items,
        tokens: total_tokens,
        bytes: total_bytes,
        count,
        in_context: false,
    })
}

fn format_reasoning_item(
    reasoning: &ReasoningItem,
    fallback: Option<&ThinkingEntry>,
) -> (String, String) {
    let mut parts = Vec::new();
    let mut has_plain = false;
    for s in &reasoning.summary {
        let SummaryPart::SummaryText(s) = s;
        if !s.text.trim().is_empty() {
            parts.push(format!("[Summary]\n{}", s.text.trim()));
            has_plain = true;
        }
    }
    for c in reasoning.content.iter().flatten() {
        let ReasoningItemContent::ReasoningText(c) = c;
        if !c.text.trim().is_empty() {
            parts.push(c.text.trim().to_owned());
            has_plain = true;
        }
    }
    if !has_plain
        && let Some(entry) = fallback
        && !entry.is_empty()
    {
        if !entry.summary.trim().is_empty() {
            parts.push(format!("[Summary]\n{}", entry.summary.trim()));
        }
        if !entry.thinking.trim().is_empty() {
            parts.push(entry.thinking.trim().to_owned());
        }
    }
    if let Some(enc) = &reasoning.encrypted_content {
        parts.push(format!(
            "<encrypted_content: {} chars, ~{} tokens>",
            enc.len(),
            enc.len().div_ceil(3)
        ));
    }
    let preview = if parts.is_empty() {
        "(empty reasoning item)".into()
    } else {
        truncate_preview(&parts.join("\n\n"))
    };
    let subtitle = if let Some(enc) = &reasoning.encrypted_content {
        if has_plain {
            format!("summary + encrypted_content ({} chars)", enc.len())
        } else {
            format!("encrypted_content ({} chars)", enc.len())
        }
    } else {
        first_line_preview(&preview)
    };
    (subtitle, preview)
}
