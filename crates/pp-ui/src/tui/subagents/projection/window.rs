//! 列表与 Activity 的可见窗口：排序、树遍历、scope 与搜索。

use super::summary::{agent_number, run_number};
use super::text::fuzzy;
use super::*;

impl Projection {
    pub(super) fn sort_rows(&mut self) {
        self.rows.sort_by_key(row_sort_key);
        self.reindex();
        self.sorted = true;
    }

    /// Agents 列表：扁平或 parent-first DFS；过滤只匹配 id/name（子序列）。
    pub(in crate::tui::subagents) fn list(&self, tree: bool, filter: &str) -> Vec<TreeRow> {
        let needle = filter.trim().to_lowercase();
        let matches = |agent: &AgentView| {
            needle.is_empty()
                || fuzzy(&needle, &agent.id.to_lowercase())
                || fuzzy(&needle, &agent.name.to_lowercase())
        };
        if !tree {
            return self
                .agents
                .iter()
                .enumerate()
                .filter(|(_, agent)| matches(agent))
                .map(|(index, _)| TreeRow {
                    agent: index,
                    prefix: String::new(),
                    muted: false,
                })
                .collect();
        }
        let children = self.children();
        // 匹配项的祖先作为 lineage 保留。
        let mut keep = BTreeSet::new();
        for (index, agent) in self.agents.iter().enumerate() {
            if matches(agent) {
                keep.insert(index);
                let mut parent = agent.parent.clone();
                while let Some(parent_index) = self.index.get(&parent).copied() {
                    if !keep.insert(parent_index) {
                        break;
                    }
                    parent = self.agents[parent_index].parent.clone();
                }
            }
        }
        let matching = self.agents.iter().map(matches).collect::<Vec<bool>>();
        let mut rows = Vec::new();
        let mut visited = BTreeSet::new();
        for root in self.roots() {
            self.walk(
                root,
                &children,
                &keep,
                &matching,
                &mut visited,
                &mut rows,
                "",
            );
        }
        // 孤立 parent 或环防护：未访问项追加为根，不丢行。
        for index in 0..self.agents.len() {
            if !visited.contains(&index) {
                self.walk(
                    index,
                    &children,
                    &keep,
                    &matching,
                    &mut visited,
                    &mut rows,
                    "",
                );
            }
        }
        rows
    }

    fn children(&self) -> BTreeMap<usize, Vec<usize>> {
        let mut children: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (index, agent) in self.agents.iter().enumerate() {
            if let Some(parent) = self.index.get(&agent.parent) {
                children.entry(*parent).or_default().push(index);
            }
        }
        children
    }

    fn roots(&self) -> Vec<usize> {
        self.agents
            .iter()
            .enumerate()
            .filter(|(_, agent)| !self.index.contains_key(&agent.parent))
            .map(|(index, _)| index)
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        index: usize,
        children: &BTreeMap<usize, Vec<usize>>,
        keep: &BTreeSet<usize>,
        matching: &[bool],
        visited: &mut BTreeSet<usize>,
        rows: &mut Vec<TreeRow>,
        prefix: &str,
    ) {
        if !visited.insert(index) {
            return;
        }
        if keep.contains(&index) {
            rows.push(TreeRow {
                agent: index,
                prefix: prefix.to_owned(),
                muted: !matching[index],
            });
        }
        let continuation = match prefix.strip_suffix("├─ ") {
            Some(head) => format!("{head}│  "),
            None => match prefix.strip_suffix("└─ ") {
                Some(head) => format!("{head}   "),
                None => prefix.to_owned(),
            },
        };
        let kids = children
            .get(&index)
            .into_iter()
            .flatten()
            .filter(|child| keep.contains(child))
            .copied()
            .collect::<Vec<_>>();
        let count = kids.len();
        for (slot, child) in kids.into_iter().enumerate() {
            let branch = format!(
                "{continuation}{}",
                if slot + 1 == count {
                    "└─ "
                } else {
                    "├─ "
                }
            );
            self.walk(child, children, keep, matching, visited, rows, &branch);
        }
    }

    /// 可见 Activity 行：类别 + scope + 搜索；scope 锚点不随活动行选择漂移。
    pub(in crate::tui::subagents) fn activity(
        &self,
        category: Category,
        scope: Scope,
        anchor: Option<&str>,
        query: &str,
    ) -> Vec<usize> {
        let query = query.to_lowercase();
        let subtree = anchor.map(|anchor| self.subtree(anchor));
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| match category {
                Category::All => true,
                Category::Errors => row.status == RowStatus::Error,
                Category::Responses => row.kind == Kind::Response,
                Category::Tools => row.kind == Kind::Tool,
            })
            .filter(|(_, row)| match scope {
                Scope::All => true,
                Scope::Selected => anchor.is_some_and(|anchor| row.agent == anchor),
                Scope::Subtree => subtree
                    .as_ref()
                    .is_some_and(|subtree| subtree.contains(&row.agent)),
            })
            .filter(|(_, row)| {
                if query.is_empty() {
                    return true;
                }
                let name = self
                    .agent(&row.agent)
                    .map(|agent| agent.name.as_str())
                    .unwrap_or("");
                format!("{} {} {} {}", row.agent, name, row.title, row.summary)
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn subtree(&self, anchor: &str) -> BTreeSet<String> {
        let mut members = BTreeSet::new();
        members.insert(anchor.to_owned());
        loop {
            let mut added = false;
            for agent in &self.agents {
                if members.contains(&agent.parent) && members.insert(agent.id.clone()) {
                    added = true;
                }
            }
            if !added {
                return members;
            }
        }
    }

    /// 概要只保留工具与已发布的最终答复，独立 Activity 页仍使用完整事件流。
    pub(in crate::tui::subagents) fn recent(&self, agent: &str, limit: usize) -> Vec<ActivityRow> {
        let mut rows = self
            .rows
            .iter()
            .rev()
            .filter(|row| row.agent == agent && row.kind == Kind::Tool)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(snapshot) = &self.snapshot {
            let stream = self.streams.get(agent);
            let mut runs = snapshot
                .runs
                .iter()
                .filter(|run| run.agent_id == agent && run.status == "completed")
                .collect::<Vec<_>>();
            runs.sort_by_key(|run| std::cmp::Reverse(run_number(&run.id)));
            rows.extend(runs.into_iter().take(limit).filter_map(|run| {
                let output = run.result.as_ref()?.get("output")?;
                let summary = match output.as_str() {
                    Some(text) => truncate(text),
                    None => truncate(&output.to_string()),
                };
                if summary.is_empty() {
                    return None;
                }
                let timestamp = stream
                    .and_then(|stream| stream.ended.get(&run.id).copied())
                    .or_else(|| {
                        self.rows.iter().rev().find_map(|row| {
                            (row.agent == agent && row.run.as_deref() == Some(&run.id))
                                .then_some(row.timestamp)
                                .flatten()
                        })
                    });
                Some(ActivityRow {
                    key: String::new(),
                    agent: agent.to_owned(),
                    run: Some(run.id.clone()),
                    tool: None,
                    offset: u64::MAX,
                    timestamp,
                    kind: Kind::Response,
                    status: RowStatus::Success,
                    title: "Response".into(),
                    summary,
                    brief: None,
                    phase: 2,
                })
            }));
        }
        rows.sort_by_key(row_sort_key);
        rows.drain(..rows.len().saturating_sub(limit));
        for row in &mut rows {
            if row.status != RowStatus::Error
                && let Some(brief) = row.brief.take()
            {
                row.summary = brief;
            }
        }
        rows
    }
}

fn row_sort_key(row: &ActivityRow) -> (bool, u64, u64, String, u64, u8) {
    (
        row.timestamp.is_some(),
        row.timestamp.unwrap_or(0),
        agent_number(&row.agent),
        row.agent.clone(),
        row.offset,
        row.phase,
    )
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use pp_core::events::AgentEvent;
    use std::path::PathBuf;

    #[test]
    fn cwd_from_spec_flows_to_agent_view() {
        let mut plain = record("a1", "Main", "idle");
        plain.spec.cwd = PathBuf::from("/work/app");
        let mut isolated = record("a2", "Main", "idle");
        isolated.spec.isolated = true;
        isolated.spec.cwd = PathBuf::from("/work/app/.pp/worktrees/a2");
        let mut projection = Projection::default();
        projection.apply_snapshot(snapshot(vec![plain, isolated], vec![]), 1);
        let view = projection.agent("a1").expect("快照应包含 a1");
        assert_eq!(
            view.cwd,
            PathBuf::from("/work/app"),
            "普通 Agent 的 spec.cwd 应原样投影"
        );
        let view = projection.agent("a2").expect("快照应包含 a2");
        assert!(
            view.isolated,
            "fixture 标记 isolated 后投影的 isolated 应保持 true"
        );
        assert_eq!(
            view.cwd,
            PathBuf::from("/work/app/.pp/worktrees/a2"),
            "isolated Agent 首轮改写后的 spec.cwd 应原样投影"
        );
    }

    #[test]
    fn tree_keeps_lineage_and_protects_cycles() {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![
                    record("a1", "Main", "idle"),
                    record("a2", "a1", "idle"),
                    record("a3", "Main", "idle"),
                ],
                vec![],
            ),
            1,
        );
        let rows = projection.list(true, "a2");
        let ids = rows
            .iter()
            .map(|row| projection.agents()[row.agent].id.clone())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["a1", "a2"], "过滤保留 lineage");
        assert!(rows[0].muted && !rows[1].muted);
        // 孤立 parent 与循环都不能丢行或死循环。
        let mut orphan = Projection::default();
        orphan.apply_snapshot(
            snapshot(
                vec![
                    record("a1", "ghost", "idle"),
                    record("a2", "a3", "idle"),
                    record("a3", "a2", "idle"),
                ],
                vec![],
            ),
            1,
        );
        let rows = orphan.list(true, "");
        assert_eq!(rows.len(), 3);
        assert_eq!(orphan.list(true, "zzz").len(), 0, "无匹配显示空列表");
    }

    #[test]
    fn subtree_scope_and_error_category() {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![
                    record("a1", "Main", "idle"),
                    record("a2", "a1", "idle"),
                    record("a3", "Main", "idle"),
                ],
                vec![run("r1", "a1", "running", "parent task")],
            ),
            1,
        );
        let entries = vec![
            entry(0, Some(5), "r1", AgentEvent::AssistantStarted),
            entry(
                60,
                Some(6),
                "r1",
                AgentEvent::ToolStarted {
                    id: "c1".into(),
                    name: "bash".into(),
                    arguments: "{}".into(),
                },
            ),
            entry(
                120,
                Some(7),
                "r1",
                AgentEvent::ToolFinished {
                    id: "c1".into(),
                    result: serde_json::json!({"ok":false,"error":"boom"}),
                },
            ),
        ];
        projection.ingest("a1", &page(entries, false), 10);
        let all = projection.activity(Category::All, Scope::All, None, "");
        assert_eq!(all.len(), 3);
        let errors = projection.activity(Category::Errors, Scope::All, Some("a1"), "");
        assert_eq!(errors.len(), 1);
        assert_eq!(projection.rows()[errors[0]].status, RowStatus::Error);
        let subtree = projection.activity(Category::All, Scope::Subtree, Some("a1"), "");
        assert_eq!(subtree.len(), 3, "subtree 包含自身与后代，不含 sibling");
        let selected = projection.activity(Category::All, Scope::Selected, Some("a3"), "");
        assert!(selected.is_empty(), "scope 无有效 Agent 时明确空态");
        let tools = projection.activity(Category::Tools, Scope::All, None, "read");
        assert!(tools.is_empty());
        let searched = projection.activity(Category::All, Scope::All, None, "BASH");
        assert_eq!(searched.len(), 1, "搜索大小写不敏感");
    }

    #[test]
    fn frozen_rank_ignores_status_heartbeat() {
        let mut projection = Projection::default();
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "idle"), record("a2", "Main", "parked")],
                vec![],
            ),
            1,
        );
        assert_eq!(
            projection
                .agents()
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            ["a1", "a2"],
            "初次按 running/idle/parked/aborted 排序"
        );
        // 状态心跳不重排。
        projection.apply_snapshot(
            snapshot(
                vec![record("a1", "Main", "parked"), record("a2", "Main", "idle")],
                vec![run("r1", "a2", "running", "new work")],
            ),
            2,
        );
        assert_eq!(
            projection
                .agents()
                .iter()
                .map(|agent| agent.id.as_str())
                .collect::<Vec<_>>(),
            ["a1", "a2"]
        );
        // 新批次插到前面。
        projection.apply_snapshot(
            snapshot(
                vec![
                    record("a1", "Main", "parked"),
                    record("a2", "Main", "idle"),
                    record("a3", "Main", "idle"),
                ],
                vec![],
            ),
            3,
        );
        assert_eq!(projection.agents()[0].id, "a3");
    }
}
