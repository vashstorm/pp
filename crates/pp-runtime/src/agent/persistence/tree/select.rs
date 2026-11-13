use super::super::load::apply_native;
use super::*;
use pp_core::session_jsonl::Reader as SessionReader;
use std::io::BufReader;

impl Tree {
    pub(in crate::agent) fn prepare(&self, file: &File, id: &str) -> Result<Prepared> {
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
    let mut reader = SessionReader::new(BufReader::new(file));
    reader.rewind()?;
    let mut offset = 0;
    let mut checkpoint = None;
    let mut line = Vec::new();
    offset += reader.read_record(&mut line)? as u64;
    let header: Header = serde_json::from_slice(&line)
        .map_err(|_| anyhow::anyhow!("invalid session tree header"))?;
    while offset < cursor.offset {
        line.clear();
        let count = reader.read_record(&mut line)?;
        ensure!(
            count > 0 && line.ends_with(b"\n"),
            "incomplete session tree history"
        );
        offset += count as u64;
        let entry: Entry = serde_json::from_slice(&line)
            .map_err(|_| anyhow::anyhow!("invalid session tree history"))?;
        apply_native(&mut checkpoint, &header, entry.payload)?;
    }
    let checkpoint = checkpoint.context("missing session tree state")?;
    let mut session = checkpoint.session;
    let model_reference = checkpoint.model_reference;
    let todos = checkpoint.workflow.todos;
    let todo_plan_id = checkpoint.workflow.todo_plan_id;
    match &mut session {
        Session::Completion(request) => {
            ensure!(
                cursor.len <= request.messages.len(),
                "invalid session tree cursor"
            );
            request.messages.truncate(cursor.len);
            crate::agent::context::completion_boundaries(&request.messages)?;
        }
        Session::Response(request) => {
            let input = crate::agent::turn::response_input(request);
            ensure!(cursor.len <= input.len(), "invalid session tree cursor");
            input.truncate(cursor.len);
            crate::agent::context::response_boundaries(input)?;
        }
    }
    Ok((session, model_reference, todos, todo_plan_id))
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use pp_config::models::Protocol;
    use serde_json::json;

    #[test]
    fn tree_root_navigation_is_durable_without_a_followup_request() {
        for protocol in [Protocol::Completion, Protocol::Response] {
            let dir = tempfile::tempdir().unwrap();
            let metadata = Metadata {
                role_route: None,
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
                load_test(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
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
                indexed_listing(dir.path()).unwrap().sessions[0].summary,
                "first-question"
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
            let listing = indexed_listing(dir.path()).unwrap();
            assert_eq!(listing.sessions.len(), 1);
            assert_eq!(listing.sessions[0].id, id);
            assert_eq!(listing.sessions[0].summary, "first-question");
            assert_eq!(listing.sessions[0].preview, "first-answer");
        }
    }
    #[test]
    fn tree_old_nodes_survive_compaction_and_failed_navigation_keeps_disk_state() {
        for protocol in [Protocol::Completion, Protocol::Response] {
            let dir = tempfile::tempdir().unwrap();
            let metadata = Metadata {
                role_route: None,
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
                load_test(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
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

    fn push_todo_user(session: &mut Session, user: &str) {
        match session {
            Session::Completion(request) => request
                .messages
                .push(serde_json::from_value(json!({"role":"user","content":user})).unwrap()),
            Session::Response(request) => crate::agent::turn::response_input(request)
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
            Session::Response(request) => crate::agent::turn::response_input(request)
                .push(serde_json::from_value(json!({"type":"message","id":"message-todo","role":"assistant","status":"completed","content":[{"type":"output_text","text":answer,"annotations":[]}]})).unwrap()),
        }
    }

    #[test]
    fn todo_prepared_restores_ordinary_and_plan_source_from_history() {
        use pp_core::todo::TodoItem;
        use pp_core::todo::TodoStatus;
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
                plan: Some(pp_core::plan::PlanDocument {
                    id: "0123456789abcdef0123456789abcdef".into(),
                    revision: 1,
                    title: "Plan".into(),
                    content: "- [ ] step one\n".into(),
                    status: pp_core::plan::PlanStatus::Executed,
                    origin_prompt: "original task".into(),
                    file_name: None,
                    location: pp_core::plan::PlanLocation::Plans,
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
