//! 基于已提交的原生记录重建逻辑会话树，节点只保存日志游标。
use super::*;
use std::collections::HashMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::agent) struct Selection {
    #[serde(deserialize_with = "required_leaf")]
    pub leaf: Option<String>,
}

fn required_leaf<'de, D: serde::Deserializer<'de>>(
    de: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::deserialize(de)
}

pub(in crate::agent) struct Prepared {
    pub session: Session,
    pub model_reference: String,
    pub replay: Vec<ReplayRecord>,
    pub selection: Selection,
    pub draft: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    User,
    Assistant,
    Tools,
}

#[derive(Clone, Copy, Debug)]
struct Cursor {
    offset: u64,
    len: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct Node {
    pub id: String,
    pub parent: Option<usize>,
    pub kind: Kind,
    pub text: String,
    pub done: bool,
    native: Cursor,
    before: Cursor,
    replay: (u64, u64),
}

#[derive(Default, Debug)]
pub(crate) struct Tree {
    pub nodes: Vec<Node>,
    pub leaf: Option<usize>,
    pending: (u64, u64),
}

impl Tree {
    #[cfg(test)]
    pub(crate) fn fixture(nodes: &[(Option<usize>, Kind, &str)], leaf: Option<usize>) -> Self {
        Self {
            nodes: nodes
                .iter()
                .enumerate()
                .map(|(i, (parent, kind, text))| Node {
                    id: format!("node-{i}"),
                    parent: *parent,
                    kind: *kind,
                    text: (*text).into(),
                    done: *kind == Kind::Assistant,
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

    pub(super) fn read(file: &File) -> Result<Self> {
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(0))?;
        let mut tree = Self::default();
        let mut ids = HashMap::new();
        let mut line = Vec::new();
        let mut offset = 0;
        let mut start = 0;
        let mut native = Cursor { offset: 0, len: 0 };
        let mut assistant: Option<String> = None;
        let mut tools = Vec::new();
        loop {
            line.clear();
            let count = reader.read_until(b'\n', &mut line)?;
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
                    let request = &data["session"]["request"];
                    native = Cursor {
                        offset,
                        len: request["messages"]
                            .as_array()
                            .or_else(|| request["input"].as_array())
                            .context("invalid session tree history")?
                            .len(),
                    };
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
                "pp.native_append" => {
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
                        node = Some((kind, text, kind == Kind::Assistant));
                    }
                    tools.clear();
                }
                "pp.cancelled" => node = Some((Kind::Assistant, "(aborted)".into(), false)),
                "pp.error" => node = Some((Kind::Assistant, "(error)".into(), false)),
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
                            false,
                        ))
                    }
                    Some("assistant") => {
                        let content = message["content"]
                            .as_array()
                            .context("invalid session tree assistant")?;
                        assistant = Some(
                            content
                                .iter()
                                .filter(|v| v["type"] == "text")
                                .filter_map(|v| v["text"].as_str())
                                .collect::<Vec<_>>()
                                .join(" "),
                        );
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
            if let Some((kind, text, done)) = node {
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
                    done,
                    native,
                    before,
                    replay: (start, offset),
                });
                tree.leaf = Some(tree.nodes.len() - 1);
                start = offset;
            }
        }
        tree.pending = (start, offset);
        Ok(tree)
    }

    pub(super) fn replay(
        &self,
        file: &File,
        leaf: Option<usize>,
        pending: bool,
    ) -> Result<Vec<ReplayRecord>> {
        let mut reader = BufReader::new(file);
        let mut records = Vec::new();
        let mut ranges: Vec<_> = self
            .path(leaf)
            .into_iter()
            .map(|i| self.nodes[i].replay)
            .collect();
        if pending {
            ranges.push(self.pending);
        }
        for (start, end) in ranges {
            reader.seek(SeekFrom::Start(start))?;
            let mut offset = start;
            let mut line = Vec::new();
            while offset < end {
                line.clear();
                let count = reader.read_until(b'\n', &mut line)?;
                ensure!(count > 0, "incomplete session tree replay");
                offset += count as u64;
                let value: serde_json::Value = serde_json::from_slice(&line)
                    .map_err(|_| anyhow::anyhow!("invalid session tree replay"))?;
                if value["type"] == "session" {
                    continue;
                }
                let entry: Entry = serde_json::from_value(value)
                    .map_err(|_| anyhow::anyhow!("invalid session tree replay"))?;
                match entry.payload {
                    Payload::Message {
                        message: SavedMessage::User { content, .. },
                    } => records.push(ReplayRecord::User(content)),
                    Payload::Custom {
                        custom: Custom::Event { event },
                    } => records.push(ReplayRecord::Event(event)),
                    Payload::Custom {
                        custom: Custom::Error { message },
                    } => records.push(ReplayRecord::Error(message)),
                    Payload::Custom {
                        custom: Custom::Cancelled {},
                    } => records.push(ReplayRecord::Cancelled),
                    _ => {}
                }
            }
        }
        Ok(records)
    }

    pub(super) fn prepare(&self, file: &File, id: &str) -> Result<Prepared> {
        let index = self
            .nodes
            .iter()
            .position(|node| node.id == id)
            .context("unknown session tree target")?;
        let node = &self.nodes[index];
        let user = node.kind == Kind::User;
        let leaf = if user { node.parent } else { Some(index) };
        let cursor = if user { node.before } else { node.native };
        let (session, model_reference) = session_at(file, cursor)?;
        let replay = self.replay(file, leaf, false)?;
        Ok(Prepared {
            session,
            model_reference,
            replay,
            selection: Selection {
                leaf: leaf.map(|i| self.nodes[i].id.clone()),
            },
            draft: user.then(|| node.text.clone()),
        })
    }
}

fn session_at(file: &File, cursor: Cursor) -> Result<(Session, String)> {
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(0))?;
    let mut offset = 0;
    let mut session = None;
    let mut model_reference = String::new();
    let mut line = Vec::new();
    while offset < cursor.offset {
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        ensure!(count > 0, "incomplete session tree history");
        offset += count as u64;
        if session.is_none() && offset == count as u64 {
            continue;
        }
        let entry: Entry = serde_json::from_slice(&line)
            .map_err(|_| anyhow::anyhow!("invalid session tree history"))?;
        match entry.payload {
            Payload::Custom {
                custom: Custom::State(state),
            } => {
                model_reference = state.model_reference;
                session = Some(state.session);
            }
            Payload::Custom {
                custom: Custom::NativeAppend(append),
            } => {
                match (
                    session.as_mut().context("missing session tree state")?,
                    append,
                ) {
                    (Session::Completion(request), NativeAppend::Completion { messages }) => {
                        request.messages.extend(messages)
                    }
                    (Session::Response(request), NativeAppend::Response { items }) => {
                        super::super::response_input(request).extend(items)
                    }
                    _ => bail!("session tree protocol mismatch"),
                }
            }
            _ => {}
        }
    }
    let mut session = session.context("missing session tree state")?;
    match &mut session {
        Session::Completion(request) => {
            ensure!(
                cursor.len <= request.messages.len(),
                "invalid session tree cursor"
            );
            request.messages.truncate(cursor.len);
            super::super::context::completion_boundaries(&request.messages)?;
        }
        Session::Response(request) => {
            let input = super::super::response_input(request);
            ensure!(cursor.len <= input.len(), "invalid session tree cursor");
            input.truncate(cursor.len);
            super::super::context::response_boundaries(input)?;
        }
    }
    Ok((session, model_reference))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Protocol;
    use serde_json::json;

    fn turn(
        journal: &mut SessionJournal,
        session: &mut Session,
        metadata: &Metadata<'_>,
        user: &str,
        answer: &str,
        tools: bool,
    ) {
        match session {
            Session::Completion(request) => request
                .messages
                .push(serde_json::from_value(json!({"role":"user","content":user})).unwrap()),
            Session::Response(request) => super::super::super::response_input(request)
                .push(serde_json::from_value(json!({"role":"user","content":user})).unwrap()),
        }
        journal
            .sent(metadata, (&*session).into(), Some(user), None)
            .unwrap();
        journal.event(&AgentEvent::AssistantStarted);
        journal.event(&AgentEvent::AssistantFinished {
            text: answer.into(),
        });
        match session {
            Session::Completion(request) => {
                let value = if tools {
                    json!({"role":"assistant","content":answer,"tool_calls":[{"id":"call-a","type":"function","function":{"name":"read","arguments":"{}"}},{"id":"call-b","type":"function","function":{"name":"read","arguments":"{}"}}]})
                } else {
                    json!({"role":"assistant","content":answer})
                };
                request
                    .messages
                    .push(serde_json::from_value(value).unwrap());
                if tools {
                    for id in ["call-a", "call-b"] {
                        request.messages.push(
                            serde_json::from_value(
                                json!({"role":"tool","tool_call_id":id,"content":"result"}),
                            )
                            .unwrap(),
                        );
                    }
                }
            }
            Session::Response(request) => {
                let input = super::super::super::response_input(request);
                input.push(serde_json::from_value(json!({"type":"message","id":"message-a","role":"assistant","status":"completed","content":[{"type":"output_text","text":answer,"annotations":[]}]})).unwrap());
                if tools {
                    for id in ["call-a", "call-b"] {
                        input.push(serde_json::from_value(json!({"type":"function_call","call_id":id,"name":"read","arguments":"{}"})).unwrap());
                    }
                    for id in ["call-a", "call-b"] {
                        input.push(serde_json::from_value(json!({"type":"function_call_output","call_id":id,"output":"result"})).unwrap());
                    }
                }
            }
        }
        journal.commit((&*session).into()).unwrap();
    }

    fn text(session: &Session) -> String {
        serde_json::to_string(session).unwrap()
    }

    #[test]
    fn tree_branches_keep_native_batches_replay_and_resume_both_protocols() {
        for protocol in [Protocol::Completion, Protocol::Response] {
            let dir = tempfile::tempdir().unwrap();
            let metadata = Metadata {
                cwd: dir.path(),
                model_reference: "fixture/model",
                think_level: ThinkLevel::None,
                edit_mode: EditMode::Replace,
                tool_limits: ToolLimits::default(),
                base_system: "system",
            };
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
                true,
            );
            let tree = journal.tree().unwrap();
            assert_eq!(tree.nodes.len(), 4);
            assert_eq!(tree.nodes[3].kind, Kind::Tools);
            let batch = tree.nodes[3].id.clone();
            let user_b = tree.nodes[2].id.clone();
            let original_len = SessionRef::from(&session).len();
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
            assert_eq!(
                replay
                    .iter()
                    .filter(|r| matches!(r, ReplayRecord::User(_)))
                    .count(),
                1
            );
            journal
                .write_tree_state(&metadata, (&candidate).into(), Some(&selection))
                .unwrap();
            session = candidate;
            turn(
                &mut journal,
                &mut session,
                &metadata,
                "question-new",
                "answer-new",
                false,
            );
            let tree = journal.tree().unwrap();
            assert_eq!(tree.nodes.len(), 6);
            assert_eq!(tree.nodes[4].parent, Some(1));
            assert_eq!(tree.path(tree.leaf), vec![0, 1, 4, 5]);
            let Prepared {
                session: candidate,
                replay,
                selection,
                ..
            } = journal.prepare_tree(&batch).unwrap();
            assert_eq!(SessionRef::from(&candidate).len(), original_len);
            assert!(text(&candidate).contains("call-a"));
            assert!(text(&candidate).contains("call-b"));
            assert!(!text(&candidate).contains("question-new"));
            assert_eq!(
                replay
                    .iter()
                    .filter(|r| matches!(r, ReplayRecord::User(_)))
                    .count(),
                2
            );
            journal
                .write_tree_state(&metadata, (&candidate).into(), Some(&selection))
                .unwrap();
            let id = journal.id().unwrap().to_owned();
            drop(journal);
            let restored = load(dir.path(), Some(&id), true).unwrap();
            assert_eq!(text(&restored.checkpoint.session), text(&candidate));
            assert_eq!(
                restored
                    .replay
                    .iter()
                    .filter(|r| matches!(r, ReplayRecord::User(_)))
                    .count(),
                2
            );
            let listing = list(dir.path()).unwrap();
            assert_eq!(listing.sessions[0].summary, "question-a");
            assert_eq!(listing.sessions[0].preview, "answer-a");
            assert!(!listing.sessions[0].done);
            let tree = Tree::read(&restored.journal.active.as_ref().unwrap().file).unwrap();
            assert_eq!(tree.nodes.len(), 6);
            assert_eq!(tree.path(tree.leaf), vec![0, 1, 2, 3]);
        }
    }

    #[test]
    fn tree_root_navigation_is_durable_without_a_followup_request() {
        for protocol in [Protocol::Completion, Protocol::Response] {
            let dir = tempfile::tempdir().unwrap();
            let metadata = Metadata {
                cwd: dir.path(),
                model_reference: "fixture/model",
                think_level: ThinkLevel::None,
                edit_mode: EditMode::Replace,
                tool_limits: ToolLimits::default(),
                base_system: "system",
            };
            let mut journal = SessionJournal::new(dir.path().into());
            let mut session =
                Session::new("model".into(), protocol, "system".into(), EditMode::Replace);
            turn(
                &mut journal,
                &mut session,
                &metadata,
                "first-question",
                "first-answer",
                false,
            );
            let tree = journal.tree().unwrap();
            let Prepared {
                session: candidate,
                replay,
                selection,
                draft,
                ..
            } = journal.prepare_tree(&tree.nodes[0].id).unwrap();
            assert_eq!(draft.as_deref(), Some("first-question"));
            assert!(replay.is_empty());
            assert!(!text(&candidate).contains("first-question"));
            assert_eq!(selection.leaf, None);
            journal
                .write_tree_state(&metadata, (&candidate).into(), Some(&selection))
                .unwrap();
            let id = journal.id().unwrap().to_owned();
            drop(journal);
            let restored = load(dir.path(), Some(&id), true).unwrap();
            assert!(restored.replay.is_empty());
            assert!(!text(&restored.checkpoint.session).contains("first-answer"));
            assert_eq!(
                list(dir.path()).unwrap().sessions[0].summary,
                "Untitled session"
            );
        }
    }
    #[test]
    fn tree_old_nodes_survive_compaction_and_failed_navigation_keeps_disk_state() {
        for protocol in [Protocol::Completion, Protocol::Response] {
            let dir = tempfile::tempdir().unwrap();
            let metadata = Metadata {
                cwd: dir.path(),
                model_reference: "fixture/model",
                think_level: ThinkLevel::None,
                edit_mode: EditMode::Replace,
                tool_limits: ToolLimits::default(),
                base_system: "system",
            };
            let mut journal = SessionJournal::new(dir.path().into());
            let mut session =
                Session::new("model".into(), protocol, "system".into(), EditMode::Replace);
            turn(
                &mut journal,
                &mut session,
                &metadata,
                "question-before-summary",
                "answer-before-summary",
                false,
            );
            turn(
                &mut journal,
                &mut session,
                &metadata,
                "question-second",
                "answer-second",
                false,
            );
            let tree = journal.tree().unwrap();
            let user = tree.nodes[2].id.clone();
            let compacted = Session::new(
                "model".into(),
                protocol,
                "compacted system".into(),
                EditMode::Replace,
            );
            journal
                .replace_history(&metadata, (&compacted).into())
                .unwrap();
            let Prepared {
                session: candidate,
                selection,
                ..
            } = journal.prepare_tree(&user).unwrap();
            assert!(text(&candidate).contains("answer-before-summary"));
            assert!(!text(&candidate).contains("compacted system"));
            let id = journal.id().unwrap().to_owned();
            let path = dir.path().join(format!("{id}.jsonl"));
            let before = std::fs::read(&path).unwrap();
            journal.active.as_mut().unwrap().file = File::open(&path).unwrap();
            assert!(
                journal
                    .write_tree_state(&metadata, (&candidate).into(), Some(&selection))
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(journal.check().is_err());
            drop(journal);
            let restored = load(dir.path(), Some(&id), true).unwrap();
            assert_eq!(text(&restored.checkpoint.session), text(&compacted));
        }
    }
}
