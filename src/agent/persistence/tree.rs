//! 基于已提交的原生记录重建逻辑会话树，节点只保存日志游标。
use super::*;
use crate::tools::todo::TodoPhase;
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
    /// 目标 cursor 之前最后一条 State/Workflow 记录折叠出的历史 todo 投影：
    /// 普通 `todos` 无来源，计划投影带 `todo_plan_id`；空 + None 为无清单。
    /// 只保留最新一份，不逐节点复制整个清单。
    pub todos: Vec<TodoPhase>,
    pub todo_plan_id: Option<String>,
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

pub(crate) struct View {
    pub tree: Tree,
    pub records: Vec<Vec<ReplayRecord>>,
}

fn replay_range(
    reader: &mut BufReader<&File>,
    line: &mut Vec<u8>,
    (start, end): (u64, u64),
    records: &mut Vec<ReplayRecord>,
) -> Result<()> {
    reader.seek(SeekFrom::Start(start))?;
    let mut offset = start;
    while offset < end {
        line.clear();
        let count = reader.read_until(b'\n', line)?;
        ensure!(count > 0, "incomplete session tree replay");
        offset += count as u64;
        let value: serde_json::Value = serde_json::from_slice(line)
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
    Ok(())
}

impl Tree {
    pub(super) fn view(self, file: &File) -> Result<View> {
        let mut reader = BufReader::new(file);
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
        let mut line = Vec::new();
        for range in ranges {
            replay_range(&mut reader, &mut line, range, &mut records)?;
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
        let (session, model_reference, todos, todo_plan_id) = session_at(file, cursor)?;
        let replay = self.replay(file, leaf, false)?;
        Ok(Prepared {
            session,
            model_reference,
            replay,
            selection: Selection {
                leaf: leaf.map(|i| self.nodes[i].id.clone()),
            },
            draft: user.then(|| node.text.clone()),
            todos,
            todo_plan_id,
        })
    }
}

/// 目标 cursor 之前的会话快照，以及折叠自最后一条 State/Workflow 记录的
/// 历史 todo 投影。每条 State 携带完整 workflow 快照，随后的 Workflow
/// 记录是运行态增量，因此只保留最后一份。
fn session_at(
    file: &File,
    cursor: Cursor,
) -> Result<(Session, String, Vec<TodoPhase>, Option<String>)> {
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(0))?;
    let mut offset = 0;
    let mut session = None;
    let mut model_reference = String::new();
    let mut todos = Vec::new();
    let mut todo_plan_id = None;
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
                todos = state.workflow.todos;
                todo_plan_id = state.workflow.todo_plan_id;
            }
            Payload::Custom {
                custom: Custom::Workflow(record),
            } => {
                todos = record.workflow.todos;
                todo_plan_id = record.workflow.todo_plan_id;
            }
            Payload::Custom {
                custom: Custom::ToolDefinitions { version, tools },
            } => {
                ensure!(version == 1, "unsupported saved tool definitions version");
                session
                    .as_mut()
                    .context("missing session tree state")?
                    .restore_tools(tools)?;
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
                        super::super::turn::response_input(request).extend(items)
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
            let input = super::super::turn::response_input(request);
            ensure!(cursor.len <= input.len(), "invalid session tree cursor");
            input.truncate(cursor.len);
            super::super::context::response_boundaries(input)?;
        }
    }
    Ok((session, model_reference, todos, todo_plan_id))
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
            Session::Response(request) => super::super::super::turn::response_input(request)
                .push(serde_json::from_value(json!({"role":"user","content":user})).unwrap()),
        }
        journal
            .sent(
                metadata,
                (&*session).into(),
                &WorkflowState::default(),
                Some(user),
                None,
            )
            .unwrap();
        journal.event(&AgentEvent::AssistantStarted);
        journal.event(&AgentEvent::AssistantFinished {
            text: answer.into(),
        });
        if tools {
            for id in ["call-a", "call-b"] {
                journal.event(&AgentEvent::ToolStarted {
                    id: id.into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                });
                journal.event(&AgentEvent::ToolFinished {
                    id: id.into(),
                    result: json!({"ok":true,"content":format!("result for {id}")}),
                });
            }
        }
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
                let input = super::super::super::turn::response_input(request);
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

    fn metadata(dir: &std::path::Path) -> Metadata<'_> {
        Metadata {
            cwd: dir,
            model_reference: "fixture/model",
            think_level: ThinkLevel::None,
            edit_mode: EditMode::Replace,
            tool_limits: ToolLimits::default(),
            base_system: "system",
            prompt_source: None,
        }
    }

    fn users(records: &[ReplayRecord]) -> Vec<&str> {
        records
            .iter()
            .filter_map(|record| match record {
                ReplayRecord::User(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn answers(records: &[ReplayRecord]) -> Vec<&str> {
        records
            .iter()
            .filter_map(|record| match record {
                ReplayRecord::Event(AgentEvent::AssistantFinished { text }) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn assert_unknown_target(journal: &mut SessionJournal, id: &str) {
        assert_eq!(
            journal.prepare_tree(id).err().unwrap().to_string(),
            "unknown session tree target"
        );
    }

    fn bytes_contain(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    /// Data payload of the last appended `pp.session_state` record.
    fn last_tree_state(bytes: &[u8]) -> serde_json::Value {
        let mut state = None;
        for line in bytes.split(|byte| *byte == b'\n') {
            if line.is_empty() {
                continue;
            }
            let entry: serde_json::Value = serde_json::from_slice(line).unwrap();
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

            // Every consumer now sees only the effective A path.
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
                load(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
            let checkpoint = text(&restored.checkpoint.session);
            assert!(!checkpoint.contains("question-b"));
            assert!(!checkpoint.contains("answer-b"));
            assert!(checkpoint.contains("answer-a"));
            assert_eq!(users(&restored.replay), ["question-a"]);
            for id in [&user_b, &answer_b] {
                assert_unknown_target(&mut restored.journal, id);
            }
            let listing = list(dir.path()).unwrap();
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
            assert!(replay_pending.iter().any(|record| matches!(
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
                prompt_source: None,
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
                .write_tree_state(
                    &metadata,
                    (&candidate).into(),
                    &WorkflowState::default(),
                    Some(&selection),
                )
                .unwrap();
            let id = journal.id().unwrap().to_owned();
            drop(journal);
            let mut restored =
                load(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
            assert!(restored.replay.is_empty());
            let view = restored.journal.tree_view().unwrap();
            assert!(view.tree.nodes.is_empty());
            assert!(view.records.is_empty());
            for node in &tree.nodes {
                assert_eq!(
                    restored
                        .journal
                        .prepare_tree(&node.id)
                        .err()
                        .unwrap()
                        .to_string(),
                    "unknown session tree target"
                );
            }
            assert!(!text(&restored.checkpoint.session).contains("first-answer"));
            assert_eq!(
                list(dir.path()).unwrap().sessions[0].summary,
                "Untitled session"
            );
            // New input continues from the empty effective history only; the
            // discarded turn never returns and the session keeps its identity.
            let mut journal = restored.journal;
            let mut session = restored.checkpoint.session;
            turn(
                &mut journal,
                &mut session,
                &metadata,
                "next-question",
                "next-answer",
                false,
            );
            for node in &tree.nodes {
                assert_unknown_target(&mut journal, &node.id);
            }
            let active = journal.tree().unwrap();
            assert_eq!(active.nodes.len(), 2);
            assert_eq!(active.nodes[0].parent, None);
            assert_eq!(active.nodes[0].text, "next-question");
            assert_eq!(active.path(active.leaf), vec![0, 1]);
            let listing = list(dir.path()).unwrap();
            assert_eq!(listing.sessions.len(), 1);
            assert_eq!(listing.sessions[0].id, id);
            assert_eq!(listing.sessions[0].summary, "next-question");
            assert_eq!(listing.sessions[0].preview, "next-answer");
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
                prompt_source: None,
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
                .replace_history(&metadata, (&compacted).into(), &WorkflowState::default())
                .unwrap();
            let Prepared {
                session: candidate,
                selection,
                ..
            } = journal.prepare_tree(&user).unwrap();
            assert!(text(&candidate).contains("answer-before-summary"));
            assert!(!text(&candidate).contains("compacted system"));
            // Compaction must not discard nodes that are still effective.
            let tree = journal.tree().unwrap();
            assert_eq!(tree.nodes.len(), 4);
            assert_eq!(tree.path(tree.leaf), vec![0, 1, 2, 3]);
            let id = journal.id().unwrap().to_owned();
            let path = dir.path().join(format!("{id}.jsonl"));
            let before = std::fs::read(&path).unwrap();
            journal.active.as_mut().unwrap().file = File::open(&path).unwrap();
            assert!(
                journal
                    .write_tree_state(
                        &metadata,
                        (&candidate).into(),
                        &WorkflowState::default(),
                        Some(&selection),
                    )
                    .is_err()
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert!(journal.check().is_err());
            drop(journal);
            let mut restored =
                load(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
            assert_eq!(text(&restored.checkpoint.session), text(&compacted));
            // The failed navigation must not invalidate the effective nodes.
            let tree = restored.journal.tree().unwrap();
            assert_eq!(tree.nodes.len(), 4);
            assert_eq!(tree.path(tree.leaf), vec![0, 1, 2, 3]);
            let view = restored.journal.tree_view().unwrap();
            assert_eq!(view.records.len(), 4);
            let Prepared {
                session: candidate, ..
            } = restored.journal.prepare_tree(&user).unwrap();
            assert!(text(&candidate).contains("answer-before-summary"));
            assert!(!text(&candidate).contains("compacted system"));
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
            let listing = list(dir.path()).unwrap();
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
            let restored = load(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
            assert_eq!(text(&restored.checkpoint.session), text(&cached.session));
            assert!(!text(&restored.checkpoint.session).contains("question-c"));
            assert_eq!(users(&restored.replay), ["question-a", "question-b"]);
        }
    }

    fn push_todo_user(session: &mut Session, user: &str) {
        match session {
            Session::Completion(request) => request
                .messages
                .push(serde_json::from_value(json!({"role":"user","content":user})).unwrap()),
            Session::Response(request) => super::super::super::turn::response_input(request)
                .push(serde_json::from_value(json!({"role":"user","content":user})).unwrap()),
        }
    }

    fn append_todo_assistant(session: &mut Session, answer: &str) {
        match session {
            Session::Completion(request) => request
                .messages
                .push(
                    serde_json::from_value(json!({"role":"assistant","content":answer})).unwrap(),
                ),
            Session::Response(request) => super::super::super::turn::response_input(request)
                .push(serde_json::from_value(json!({"type":"message","id":"message-todo","role":"assistant","status":"completed","content":[{"type":"output_text","text":answer,"annotations":[]}]})).unwrap()),
        }
    }

    #[test]
    fn todo_prepared_restores_ordinary_and_plan_source_from_history() {
        use crate::tools::todo::{TodoItem, TodoStatus};
        for protocol in [Protocol::Completion, Protocol::Response] {
            let dir = tempfile::tempdir().unwrap();
            let metadata = metadata(dir.path());
            let mut journal = SessionJournal::new(dir.path().into());
            let mut session =
                Session::new("model".into(), protocol, "system".into(), EditMode::Replace);

            // 第一轮请求携带普通清单（无计划来源）。
            let ordinary = WorkflowState {
                todos: vec![TodoPhase {
                    name: "Tasks".into(),
                    tasks: vec![TodoItem {
                        content: "A".into(),
                        status: TodoStatus::InProgress,
                        blocker: None,
                    }],
                }],
                ..Default::default()
            };
            push_todo_user(&mut session, "question-a");
            journal
                .sent(
                    &metadata,
                    (&session).into(),
                    &ordinary,
                    Some("question-a"),
                    None,
                )
                .unwrap();
            append_todo_assistant(&mut session, "answer-a");
            journal.commit((&session).into()).unwrap();

            let tree = journal.tree().unwrap();
            let user_a = tree.nodes[0].id.clone();
            let answer_a = tree.nodes[1].id.clone();

            // 用户 before cursor 与助手 native cursor 都折叠出 checkpoint 里的普通清单。
            let Prepared {
                todos,
                todo_plan_id,
                ..
            } = journal.prepare_tree(&user_a).unwrap();
            assert_eq!(todos, ordinary.todos);
            assert_eq!(todo_plan_id, None);
            let Prepared {
                todos,
                todo_plan_id,
                ..
            } = journal.prepare_tree(&answer_a).unwrap();
            assert_eq!(todos, ordinary.todos);
            assert_eq!(todo_plan_id, None);

            // 第二轮：执行中经 pp.workflow 记录把清单切换到计划投影。
            let planned = WorkflowState {
                mode: WorkMode::Execute,
                review: None,
                plan: Some(crate::plan::PlanDocument {
                    id: "0123456789abcdef0123456789abcdef".into(),
                    revision: 1,
                    title: "Plan".into(),
                    content: "- [ ] step one\n".into(),
                    status: crate::plan::PlanStatus::Executed,
                    origin_prompt: "original task".into(),
                    file_name: None,
                    location: crate::plan::PlanLocation::Plans,
                    approved_tasks: None,
                }),
                todos: vec![TodoPhase {
                    name: "Write".into(),
                    tasks: vec![TodoItem {
                        content: "step one".into(),
                        status: TodoStatus::Pending,
                        blocker: None,
                    }],
                }],
                todo_plan_id: Some("0123456789abcdef0123456789abcdef".into()),
            };
            push_todo_user(&mut session, "question-b");
            journal
                .sent(
                    &metadata,
                    (&session).into(),
                    &WorkflowState::default(),
                    Some("question-b"),
                    None,
                )
                .unwrap();
            journal.write_workflow(&planned).unwrap();
            append_todo_assistant(&mut session, "answer-b");
            journal.commit((&session).into()).unwrap();

            let tree = journal.tree().unwrap();
            let user_b = tree.nodes[2].id.clone();
            let answer_b = tree.nodes[3].id.clone();

            // 导航到 B 的用户消息之前：最后一 State/Workflow 记录仍是普通清单。
            let Prepared {
                todos,
                todo_plan_id,
                ..
            } = journal.prepare_tree(&user_b).unwrap();
            assert_eq!(todos, ordinary.todos);
            assert_eq!(todo_plan_id, None);

            // 助手节点：同轮 pp.workflow 增量成为最新投影来源。
            let Prepared {
                todos,
                todo_plan_id,
                ..
            } = journal.prepare_tree(&answer_b).unwrap();
            assert_eq!(todos, planned.todos);
            assert_eq!(
                todo_plan_id.as_deref(),
                Some("0123456789abcdef0123456789abcdef")
            );
        }
    }
}
