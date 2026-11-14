//! 观察记录摄入：Activity 行的生成、合并与旧日志合成。

use super::text::summarize_arguments;
use super::*;

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
                        // 工具在 AssistantFinished 后启动，保留本轮摘要供整批工具使用。
                        work.latest.clone_from(&work.draft);
                    }
                    let turn = stream.turns.entry(run_key).or_insert(1);
                    if *turn == 0 {
                        *turn = 1;
                    }
                    *turn
                };
                // 纯工具调用的 turn 没有正文，不保留空 response 行。
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
                    name.clone()
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
                    // 旧日志缺 ToolStarted：补一条终态行，不伪造开始位置。
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
            | AgentEvent::ToolShellSnapshot { .. } => {}
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
                // 投影按 agent/run 流隔离：只显示该子 Agent owner 的报告。
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
            | AgentEvent::ProjectInstructionsLoaded { .. }
            // 瞬时 hook 诊断与输入拒绝不进入子 Agent 事件流；这里保持无行。
            | AgentEvent::HookDiagnostic { .. }
            | AgentEvent::UserInputRejected { .. }
            | AgentEvent::FileChanged { .. } => {}
        }
    }

    /// 一个 assistant turn 一条 response；delta 原位更新同一短摘要。
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
        self.rows.push(row);
        self.sorted = false;
    }

    fn drop_row(&mut self, key: &str) {
        if let Some(index) = self.keys.remove(key) {
            self.rows.remove(index);
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

    /// 合成旧日志缺失的 task 关联与终态 lifecycle；新日志有标记不重复。
    pub(super) fn finalize(&mut self) {
        if !self.needs_finalize {
            return;
        }
        self.needs_finalize = false;
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        let runs = snapshot.runs.clone();
        for run in runs {
            let agent = run.agent_id.clone();
            if !self.scanned.contains(&agent) || !self.streams.contains_key(&agent) {
                continue;
            }
            if self
                .streams
                .get(&agent)
                .is_some_and(|stream| stream.markers.contains(&run.id))
            {
                continue;
            }
            let bounds = self
                .rows
                .iter()
                .filter(|row| row.agent == agent && row.run.as_deref() == Some(run.id.as_str()))
                .fold(None, |bounds: Option<(u64, u64)>, row| {
                    Some(match bounds {
                        None => (row.offset, row.offset),
                        Some((low, high)) => (low.min(row.offset), high.max(row.offset)),
                    })
                });
            let Some((first, last)) = bounds else {
                continue;
            };
            if !self
                .streams
                .get(&agent)
                .is_some_and(|stream| stream.legacy_task.contains(&run.id))
            {
                self.insert_lifecycle(
                    &agent,
                    Some(&run.id),
                    first,
                    None,
                    "Task",
                    truncate(&run.task),
                    RowStatus::Success,
                    0,
                );
                self.streams
                    .entry(agent.clone())
                    .or_default()
                    .legacy_task
                    .insert(run.id.clone());
            }
            let terminal = match run.status.as_str() {
                "completed" => Some((RowStatus::Success, "completed")),
                "failed" => Some((RowStatus::Error, "failed")),
                "interrupted" => Some((RowStatus::Error, "interrupted")),
                "cancelled" => Some((RowStatus::Aborted, "cancelled")),
                _ => None,
            };
            if let Some((status, label)) = terminal
                && !self
                    .streams
                    .get(&agent)
                    .is_some_and(|stream| stream.legacy_terminal.contains(&run.id))
            {
                self.insert_lifecycle(
                    &agent,
                    Some(&run.id),
                    last.saturating_add(1),
                    None,
                    "Run",
                    label.into(),
                    status,
                    2,
                );
                self.streams
                    .entry(agent.clone())
                    .or_default()
                    .legacy_terminal
                    .insert(run.id.clone());
            }
        }
        // 合成行插入后需要重排。
        self.sorted = false;
    }
}

fn tool_key(agent: &str, run: Option<&str>, id: &str) -> String {
    format!("{agent}|{}|tool|{id}", run.unwrap_or("-"))
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use pp_core::events::AgentEvent;
    use pp_core::events::OutputStream;

    #[test]
    fn response_and_tool_update_in_place_across_runs() {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "idle")],
                vec![run("r1", "a1", "running", "work")],
            ),
            1,
        );
        let entries = vec![
            entry(0, Some(10), "r1", AgentEvent::AssistantStarted),
            entry(60, Some(11), "r1", delta("hello")),
            entry(120, Some(12), "r1", delta(" world")),
            entry(
                180,
                Some(13),
                "r1",
                AgentEvent::AssistantFinished {
                    text: "hello world".into(),
                },
            ),
            entry(
                240,
                Some(14),
                "r1",
                AgentEvent::ToolStarted {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"src/main.rs"}"#.into(),
                },
            ),
            entry(
                300,
                Some(15),
                "r1",
                AgentEvent::ToolFinished {
                    id: "c1".into(),
                    result: serde_json::json!({"ok":false,"error":"missing file"}),
                },
            ),
            entry(
                360,
                Some(16),
                "r1",
                AgentEvent::ToolOutput {
                    id: "c1".into(),
                    stream: OutputStream::Stdout,
                    text: "ignored".into(),
                },
            ),
        ];
        projection.ingest("a1", &page(entries, false), 1_000);
        let rows = projection.rows();
        assert_eq!(
            rows.iter().filter(|row| row.kind == Kind::Response).count(),
            1,
            "一个 assistant turn 只产生一行"
        );
        let response = rows.iter().find(|row| row.kind == Kind::Response).unwrap();
        assert_eq!(response.summary, "hello world");
        assert_eq!(response.status, RowStatus::Success);
        let tools = rows
            .iter()
            .filter(|row| row.kind == Kind::Tool)
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 1, "ToolOutput 不产生新活动行");
        assert_eq!(tools[0].status, RowStatus::Error);
        assert_eq!(tools[0].summary, "missing file");
        assert_eq!(projection.recent("a1", 1)[0].summary, "missing file");
        assert_eq!(projection.agents()[0].tools, 1);
        // 新 Run 复用相同 call id 必须生成独立行。
        let more = vec![entry(
            420,
            Some(20),
            "r2",
            AgentEvent::ToolStarted {
                id: "c1".into(),
                name: "read".into(),
                arguments: "{}".into(),
            },
        )];
        projection.ingest("a1", &page(more, false), 2_000);
        assert_eq!(
            projection
                .rows()
                .iter()
                .filter(|row| row.kind == Kind::Tool)
                .count(),
            2
        );
    }

    #[test]
    fn legacy_rows_keep_file_order_and_synthesize_once() {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "idle")],
                vec![run("r1", "a1", "failed", "legacy work")],
            ),
            1,
        );
        // 旧日志：只有事件、无 timestamp、无 RunState。
        let entries = vec![
            entry(0, None, "r1", AgentEvent::AssistantStarted),
            entry(
                90,
                None,
                "r1",
                AgentEvent::AssistantFinished {
                    text: "legacy answer".into(),
                },
            ),
        ];
        projection.ingest("a1", &page(entries, false), 10);
        projection.ingest("a1", &page(Vec::new(), false), 10);
        let rows = projection.rows();
        assert_eq!(
            rows.iter().filter(|row| row.title == "Task").count(),
            1,
            "旧日志只合成一次 task 关联"
        );
        assert_eq!(
            rows.iter().filter(|row| row.title == "Run").count(),
            1,
            "旧日志只合成一次终态"
        );
        let task = rows.iter().position(|row| row.title == "Task").unwrap();
        let first_event = rows
            .iter()
            .position(|row| row.kind == Kind::Response)
            .unwrap();
        assert!(task < first_event, "task 关联排在 Run 第一条事件之前");
        let terminal = rows.iter().find(|row| row.title == "Run").unwrap();
        assert_eq!(terminal.status, RowStatus::Error);
        assert_eq!(terminal.timestamp, None, "合成行不带伪造时间");
        let without = rows.iter().filter(|row| row.timestamp.is_none()).count();
        let with = rows.iter().filter(|row| row.timestamp.is_some()).count();
        assert!(with <= 1 && without >= 3);
        // 无时间记录排在有时间记录之前。
        let first_ts = rows.iter().position(|row| row.timestamp.is_some());
        if let Some(first_ts) = first_ts {
            assert!(rows[..first_ts].iter().all(|row| row.timestamp.is_none()));
        }
    }

    #[test]
    fn activity_row_ids_are_stable_across_ingest() {
        let mut projection = Projection::default();
        projection.apply_snapshot(snapshot(vec![record("a1", "Main", "idle")], vec![]), 1);
        projection.ingest(
            "a1",
            &page(
                vec![entry(0, Some(10), "r1", AgentEvent::AssistantStarted)],
                false,
            ),
            10,
        );
        let id = projection.rows()[0].id();
        projection.ingest(
            "a1",
            &page(vec![entry(30, Some(11), "r1", delta("more"))], false),
            20,
        );
        assert_eq!(projection.rows().len(), 1);
        assert_eq!(projection.rows()[0].id(), id);
    }
}
