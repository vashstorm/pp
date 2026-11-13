use super::*;
use pp_core::session_jsonl::Reader as SessionReader;
use std::io::BufReader;

fn replay_range(
    reader: &mut SessionReader<BufReader<&File>>,
    line: &mut Vec<u8>,
    (start, end): (u64, u64),
    records: &mut Vec<ReplayRecord>,
) -> Result<()> {
    if reader.position() > start {
        reader.rewind()?;
    }
    let mut offset = reader.position();
    let mut projector = replay::Projector::default();
    while offset < end {
        line.clear();
        let count = reader.read_record(line)?;
        ensure!(
            count > 0 && line.ends_with(b"\n"),
            "incomplete session tree replay"
        );
        let previous = offset;
        offset += count as u64;
        if previous < start {
            continue;
        }
        let value: serde_json::Value = serde_json::from_slice(line)
            .map_err(|_| anyhow::anyhow!("invalid session tree replay"))?;
        if value["type"] == "session" {
            continue;
        }
        let entry: Entry = serde_json::from_value(value)
            .map_err(|_| anyhow::anyhow!("invalid session tree replay"))?;
        projector.feed(&entry.payload, line)?;
    }
    records.extend(projector.finish()?);
    Ok(())
}

impl Tree {
    pub(in crate::agent) fn view(self, file: &File) -> Result<View> {
        let mut reader = SessionReader::new(BufReader::new(file));
        reader.rewind()?;
        let mut line = Vec::new();
        let mut records = Vec::with_capacity(self.nodes.len());
        for node in &self.nodes {
            let mut own = Vec::new();
            replay_range(&mut reader, &mut line, node.replay, &mut own)?;
            records.push(own);
        }
        Ok(View {
            tree: self,
            records,
        })
    }

    pub(in crate::agent) fn replay(
        &self,
        file: &File,
        leaf: Option<usize>,
        pending: bool,
    ) -> Result<Vec<ReplayRecord>> {
        let mut reader = SessionReader::new(BufReader::new(file));
        reader.rewind()?;
        let mut records = Vec::new();
        let mut ranges: Vec<_> = self
            .path(leaf)
            .into_iter()
            .map(|i| self.nodes[i].replay)
            .collect();
        if pending {
            ranges.push(self.pending);
        }
        let mut line = Vec::new();
        for range in ranges {
            replay_range(&mut reader, &mut line, range, &mut records)?;
        }
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use pp_config::models::Protocol;

    #[test]
    fn tree_view_keeps_duplicate_text_attempts_and_tool_events_separate() {
        for protocol in [Protocol::Completion, Protocol::Response] {
            let dir = tempfile::tempdir().unwrap();
            let metadata = metadata(dir.path());
            let mut journal = SessionJournal::new(dir.path().into());
            let mut session =
                Session::new("model".into(), protocol, "system".into(), EditMode::Replace);
            turn(
                &mut journal,
                &mut session,
                &metadata,
                "same question",
                "same answer",
                true,
            );
            let tree = journal.tree().unwrap();
            assert_eq!(tree.nodes[1].kind, Kind::Tools);
            let first_user = tree.nodes[0].id.clone();
            let first_tools = tree.nodes[1].id.clone();
            let Prepared {
                session: candidate,
                selection,
                ..
            } = journal.prepare_tree(&first_user).unwrap();
            assert_eq!(selection.leaf, None);
            journal
                .write_tree_state(
                    &metadata,
                    (&candidate).into(),
                    &WorkflowState::default(),
                    Some(&selection),
                )
                .unwrap();
            session = candidate;
            // Retrying the identical turn keeps the tools on the new effective
            // attempt with fresh IDs; the discarded attempt is gone for good.
            turn(
                &mut journal,
                &mut session,
                &metadata,
                "same question",
                "same answer",
                true,
            );
            journal.event(&AgentEvent::AssistantStarted);
            journal.event(&AgentEvent::AssistantFinished {
                text: "pending answer".into(),
            });
            let path = dir.path().join(format!("{}.jsonl", journal.id().unwrap()));
            let before = std::fs::read(&path).unwrap();
            let view = journal.tree_view().unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(view.tree.nodes.len(), 2);
            assert_eq!(view.tree.nodes[0].text, "same question");
            assert_eq!(view.tree.nodes[1].kind, Kind::Tools);
            assert_ne!(view.tree.nodes[0].id, first_user);
            assert_ne!(view.tree.nodes[1].id, first_tools);
            assert_eq!(view.tree.nodes[1].parent, Some(0));
            assert_eq!(view.records.len(), 2);
            assert_unknown_target(&mut journal, &first_user);
            assert_unknown_target(&mut journal, &first_tools);
            let Prepared { draft, .. } = journal.prepare_tree(&view.tree.nodes[0].id).unwrap();
            assert_eq!(draft.as_deref(), Some("same question"));
            // Retained tool results replay completely, by call ID and in order.
            let tools: Vec<_> = view.records[1]
                .iter()
                .filter_map(|r| match r {
                    ReplayRecord::Event(AgentEvent::ToolFinished { id, result }) => {
                        Some((id.as_str(), result["content"].as_str().unwrap()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(
                tools,
                [
                    ("call-a", "result for call-a"),
                    ("call-b", "result for call-b")
                ]
            );
            assert_eq!(users(&view.records[0]), ["same question"]);
            assert_eq!(answers(&view.records[1]), ["same answer"]);
            // Pending display events only enter replay, never completed Views.
            for records in &view.records {
                assert!(!records.iter().any(|record| matches!(
                    record,
                    ReplayRecord::Event(AgentEvent::AssistantFinished { text })
                        if text == "pending answer"
                )));
            }
            let replay_pending = view
                .tree
                .replay(&journal.active.as_ref().unwrap().file, view.tree.leaf, true)
                .unwrap();
            // 终态快照只在提交／失败／取消时落盘，未提交的流式尾部不进入磁盘回放。
            assert!(!replay_pending.iter().any(|record| matches!(
                record,
                ReplayRecord::Event(AgentEvent::AssistantFinished { text })
                    if text == "pending answer"
            )));
            let replay_committed = view
                .tree
                .replay(
                    &journal.active.as_ref().unwrap().file,
                    view.tree.leaf,
                    false,
                )
                .unwrap();
            assert!(!replay_committed.iter().any(|record| matches!(
                record,
                ReplayRecord::Event(AgentEvent::AssistantFinished { text })
                    if text == "pending answer"
            )));
            // Read failure must reject the whole View, not return the earlier valid nodes.
            let tree = journal.tree().unwrap();
            journal
                .active
                .as_mut()
                .unwrap()
                .file
                .set_len(before.len() as u64 / 2)
                .unwrap();
            assert!(tree.view(&journal.active.as_ref().unwrap().file).is_err());
        }
    }
}
