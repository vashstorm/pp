//! Agent 视图重建：排序权重、指标与耗时。

use super::*;

impl Projection {
    /// 冻结首次出现的 (id, generation) 排序权重；新批次整体插到前面。
    pub(super) fn freeze_ranks(&mut self, snapshot: &HubSnapshot) {
        let mut fresh = snapshot
            .agents
            .iter()
            .filter(|agent| {
                !self
                    .ranks
                    .contains_key(&(agent.record.id.clone(), agent.record.generation))
            })
            .collect::<Vec<_>>();
        if fresh.is_empty() {
            return;
        }
        fresh.sort_by(|left, right| {
            (
                status_of(snapshot, &left.record.id),
                std::cmp::Reverse(left.record.updated_ms),
                agent_number(&left.record.id),
            )
                .cmp(&(
                    status_of(snapshot, &right.record.id),
                    std::cmp::Reverse(right.record.updated_ms),
                    agent_number(&right.record.id),
                ))
        });
        let count = fresh.len() as u64;
        for (index, agent) in fresh.iter().enumerate() {
            let weight = self.next_rank + count - index as u64;
            self.ranks
                .insert((agent.record.id.clone(), agent.record.generation), weight);
        }
        self.next_rank += count;
    }

    pub(super) fn rebuild_agents(&mut self, snapshot: &HubSnapshot, now_ms: u64) {
        let mut views = snapshot
            .agents
            .iter()
            .map(|agent| {
                let record = &agent.record;
                let runs = snapshot
                    .runs
                    .iter()
                    .filter(|run| run.agent_id == record.id)
                    .collect::<Vec<&Run>>();
                let stream = self.streams.get(&record.id);
                let status = status_of(snapshot, &record.id);
                let run_status = runs
                    .iter()
                    .find(|run| matches!(run.status.as_str(), "queued" | "running"))
                    .map(|run| run.status.clone());
                let latest = runs.iter().max_by_key(|run| run_number(&run.id));
                let task = latest.map(|run| run.task.clone());
                let current = stream.and_then(|stream| {
                    let key = latest.map_or("", |run| run.id.as_str());
                    stream
                        .work
                        .get(key)
                        .or_else(|| stream.work.get(""))
                        .and_then(WorkSummary::current)
                });
                let (active_ms, span_ms) = timings(&runs, stream, now_ms);
                let requests = runs.iter().map(|run| run.requests).sum();
                let tokens_from_runs = runs.iter().map(|run| run.tokens).sum::<u64>();
                let usage = stream.filter(|stream| stream.usage_seen);
                let tokens = usage
                    .map(|stream| {
                        stream
                            .usage_in
                            .saturating_add(stream.usage_out)
                            .max(tokens_from_runs)
                    })
                    .or_else(|| (tokens_from_runs > 0).then_some(tokens_from_runs));
                let cost_measured = runs.iter().filter(|run| run.cost.is_some()).count();
                let cost = (cost_measured > 0)
                    .then(|| runs.iter().filter_map(|run| run.cost).sum::<f64>());
                let mut time = Some(
                    runs.iter()
                        .filter(|run| !matches!(run.status.as_str(), "queued" | "running"))
                        .map(|run| run.duration_ms)
                        .sum::<u64>(),
                );
                if let Some(stream) = stream {
                    for run in runs
                        .iter()
                        .filter(|run| matches!(run.status.as_str(), "queued" | "running"))
                    {
                        match stream.started.get(&run.id) {
                            Some(started) => {
                                time = time.map(|total| {
                                    total.saturating_add(now_ms.saturating_sub(*started))
                                });
                            }
                            None => time = None,
                        }
                    }
                }
                let artifacts = runs
                    .iter()
                    .filter(|run| run.result.is_some())
                    .max_by_key(|run| run_number(&run.id))
                    .map(|run| {
                        let mut artifacts = vec![format!("agent://{}", record.id)];
                        if let Some(patches) = run
                            .result
                            .as_ref()
                            .and_then(|result| result["patches"].as_array())
                        {
                            for patch in patches {
                                if let Some(path) = patch.as_str() {
                                    artifacts.push(path.to_owned());
                                }
                            }
                        }
                        artifacts
                    })
                    .unwrap_or_default();
                AgentView {
                    id: record.id.clone(),
                    name: record.name.clone(),
                    definition: record.spec.definition.clone(),
                    role: record.spec.model_role.map(|role| role.name()),
                    registered_ms: record.registered_ms,
                    parent: record.parent.clone(),
                    model: record.spec.model.clone(),
                    cwd: record.spec.cwd.clone(),
                    thinking: record.spec.thinking.as_str().to_owned(),
                    model_change: agent.model_change.clone(),
                    status,
                    run_status,
                    task,
                    current,
                    requests,
                    tokens,
                    cost,
                    cost_measured,
                    runs: runs.len(),
                    tools: 0,
                    time_ms: time,
                    active_ms,
                    span_ms,
                    loading: !self.scanned.contains(&record.id),
                    context: stream.and_then(|stream| stream.context),
                    readonly: record.spec.readonly,
                    isolated: record.spec.isolated,
                    artifacts,
                    last_error: stream.and_then(|stream| stream.last_error.clone()),
                    last_activity: stream
                        .and_then(|stream| stream.last_activity)
                        .or(Some(record.updated_ms)),
                }
            })
            .collect::<Vec<_>>();
        for view in &mut views {
            view.tools = self
                .rows
                .iter()
                .filter(|row| row.agent == view.id && row.kind == Kind::Tool)
                .count();
        }
        views.sort_by(|left, right| {
            let weight = |view: &AgentView| {
                self.ranks
                    .get(&(view.id.clone(), generation_of(snapshot, &view.id)))
                    .copied()
                    .unwrap_or(0)
            };
            weight(right).cmp(&weight(left))
        });
        self.index = views
            .iter()
            .enumerate()
            .map(|(index, view)| (view.id.clone(), index))
            .collect();
        self.agents = views;
    }
}

fn timings(runs: &[&Run], stream: Option<&Stream>, now_ms: u64) -> (Option<u64>, Option<u64>) {
    let Some(stream) = stream else {
        return (None, None);
    };
    let mut active = None::<u64>;
    let mut first = u64::MAX;
    let mut last = 0;
    let mut span_known = true;
    for run in runs {
        let start = stream.started.get(&run.id).copied();
        let end = stream.ended.get(&run.id).copied();
        let stop = end.or_else(|| (run.status == "running").then_some(now_ms));
        if let Some((start, stop)) = start.zip(stop).filter(|(start, stop)| start <= stop) {
            active = Some(active.unwrap_or(0).saturating_add(stop - start));
            first = first.min(start);
            if let Some(end) = end {
                last = last.max(end);
            }
        } else if !matches!(run.status.as_str(), "queued" | "cancelled") || start.is_some() {
            span_known = false;
        }
    }
    (
        active,
        span_known.then(|| last.checked_sub(first)).flatten(),
    )
}

fn generation_of(snapshot: &HubSnapshot, id: &str) -> u64 {
    snapshot
        .agents
        .iter()
        .find(|agent| agent.record.id == id)
        .map_or(0, |agent| agent.record.generation)
}

fn status_of(snapshot: &HubSnapshot, id: &str) -> AgentStatus {
    let record = snapshot
        .agents
        .iter()
        .find(|agent| agent.record.id == id)
        .map(|agent| &agent.record);
    // aborted 的 Agent 状态优先。
    if record.is_some_and(|record| record.status == "aborted") {
        return AgentStatus::Aborted;
    }
    if snapshot
        .runs
        .iter()
        .any(|run| run.agent_id == id && matches!(run.status.as_str(), "queued" | "running"))
    {
        return AgentStatus::Running;
    }
    match record.map(|record| record.status.as_str()) {
        Some("parked") => AgentStatus::Parked,
        Some("aborted") => AgentStatus::Aborted,
        _ => AgentStatus::Idle,
    }
}

pub(super) fn agent_number(id: &str) -> u64 {
    id.strip_prefix('a')
        .and_then(|rest| rest.parse().ok())
        .unwrap_or(u64::MAX)
}

pub(super) fn run_number(id: &str) -> u64 {
    id.strip_prefix('r')
        .and_then(|rest| rest.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;

    #[test]
    fn unknown_cost_and_tokens_stay_unknown() {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "idle")],
                vec![run("r1", "a1", "completed", "work")],
            ),
            1,
        );
        projection.ingest("a1", &page(Vec::new(), false), 10);
        let view = &projection.agents()[0];
        assert_eq!(view.cost, None, "未知 cost 不能补零");
        assert_eq!(view.tokens, None, "0 tokens 且无 usage 证据显示未知");
        assert_eq!(view.time_ms, Some(0));
        // 旧版只有 Run.tokens 时按总量展示。
        let mut legacy = Projection::default();
        let mut completed = run("r1", "a1", "completed", "work");
        completed.tokens = 5_000;
        legacy.apply_snapshot(
            snapshot(vec![record("a1", "Main", "idle")], vec![completed]),
            1,
        );
        assert_eq!(legacy.agents()[0].tokens, Some(5_000));
    }
}
