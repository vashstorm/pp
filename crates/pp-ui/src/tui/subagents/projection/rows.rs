//! Observation record ingestion: creation, merging and legacy-log synthesis of Activity rows.

use super::text::summarize_arguments;
use super::*;
use crate::tui::transcript_style::tool_label;

impl Projection {
    pub(super) fn consume(&mut self, agent: &str, entry: &ObservedEntry) {
        let record = &entry.record;
        let offset = entry.start_offset;
        let timestamp = record.timestamp_ms;
        let run = record.run_id.clone();
        {
            let stream = self.streams.entry(agent.to_owned()).or_default();
            if let Some(timestamp) = timestamp {
                stream.last_activity = Some(
                    stream
                        .last_activity
                        .map_or(timestamp, |old| old.max(timestamp)),
                );
            }
        }
        match (&record.event, &record.hub) {
            (Some(event), _) => {
                self.consume_event(agent, run.as_deref(), offset, timestamp, event);
            }
            (None, Some(HubEvent::RunState { status })) => {
                let stream = self.streams.entry(agent.to_owned()).or_default();
                if let Some(run) = &run {
                    stream.markers.insert(run.clone());
                    if let Some(timestamp) = timestamp {
                        match status.as_str() {
                            "running" => {
                                stream.started.entry(run.clone()).or_insert(timestamp);
                            }
                            "queued" => {}
                            _ => {
                                stream.ended.entry(run.clone()).or_insert(timestamp);
                            }
                        }
                    }
                }
                let row_status = match status.as_str() {
                    "queued" | "running" => RowStatus::Pending,
                    "completed" => RowStatus::Success,
                    "cancelled" => RowStatus::Aborted,
                    _ => RowStatus::Error,
                };
                if row_status == RowStatus::Error {
                    let message = format!("run {status}");
                    self.streams.entry(agent.to_owned()).or_default().last_error =
                        Some(message.clone());
                }
                self.insert_lifecycle(
                    agent,
                    run.as_deref(),
                    offset,
                    timestamp,
                    "Run",
                    status.clone(),
                    row_status,
                    1,
                );
            }
            (None, Some(HubEvent::AgentState { status })) => {
                let row_status = match status.as_str() {
                    "running" => RowStatus::Pending,
                    "aborted" => RowStatus::Aborted,
                    _ => RowStatus::Success,
                };
                self.insert_lifecycle(
                    agent,
                    None,
                    offset,
                    timestamp,
                    "Agent",
                    status.clone(),
                    row_status,
                    1,
                );
            }
            _ => {}
        }
    }

    fn consume_event(
        &mut self,
        agent: &str,
        run: Option<&str>,
        offset: u64,
        timestamp: Option<u64>,
        event: &AgentEvent,
    ) {
        let run_key = format!("{agent}|{}", run.unwrap_or("-"));
        match event {
            AgentEvent::AssistantStarted => {
                let turn = {
                    let stream = self.streams.entry(agent.to_owned()).or_default();
                    stream
                        .work
                        .entry(run.unwrap_or_default().into())
                        .or_default()
                        .draft
                        .clear();
                    let turn = stream.turns.entry(run_key.clone()).or_insert(0);
                    *turn += 1;
                    *turn
                };
                self.response_row(
                    agent,
                    run,
                    turn,
                    offset,
                    timestamp,
                    String::new(),
                    RowStatus::Pending,
                );
            }
            AgentEvent::AssistantDelta { text, .. }
            | AgentEvent::ThinkingDelta { text }
            | AgentEvent::ReasoningSummaryDelta { text } => {
                let turn = {
                    let stream = self.streams.entry(agent.to_owned()).or_default();
                    if matches!(event, AgentEvent::ReasoningSummaryDelta { .. }) {
                        let work = stream
                            .work
                            .entry(run.unwrap_or_default().into())
                            .or_default();
                        let room = SUMMARY_SCALARS.saturating_sub(work.draft.chars().count());
                        work.draft.extend(text.chars().take(room));
                    }
                    let turn = stream.turns.entry(run_key.clone()).or_insert(1);
                    *turn
                };
                self.response_row(
                    agent,
                    run,
                    turn,
                    offset,
                    timestamp,
                    truncate(text),
                    RowStatus::Pending,
                );
            }
            AgentEvent::AssistantFinished { text } => {
                let turn = {
                    let stream = self.streams.entry(agent.to_owned()).or_default();
                    let work = stream
                        .work
                        .entry(run.unwrap_or_default().into())
                        .or_default();
                    if !work.draft.trim().is_empty() {
                        // The tool started after AssistantFinished, so this turn's summary is kept for the whole tool batch.
                        work.latest.clone_from(&work.draft);
                    }
                    let turn = stream.turns.entry(run_key).or_insert(1);
                    if *turn == 0 {
                        *turn = 1;
                    }
                    *turn
                };
                // A pure tool-call turn has no body, so no empty response row is kept.
                let key = format!("{agent}|{}|response|{turn}", run.unwrap_or("-"));
                if text.trim().is_empty()
                    && self
                        .keys
                        .get(&key)
                        .is_some_and(|index| self.rows[*index].summary.trim().is_empty())
                {
                    self.drop_row(&key);
                    return;
                }
                self.response_row(
                    agent,
                    run,
                    turn,
                    offset,
                    timestamp,
                    truncate(text),
                    RowStatus::Success,
                );
            }
            AgentEvent::ToolStarted {
                id,
                name,
                arguments,
            } => {
                let summary = summarize_arguments(arguments);
                let stream = self.streams.entry(agent.to_owned()).or_default();
                let work = stream
                    .work
                    .entry(run.unwrap_or_default().into())
                    .or_default();
                let brief = truncate(work.draft.trim());
                work.tool = if summary.is_empty() {
                    tool_label(name).into_owned()
                } else {
                    summary.clone()
                };
                self.upsert(
                    ActivityRow {
                        key: String::new(),
                        agent: agent.to_owned(),
                        run: run.map(str::to_owned),
                        tool: Some(id.clone()),
                        offset,
                        timestamp,
                        kind: Kind::Tool,
                        status: RowStatus::Pending,
                        title: name.clone(),
                        summary: truncate(arguments),
                        brief: Some(brief),
                        phase: 1,
                    },
                    tool_key(agent, run, id),
                );
            }
            AgentEvent::ToolFinished { id, result } => {
                let ok = result["ok"] != serde_json::Value::Bool(false);
                let message = result["error"].as_str().unwrap_or("tool failed").to_owned();
                if !ok {
                    self.streams.entry(agent.to_owned()).or_default().last_error =
                        Some(message.clone());
                }
                let key = tool_key(agent, run, id);
                if self.keys.contains_key(&key) {
                    self.upsert(
                        ActivityRow {
                            key: String::new(),
                            agent: agent.to_owned(),
                            run: run.map(str::to_owned),
                            tool: Some(id.clone()),
                            offset,
                            timestamp,
                            kind: Kind::Tool,
                            status: if ok {
                                RowStatus::Success
                            } else {
                                RowStatus::Error
                            },
                            title: String::new(),
                            summary: if ok {
                                String::new()
                            } else {
                                truncate(&message)
                            },
                            brief: None,
                            phase: 1,
                        },
                        key,
                    );
                } else {
                    // Legacy log without ToolStarted: add a terminal row instead of forging a start position.
                    self.upsert(
                        ActivityRow {
                            key: String::new(),
                            agent: agent.to_owned(),
                            run: run.map(str::to_owned),
                            tool: Some(id.clone()),
                            offset,
                            timestamp,
                            kind: Kind::Tool,
                            status: if ok {
                                RowStatus::Success
                            } else {
                                RowStatus::Error
                            },
                            title: "tool".into(),
                            summary: if ok {
                                String::new()
                            } else {
                                truncate(&message)
                            },
                            brief: None,
                            phase: 1,
                        },
                        key,
                    );
                }
            }
            AgentEvent::ToolOutput { .. }
            | AgentEvent::ToolArgumentsDelta { .. }
            | AgentEvent::ToolOutputSnapshot { .. }
            | AgentEvent::ToolPipelineSnapshot { .. }
            | AgentEvent::ToolShellSnapshot { .. }
            | AgentEvent::AutoReviewStarted { .. }
            | AgentEvent::AutoReviewFinished { .. } => {}
            AgentEvent::UserInputAccepted { text, .. } => self.insert_lifecycle(
                agent,
                run,
                offset,
                timestamp,
                "Input accepted",
                truncate(text),
                RowStatus::Success,
                1,
            ),
            AgentEvent::ModelRequestFinished { success, usage, .. } => {
                if let Some(usage) = usage {
                    let stream = self.streams.entry(agent.to_owned()).or_default();
                    stream.usage_in = stream.usage_in.saturating_add(usage.input_tokens);
                    stream.usage_out = stream.usage_out.saturating_add(usage.output_tokens);
                    stream.usage_seen = true;
                }
                if !success {
                    self.streams.entry(agent.to_owned()).or_default().last_error =
                        Some("model request failed".into());
                    self.insert_lifecycle(
                        agent,
                        run,
                        offset,
                        timestamp,
                        "Request failed",
                        "model request failed".into(),
                        RowStatus::Error,
                        1,
                    );
                }
            }
            AgentEvent::ModelAttemptFailed {
                attempt,
                model,
                error,
            } => {
                if let Some(work) = self
                    .streams
                    .get_mut(agent)
                    .and_then(|stream| stream.work.get_mut(run.unwrap_or_default()))
                {
                    work.draft.clear();
                }
                self.insert_lifecycle(
                    agent,
                    run,
                    offset,
                    timestamp,
                    "Failed attempt",
                    format!("Attempt {attempt} failed; partial output is excluded from history\nModel: {model}\nReason: {error}"),
                    RowStatus::Error,
                    1,
                );
            }
            AgentEvent::ModelRouteChanged { model, .. } => {
                self.insert_lifecycle(
                    agent,
                    run,
                    offset,
                    timestamp,
                    "Fallback",
                    model.clone(),
                    RowStatus::Pending,
                    1,
                );
            }
            AgentEvent::RequestRetrying {
                reason,
                attempt,
                wait_ms,
            } => {
                let summary = format!(
                    "retrying {reason}, attempt {attempt}, wait {}",
                    crate::tui::format_wait(*wait_ms)
                );
                self.insert_lifecycle(
                    agent,
                    run,
                    offset,
                    timestamp,
                    "Retry",
                    summary,
                    RowStatus::Pending,
                    1,
                );
            }
            AgentEvent::ContextUpdated { usage } => {
                self.streams.entry(agent.to_owned()).or_default().context = Some(*usage);
            }
            AgentEvent::ModelRequestStarted => {}
            AgentEvent::LspDiagnostics { id, report } => {
                // The projection is isolated per agent/run stream: only the reports owned by that subagent are shown.
                let total = report.total();
                let summary = format!(
                    "{} · {} {}",
                    report.path,
                    total,
                    if total == 1 {
                        "diagnostic"
                    } else {
                        "diagnostics"
                    }
                );
                self.insert_lifecycle(
                    agent,
                    run,
                    offset,
                    timestamp,
                    if id.is_none() {
                        "LSP diagnostics (late)"
                    } else {
                        "LSP diagnostics"
                    },
                    truncate(&summary),
                    if report.has_issues() {
                        RowStatus::Error
                    } else if report.all_clean() {
                        RowStatus::Success
                    } else {
                        RowStatus::Pending
                    },
                    1,
                );
            }
            AgentEvent::TodoStateUpdated { .. }
            | AgentEvent::TodoReminder { .. }
            | AgentEvent::TodoSnapshot { .. }
            | AgentEvent::PlanProgressUpdated { .. }
            | AgentEvent::CompactionStarted { .. }
            | AgentEvent::CompactionFinished { .. }
            | AgentEvent::ModelOutputProgress { .. }
            | AgentEvent::SubagentResultsInjected { .. }
            | AgentEvent::ProjectInstructionsLoaded { .. }
            // Transient hook diagnostics and input rejections never enter the subagent event stream; no row is kept here.
            | AgentEvent::HookDiagnostic { .. }
            | AgentEvent::UserInputRejected { .. }
            | AgentEvent::FileChanged { .. } => {}
        }
    }

    /// One response per assistant turn; deltas update the same short summary in place.
    #[allow(clippy::too_many_arguments)]
    fn response_row(
        &mut self,
        agent: &str,
        run: Option<&str>,
        turn: u64,
        offset: u64,
        timestamp: Option<u64>,
        summary: String,
        status: RowStatus,
    ) {
        let key = format!("{agent}|{}|response|{turn}", run.unwrap_or("-"));
        if let Some(index) = self.keys.get(&key).copied() {
            let existing = &mut self.rows[index];
            existing.status = status;
            if !summary.trim().is_empty() {
                existing.summary = summary;
            }
            return;
        }
        self.upsert(
            ActivityRow {
                key: String::new(),
                agent: agent.to_owned(),
                run: run.map(str::to_owned),
                tool: None,
                offset,
                timestamp,
                kind: Kind::Response,
                status,
                title: format!("Response {turn}"),
                summary,
                brief: None,
                phase: 1,
            },
            key,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_lifecycle(
        &mut self,
        agent: &str,
        run: Option<&str>,
        offset: u64,
        timestamp: Option<u64>,
        title: &str,
        summary: String,
        status: RowStatus,
        phase: u8,
    ) {
        self.upsert(
            ActivityRow {
                key: String::new(),
                agent: agent.to_owned(),
                run: run.map(str::to_owned),
                tool: None,
                offset,
                timestamp,
                kind: Kind::Lifecycle,
                status,
                title: title.to_owned(),
                summary,
                brief: None,
                phase,
            },
            format!("{agent}|{}|lifecycle|{title}|{offset}", run.unwrap_or("-")),
        );
    }

    fn upsert(&mut self, mut row: ActivityRow, key: String) {
        if let Some(index) = self.keys.get(&key).copied() {
            // The update branch only changes status/summary/brief (non-sort fields) and never marks the table dirty;
            // any path that rewrites a sort field such as offset/timestamp/phase/agent must set `sorted = false`.
            let target = &mut self.rows[index];
            target.status = row.status;
            if !row.summary.is_empty() {
                target.summary = row.summary;
            }
            if row.brief.is_some() {
                target.brief = row.brief;
            }
            return;
        }
        row.key = key.clone();
        self.keys.insert(key, self.rows.len());
        // Invariant: inserting a row must mark the table dirty, the sort key contains offset, and a missing re-sort breaks row order.
        self.rows.push(row);
        self.sorted = false;
    }

    fn drop_row(&mut self, key: &str) {
        if let Some(index) = self.keys.remove(key) {
            self.rows.remove(index);
            // Invariant: changing offset must mark the table dirty - removal shifts the indexes of later rows, so keys are rebuilt before the re-sort.
            self.reindex();
            self.sorted = false;
        }
    }

    pub(super) fn reindex(&mut self) {
        self.keys = self
            .rows
            .iter()
            .enumerate()
            .map(|(index, row)| (row.key.clone(), index))
            .collect();
    }

    /// Synthesize the task association and terminal lifecycle missing from old logs; a new log with markers is not duplicated.
    /// Idempotent: when neither end is missing the rows scan is skipped and no full-table re-sort is triggered.
    pub(super) fn finalize(&mut self) {
        if !self.needs_finalize {
            return;
        }
        self.needs_finalize = false;
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        // Immutable pre-filter: no deep copy of the whole snapshot, only the small run fields that really need synthesis.
        let mut pending = Vec::new();
        for run in &snapshot.runs {
            let Some(stream) = self.streams.get(&run.agent_id) else {
                continue;
            };
            if !self.scanned.contains(&run.agent_id) {
                continue;
            }
            // First criterion: the run in the new log saw a RunState marker, so the real row already exists.
            if stream.markers.contains(&run.id) {
                continue;
            }
            // Second criterion: idempotence check, no bounds are computed when neither end is missing.
            let needs_task = !stream.legacy_task.contains(&run.id);
            let terminal =
                terminal_of(&run.status).filter(|_| !stream.legacy_terminal.contains(&run.id));
            if !needs_task && terminal.is_none() {
                continue;
            }
            // Only the missing end gets its endpoints computed; first/last are exactly the
            // min/max of the existing rows, and the synthesized row never advances the bounds itself.
            let mut first = u64::MAX;
            let mut last = 0;
            let mut seen = false;
            for row in self.rows.iter().filter(|row| {
                row.agent == run.agent_id && row.run.as_deref() == Some(run.id.as_str())
            }) {
                seen = true;
                if needs_task {
                    first = first.min(row.offset);
                }
                if terminal.is_some() {
                    last = last.max(row.offset);
                }
            }
            if !seen {
                continue;
            }
            pending.push(LegacyRun {
                agent: run.agent_id.clone(),
                run: run.id.clone(),
                task: run.task.clone(),
                needs_task,
                terminal,
                first,
                last,
            });
        }
        // Insert after releasing the snapshot borrow; insertions go through upsert and mark the table dirty, while no insertion means no re-sort.
        for item in &pending {
            if item.needs_task {
                self.insert_lifecycle(
                    &item.agent,
                    Some(&item.run),
                    item.first,
                    None,
                    "Task",
                    truncate(&item.task),
                    RowStatus::Success,
                    0,
                );
                self.streams
                    .entry(item.agent.clone())
                    .or_default()
                    .legacy_task
                    .insert(item.run.clone());
            }
            if let Some((status, label)) = item.terminal {
                self.insert_lifecycle(
                    &item.agent,
                    Some(&item.run),
                    item.last.saturating_add(1),
                    None,
                    "Run",
                    label.into(),
                    status,
                    2,
                );
                self.streams
                    .entry(item.agent.clone())
                    .or_default()
                    .legacy_terminal
                    .insert(item.run.clone());
            }
        }
    }
}

/// Pending single-run synthesis for legacy logs: the pre-filter copies only small fields so the snapshot is not cloned wholesale.
struct LegacyRun {
    agent: String,
    run: String,
    task: String,
    /// The task association row is still missing: inserted at min(offset) of the existing rows.
    needs_task: bool,
    /// Terminal row (with status and label); only synthesized when Some, inserted at max(offset) + 1 of the existing rows.
    terminal: Option<(RowStatus, &'static str)>,
    first: u64,
    last: u64,
}

/// Mapping from a legacy run status to a terminal row; a non-terminal status synthesizes no Run row.
fn terminal_of(status: &str) -> Option<(RowStatus, &'static str)> {
    match status {
        "completed" => Some((RowStatus::Success, "completed")),
        "failed" => Some((RowStatus::Error, "failed")),
        "interrupted" => Some((RowStatus::Error, "interrupted")),
        "cancelled" => Some((RowStatus::Aborted, "cancelled")),
        _ => None,
    }
}

fn tool_key(agent: &str, run: Option<&str>, id: &str) -> String {
    format!("{agent}|{}|tool|{id}", run.unwrap_or("-"))
}

#[cfg(test)]
mod tests;
