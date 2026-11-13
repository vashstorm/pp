use super::super::load::apply_native;
use super::*;
use pp_core::session_jsonl::Reader as SessionReader;
use std::collections::HashMap;
use std::io::BufReader;

impl Tree {
    #[cfg(any(test, feature = "test-support"))]
    pub fn fixture(nodes: &[(Option<usize>, Kind, &str)], leaf: Option<usize>) -> Self {
        Self {
            nodes: nodes
                .iter()
                .enumerate()
                .map(|(i, (parent, kind, text))| Node {
                    id: format!("node-{i}"),
                    parent: *parent,
                    kind: *kind,
                    text: (*text).into(),
                    native: Cursor { offset: 0, len: 0 },
                    before: Cursor { offset: 0, len: 0 },
                    replay: (0, 0),
                })
                .collect(),
            leaf,
            pending: (0, 0),
        }
    }

    pub fn path(&self, mut leaf: Option<usize>) -> Vec<usize> {
        let mut path = Vec::new();
        while let Some(index) = leaf {
            path.push(index);
            leaf = self.nodes[index].parent;
        }
        path.reverse();
        path
    }

    pub(in crate::agent) fn read(file: &File) -> Result<Self> {
        let mut reader = SessionReader::new(BufReader::new(file));
        reader.rewind()?;
        let mut tree = Self::default();
        let mut checkpoint = None;
        let mut header = None;
        let mut ids = HashMap::new();
        let mut line = Vec::new();
        let mut offset = 0;
        let mut start = 0;
        let mut native = Cursor { offset: 0, len: 0 };
        let mut assistant: Option<String> = None;
        let mut protocol: Option<String> = None;
        let mut tools = Vec::new();
        loop {
            line.clear();
            let count = reader.read_record(&mut line)?;
            if count == 0 || !line.ends_with(b"\n") {
                break;
            }
            offset += count as u64;
            let entry: serde_json::Value = serde_json::from_slice(&line)
                .map_err(|_| anyhow::anyhow!("invalid session tree record"))?;
            let custom = entry["customType"].as_str().unwrap_or_default();
            let data = &entry["data"];
            let mut node = None;
            match custom {
                "pp.session_state" => {
                    if let Some(session) = data.get("session").filter(|v| !v.is_null()) {
                        protocol = Some(
                            session["protocol"]
                                .as_str()
                                .context("invalid session tree protocol")?
                                .into(),
                        );
                        let request = &session["request"];
                        native.len = request["messages"]
                            .as_array()
                            .or_else(|| request["input"].as_array())
                            .context("invalid session tree history")?
                            .len();
                    } else {
                        ensure!(protocol.is_some(), "missing pp session baseline");
                    }
                    native.offset = offset;
                    if let Some(selection) = data.get("tree").filter(|v| !v.is_null()) {
                        let selection: Selection = serde_json::from_value(selection.clone())
                            .map_err(|_| anyhow::anyhow!("invalid session tree selection"))?;
                        tree.leaf = selection
                            .leaf
                            .as_ref()
                            .map(|id| ids.get(id).copied().context("unknown session tree target"))
                            .transpose()?;
                        start = offset;
                        assistant = None;
                        tools.clear();
                    }
                }
                "pp.event" if data["event"].get("model_attempt_failed").is_some() => {
                    assistant = None;
                    tools.clear();
                }
                "pp.native_append" => {
                    ensure!(
                        data["protocol"].as_str() == protocol.as_deref(),
                        "session tree protocol mismatch"
                    );
                    native.len += data["messages"]
                        .as_array()
                        .or_else(|| data["items"].as_array())
                        .context("invalid session tree append")?
                        .len();
                    native.offset = offset;
                    if let Some(text) = assistant.take() {
                        let kind = if tools.is_empty() {
                            Kind::Assistant
                        } else {
                            Kind::Tools
                        };
                        let text = if kind == Kind::Tools {
                            format!(
                                "{}{}",
                                if text.is_empty() {
                                    String::new()
                                } else {
                                    format!("{text} · ")
                                },
                                tools.join(", ")
                            )
                        } else {
                            text
                        };
                        node = Some((kind, text));
                    }
                    tools.clear();
                }
                "pp.cancelled" | "pp.error" => {
                    node = Some((
                        Kind::Assistant,
                        if custom == "pp.cancelled" {
                            "(aborted)"
                        } else {
                            "(error)"
                        }
                        .into(),
                    ));
                    assistant = None;
                    tools.clear();
                }
                _ => {}
            }
            if entry["type"] == "message" {
                let message = &entry["message"];
                match message["role"].as_str() {
                    Some("user") => {
                        node = Some((
                            Kind::User,
                            message["content"]
                                .as_str()
                                .context("invalid session tree user message")?
                                .to_owned(),
                        ))
                    }
                    Some("assistant") => {
                        let content = message["content"]
                            .as_array()
                            .context("invalid session tree assistant")?;
                        let text = replay::completed_text(message).unwrap_or_else(|| {
                            content
                                .iter()
                                .filter(|v| v["type"] == "text")
                                .filter_map(|v| v["text"].as_str())
                                .collect::<Vec<_>>()
                                .join(" ")
                        });
                        match &mut assistant {
                            Some(previous) => previous.push_str(&text),
                            None => assistant = Some(text),
                        }
                        tools.extend(
                            content
                                .iter()
                                .filter(|v| v["type"] == "toolCall")
                                .filter_map(|v| v["name"].as_str())
                                .map(str::to_owned),
                        );
                    }
                    _ => {}
                }
            }
            if entry["type"] == "session" {
                header = Some(
                    serde_json::from_value::<Header>(entry.clone())
                        .map_err(|_| anyhow::anyhow!("invalid session tree header"))?,
                );
            } else if entry["type"] != "message"
                && !matches!(custom, "pp.event" | "pp.error" | "pp.cancelled")
            {
                let saved: Entry = serde_json::from_slice(&line)
                    .map_err(|_| anyhow::anyhow!("invalid session tree record"))?;
                apply_native(
                    &mut checkpoint,
                    header.as_ref().context("missing session tree header")?,
                    saved.payload,
                )?;
            }
            if let Some((kind, text)) = node {
                let id = entry["id"]
                    .as_str()
                    .context("missing session tree ID")?
                    .to_owned();
                ensure!(!ids.contains_key(&id), "duplicate session tree ID");
                let before = if kind == Kind::User {
                    Cursor {
                        len: native
                            .len
                            .checked_sub(1)
                            .context("missing native user message")?,
                        ..native
                    }
                } else {
                    native
                };
                ids.insert(id.clone(), tree.nodes.len());
                tree.nodes.push(Node {
                    id,
                    parent: tree.leaf,
                    kind,
                    text,
                    native,
                    before,
                    replay: (start, offset),
                });
                tree.leaf = Some(tree.nodes.len() - 1);
                start = offset;
            }
        }
        tree.pending = (start, offset);
        tree.retain_active_path();
        Ok(tree)
    }

    fn retain_active_path(&mut self) {
        let path = self.path(self.leaf);
        // Parents precede children in the log, so moving a retained node forward
        // cannot displace another node that has yet to be retained.
        for (index, &old) in path.iter().enumerate() {
            self.nodes.swap(index, old);
            self.nodes[index].parent = index.checked_sub(1);
        }
        self.nodes.truncate(path.len());
        self.leaf = self.nodes.len().checked_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use pp_config::models::Protocol;

    fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    /// Data payload of the last appended `pp.session_state` record.
    fn last_tree_state(bytes: &[u8]) -> serde_json::Value {
        let mut state = None;
        let mut reader = SessionReader::new(BufReader::new(bytes));
        let mut line = Vec::new();
        while reader.read_record(&mut line).unwrap() != 0 {
            let entry: serde_json::Value = serde_json::from_slice(&line).unwrap();
            if entry["customType"] == "pp.session_state" {
                state = Some(entry["data"].clone());
            }
        }
        state.expect("appended pp.session_state record")
    }

    fn native_messages(state: &serde_json::Value) -> String {
        let request = &state["session"]["request"];
        let messages = request["messages"]
            .as_array()
            .or_else(|| request["input"].as_array())
            .expect("native session history");
        serde_json::to_string(messages).unwrap()
    }

    #[test]
    fn tree_rewind_invalidates_discarded_nodes_durably_both_protocols() {
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
                "question-a",
                "answer-a",
                false,
            );
            turn(
                &mut journal,
                &mut session,
                &metadata,
                "question-b",
                "answer-b",
                false,
            );
            let tree = journal.tree().unwrap();
            assert_eq!(tree.path(tree.leaf), vec![0, 1, 2, 3]);
            let answer_a = tree.nodes[1].id.clone();
            let user_b = tree.nodes[2].id.clone();
            let answer_b = tree.nodes[3].id.clone();
            let path = dir.path().join(format!("{}.jsonl", journal.id().unwrap()));
            let before = std::fs::read(&path).unwrap();

            // Rewinding to before B's user message is a durable append-only
            // commit: the old records stay on disk as a byte prefix, and the
            // appended full state carries only the effective history.
            let Prepared {
                session: candidate,
                replay,
                selection,
                draft,
                ..
            } = journal.prepare_tree(&user_b).unwrap();
            assert_eq!(draft.as_deref(), Some("question-b"));
            assert!(!text(&candidate).contains("question-b"));
            assert!(text(&candidate).contains("answer-a"));
            assert_eq!(users(&replay), ["question-a"]);
            journal
                .write_tree_state(
                    &metadata,
                    (&candidate).into(),
                    &WorkflowState::default(),
                    Some(&selection),
                )
                .unwrap();
            let after = std::fs::read(&path).unwrap();
            assert!(after.starts_with(&before));
            assert!(bytes_contain(&after, b"question-b"));
            let state = last_tree_state(&after);
            assert_eq!(state["tree"]["leaf"], serde_json::Value::String(answer_a));
            let native = native_messages(&state);
            assert!(!native.contains("question-b"));
            assert!(!native.contains("answer-b"));

            // 树与恢复只保留有效的 A 路径；列表摘要固定使用最初消息。
            let tree = journal.tree().unwrap();
            assert_eq!(tree.nodes.len(), 2);
            assert_eq!(tree.nodes[1].parent, Some(0));
            let view = journal.tree_view().unwrap();
            assert_eq!(view.records.len(), 2);
            assert_eq!(users(&view.records[0]), ["question-a"]);
            assert_eq!(answers(&view.records[1]), ["answer-a"]);
            let before_reject = std::fs::read(&path).unwrap();
            for id in [&user_b, &answer_b] {
                assert_unknown_target(&mut journal, id);
            }
            assert_eq!(std::fs::read(&path).unwrap(), before_reject);

            // Resuming immediately after the commit must not resurrect B.
            let id = journal.id().unwrap().to_owned();
            drop(journal);
            let mut restored =
                load_test(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
            let checkpoint = text(&restored.checkpoint.session);
            assert!(!checkpoint.contains("question-b"));
            assert!(!checkpoint.contains("answer-b"));
            assert!(checkpoint.contains("answer-a"));
            assert_eq!(users(&restored.replay), ["question-a"]);
            for id in [&user_b, &answer_b] {
                assert_unknown_target(&mut restored.journal, id);
            }
            let listing = indexed_listing(dir.path()).unwrap();
            assert_eq!(listing.sessions[0].summary, "question-a");
            assert_eq!(listing.sessions[0].preview, "answer-a");

            // A later C turn is discarded the same way, while a same-text
            // retry of B gets fresh IDs on the effective path.
            let mut journal = restored.journal;
            let mut session = restored.checkpoint.session;
            turn(
                &mut journal,
                &mut session,
                &metadata,
                "question-c",
                "answer-c",
                false,
            );
            let tree = journal.tree().unwrap();
            let user_c = tree.nodes[2].id.clone();
            let answer_c = tree.nodes[3].id.clone();
            let Prepared {
                session: candidate,
                selection,
                ..
            } = journal.prepare_tree(&user_c).unwrap();
            journal
                .write_tree_state(
                    &metadata,
                    (&candidate).into(),
                    &WorkflowState::default(),
                    Some(&selection),
                )
                .unwrap();
            session = candidate;
            turn(
                &mut journal,
                &mut session,
                &metadata,
                "question-b",
                "answer-b",
                false,
            );
            let tree = journal.tree().unwrap();
            assert_eq!(tree.nodes.len(), 4);
            assert_eq!(tree.nodes[2].text, "question-b");
            assert_ne!(tree.nodes[2].id, user_b);
            assert_ne!(tree.nodes[3].id, answer_b);
            assert_eq!(tree.nodes[2].parent, Some(1));
            assert_eq!(tree.nodes[3].parent, Some(2));
            assert_eq!(tree.path(tree.leaf), vec![0, 1, 2, 3]);
            for id in [&user_b, &answer_b, &user_c, &answer_c] {
                assert_unknown_target(&mut journal, id);
            }
            let Prepared {
                session: candidate,
                replay,
                draft,
                ..
            } = journal.prepare_tree(&tree.nodes[2].id).unwrap();
            assert_eq!(draft.as_deref(), Some("question-b"));
            assert!(!text(&candidate).contains("question-b"));
            assert!(!text(&candidate).contains("answer-c"));
            assert_eq!(users(&replay), ["question-a"]);
            let Prepared {
                session: candidate, ..
            } = journal.prepare_tree(&tree.nodes[3].id).unwrap();
            let native = text(&candidate);
            assert!(native.contains("question-b"));
            assert!(native.contains("answer-b"));
            assert!(!native.contains("question-c"));
            assert!(!native.contains("answer-c"));
            let view = journal.tree_view().unwrap();
            assert_eq!(view.records.len(), 4);
            assert_eq!(users(&view.records[2]), ["question-b"]);
            assert_eq!(answers(&view.records[3]), ["answer-b"]);
            for records in &view.records {
                assert!(!users(records).contains(&"question-c"));
                assert!(!answers(records).contains(&"answer-c"));
            }
        }
    }

    #[test]
    fn tree_legacy_reselection_log_resolves_final_committed_path_both_protocols() {
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
                "question-a",
                "answer-a",
                false,
            );
            turn(
                &mut journal,
                &mut session,
                &metadata,
                "question-b",
                "answer-b",
                false,
            );
            let tree = journal.tree().unwrap();
            let a_answer = tree.nodes[1].id.clone();
            let b_answer = tree.nodes[3].id.clone();

            // Simulate an old client: it caches its prepared selection of the
            // B answer node before rewinding, rewinds to A, appends a C
            // branch, then writes the cached B selection back as a full state.
            let cached = journal.prepare_tree(&b_answer).unwrap();
            let Prepared {
                session: candidate,
                selection,
                ..
            } = journal.prepare_tree(&a_answer).unwrap();
            assert!(text(&candidate).contains("answer-a"));
            assert!(!text(&candidate).contains("question-b"));
            journal
                .write_tree_state(
                    &metadata,
                    (&candidate).into(),
                    &WorkflowState::default(),
                    Some(&selection),
                )
                .unwrap();
            session = candidate;
            turn(
                &mut journal,
                &mut session,
                &metadata,
                "question-c",
                "answer-c",
                false,
            );
            let tree = journal.tree().unwrap();
            assert_eq!(tree.nodes.len(), 4);
            let user_c = tree.nodes[2].id.clone();
            let answer_c = tree.nodes[3].id.clone();
            journal
                .write_tree_state(
                    &metadata,
                    (&cached.session).into(),
                    &WorkflowState::default(),
                    Some(&cached.selection),
                )
                .unwrap();

            // The new reader resolves the whole log to the final committed
            // path A→B and rejects C instead of failing midway with unknown
            // targets. Reading must not modify the file.
            let path = dir.path().join(format!("{}.jsonl", journal.id().unwrap()));
            let before_bytes = std::fs::read(&path).unwrap();
            let before_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
            let tree = journal.tree().unwrap();
            assert_eq!(tree.nodes.len(), 4);
            assert_eq!(tree.nodes[0].text, "question-a");
            assert_eq!(tree.nodes[1].text, "answer-a");
            assert_eq!(tree.nodes[2].text, "question-b");
            assert_eq!(tree.nodes[3].text, "answer-b");
            assert_eq!(tree.nodes[3].parent, Some(2));
            assert_eq!(tree.path(tree.leaf), vec![0, 1, 2, 3]);
            for id in [&user_c, &answer_c] {
                assert_unknown_target(&mut journal, id);
            }
            let view = journal.tree_view().unwrap();
            assert_eq!(view.records.len(), 4);
            for records in &view.records {
                assert!(!users(records).contains(&"question-c"));
                assert!(!answers(records).contains(&"answer-c"));
            }
            let listing = indexed_listing(dir.path()).unwrap();
            assert_eq!(listing.sessions[0].summary, "question-a");
            assert_eq!(listing.sessions[0].preview, "answer-a");
            assert!(listing.sessions[0].done);
            assert_eq!(std::fs::read(&path).unwrap(), before_bytes);
            assert_eq!(
                std::fs::metadata(&path).unwrap().modified().unwrap(),
                before_mtime
            );

            let id = journal.id().unwrap().to_owned();
            drop(journal);
            let restored =
                load_test(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
            assert_eq!(text(&restored.checkpoint.session), text(&cached.session));
            assert!(!text(&restored.checkpoint.session).contains("question-c"));
            assert_eq!(users(&restored.replay), ["question-a", "question-b"]);
        }
    }
}
