//! 新格式验收使用冻结旧事件与真实 journal，覆盖物理分段和展示／原生历史隔离。
use super::*;
use pp_config::models::Protocol;
use pp_core::events::{FileDiff, OutputStream};
use serde_json::{Value, json};

fn metadata(dir: &Path) -> Metadata<'_> {
    Metadata {
        role_route: None,
        cwd: dir,
        model_reference: "fixture/model",
        think_level: ThinkLevel::None,
        edit_mode: EditMode::Replace,
        tool_limits: ToolLimits::default(),
        base_system: "system",
        prompt_source: None,
    }
}

fn append_answer(session: &mut Session, text: &str) {
    match session {
        Session::Completion(request) => request.messages.push(serde_json::from_value(json!({"role":"assistant","content":text})).unwrap()),
        Session::Response(request) => super::super::turn::response_input(request).push(serde_json::from_value(json!({"type":"message","id":"m","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]})).unwrap()),
    }
}

fn start(journal: &mut SessionJournal, session: &mut Session, dir: &Path, user: &str) {
    super::super::append_user_to(session, user);
    journal
        .sent(
            &metadata(dir),
            (&*session).into(),
            &WorkflowState::default(),
            Some(user),
            None,
        )
        .unwrap();
    journal.event(&AgentEvent::AssistantStarted);
}

fn event_values(records: &[ReplayRecord]) -> Vec<Value> {
    records
        .iter()
        .filter_map(|r| match r {
            ReplayRecord::Event(event) => Some(serde_json::to_value(event).unwrap()),
            _ => None,
        })
        .collect()
}

fn records(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn frozen_legacy(dir: &Path, protocol: Protocol) -> String {
    let id = format!("2026-11-12T01-02-03-{}", "a".repeat(32));
    let mut session = Session::new("model".into(), protocol, "system".into(), EditMode::Replace);
    super::super::append_user_to(&mut session, "old question");
    let mut state = serde_json::to_value(StateRef::new(
        &metadata(dir),
        (&session).into(),
        &WorkflowState::default(),
    ))
    .unwrap();
    state.as_object_mut().unwrap().remove("replayVersion");
    let mut lines = vec![
        json!({"type":"session","version":3,"id":id,"timestamp":"2026-11-12T01:02:03Z","cwd":dir}),
    ];
    append_answer(&mut session, "old answer");
    let native = serde_json::to_value(&session).unwrap();
    let append = match protocol {
        Protocol::Completion => {
            json!({"protocol":"completion","messages":[native["request"]["messages"].as_array().unwrap().last().unwrap()]})
        }
        Protocol::Response => {
            json!({"protocol":"response","items":[native["request"]["input"].as_array().unwrap().last().unwrap()]})
        }
    };
    // 旧格式直接冻结，不使用会被新 writer 过滤的 journal.event。
    let payloads = [
        json!({"type":"custom","customType":"pp.session_state","data":state}),
        json!({"type":"message","message":{"role":"user","content":"old question","timestamp":1}}),
        json!({"type":"custom","customType":"pp.event","data":{"event":"assistant_started"}}),
        json!({"type":"custom","customType":"pp.event","data":{"event":{"thinking_delta":{"text":"old thinking"}}}}),
        json!({"type":"custom","customType":"pp.event","data":{"event":{"assistant_finished":{"text":"old answer"}}}}),
        json!({"type":"message","message":{"role":"assistant","content":[{"type":"thinking","thinking":"old thinking"},{"type":"text","text":"old answer"}],"stopReason":"stop"}}),
        json!({"type":"custom","customType":"pp.native_append","data":append}),
    ];
    for (i, mut value) in payloads.into_iter().enumerate() {
        value["id"] = format!("old-{i}").into();
        value["parentId"] = if i == 0 {
            Value::Null
        } else {
            format!("old-{}", i - 1).into()
        };
        value["timestamp"] = "2026-11-12T01:02:03Z".into();
        lines.push(value);
    }
    fs::write(
        dir.join(format!("{id}.jsonl")),
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    id
}

#[test]
fn slimming_mixed_legacy_two_new_turns_and_tree_ranges_both_protocols() {
    for protocol in [Protocol::Completion, Protocol::Response] {
        let dir = tempfile::tempdir().unwrap();
        let id = frozen_legacy(dir.path(), protocol);
        let path = dir.path().join(format!("{id}.jsonl"));
        let before = fs::read(&path).unwrap();
        let restored = load_test(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        let mut journal = restored.journal;
        let mut session = restored.checkpoint.session;
        for answer in ["new one", "new two"] {
            start(&mut journal, &mut session, dir.path(), answer);
            journal.event(&AgentEvent::ThinkingDelta {
                text: "new thought".into(),
            });
            journal.event(&AgentEvent::AssistantDelta {
                text: answer.into(),
                refusal: false,
            });
            journal.event(&AgentEvent::AssistantFinished {
                text: answer.into(),
            });
            append_answer(&mut session, answer);
            journal.commit((&session).into()).unwrap();
        }
        let states: Vec<_> = records(&path)
            .into_iter()
            .filter(|r| r["customType"] == "pp.session_state")
            .collect();
        assert_eq!(states.len(), 2);
        assert_eq!(states[1]["data"]["replayVersion"], 1);
        assert!(states[1]["data"].get("session").is_none());
        let view = journal.tree_view().unwrap();
        assert_eq!(view.tree.nodes.len(), 6);
        for (i, expected) in [(3, "new one"), (5, "new two")] {
            assert!(
                event_values(&view.records[i])
                    .iter()
                    .any(|e| e["assistant_finished"]["text"] == expected)
            );
            assert!(
                !event_values(&view.records[i])
                    .iter()
                    .any(|e| e["assistant_finished"]["text"] == "old answer")
            );
        }
        let selected = journal.prepare_tree(&view.tree.nodes[3].id).unwrap();
        journal
            .write_tree_state(
                &metadata(dir.path()),
                (&selected.session).into(),
                &WorkflowState::default(),
                Some(&selected.selection),
            )
            .unwrap();
        assert_eq!(journal.replay_version, 1);
        drop(journal);
        let restored = load_test(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
        // 树选择恢复旧节点与新节点各一次，裁剪掉第二个新分支。
        assert_eq!(
            event_values(&restored.replay)
                .iter()
                .filter(|e| e["assistant_finished"]["text"] == "old answer")
                .count(),
            1
        );
        assert!(
            event_values(&restored.replay)
                .iter()
                .any(|e| e["assistant_finished"]["text"] == "new one")
        );
        assert!(
            !event_values(&restored.replay)
                .iter()
                .any(|e| e["assistant_finished"]["text"] == "new two")
        );
    }
}

#[test]
fn slimming_partial_retry_tools_and_terminal_snapshots_do_not_change_native_history() {
    for protocol in [Protocol::Completion, Protocol::Response] {
        for terminal in ["cancelled", "error", "success"] {
            let dir = tempfile::tempdir().unwrap();
            let mut journal = SessionJournal::new(dir.path().into());
            let mut session =
                Session::new("model".into(), protocol, "system".into(), EditMode::Replace);
            start(&mut journal, &mut session, dir.path(), "question");
            journal.event(&AgentEvent::ThinkingDelta {
                text: "failed thought".into(),
            });
            journal.event(&AgentEvent::AssistantDelta {
                text: "failed prefix".into(),
                refusal: false,
            });
            journal.event(&AgentEvent::ModelAttemptFailed {
                attempt: 1,
                model: "model".into(),
                error: "retry".into(),
            });
            journal.event(&AgentEvent::AssistantStarted);
            journal.event(&AgentEvent::ReasoningSummaryDelta {
                text: "summary".into(),
            });
            journal.event(&AgentEvent::ThinkingDelta {
                text: "reasoning".into(),
            });
            journal.event(&AgentEvent::AssistantDelta {
                text: "answer".into(),
                refusal: false,
            });
            journal.event(&AgentEvent::AssistantFinished {
                text: "answer".into(),
            });
            let mut expected = vec![
                AgentEvent::ToolStarted {
                    id: "reuse".into(),
                    name: "write".into(),
                    arguments: "[]".into(),
                },
                AgentEvent::FileChanged {
                    id: "reuse".into(),
                    path: "a.txt".into(),
                    diff: FileDiff::Text {
                        created: true,
                        deleted: false,
                        added: 1,
                        removed: 0,
                        hunks: vec![],
                    },
                },
                AgentEvent::ToolFinished {
                    id: "reuse".into(),
                    result: json!({"ok":true,"artifact":"artifact://1","pipes":{"run_id":"r"}}),
                },
                AgentEvent::ToolStarted {
                    id: "other".into(),
                    name: "bash".into(),
                    arguments: "{".into(),
                },
                AgentEvent::ToolOutput {
                    id: "other".into(),
                    stream: OutputStream::Stdout,
                    text: "unfinished stdout".into(),
                },
            ];
            if terminal == "success" {
                expected.push(AgentEvent::ToolFinished {
                    id: "other".into(),
                    result: json!({"ok":false,"output":"final stdout","cancelled":true}),
                });
            }
            for e in &expected {
                journal.event(e);
            }
            let native_before = serde_json::to_value(&session).unwrap();
            match terminal {
                "cancelled" => journal.cancelled().unwrap(),
                "error" => journal.error("batch cleanup failed"),
                _ => {
                    append_answer(&mut session, "answer");
                    journal.commit((&session).into()).unwrap();
                    journal.error("post-commit cleanup failed");
                }
            }
            if terminal != "success" {
                assert_eq!(serde_json::to_value(&session).unwrap(), native_before);
            }
            let id = journal.id().unwrap().to_owned();
            let path = dir.path().join(format!("{id}.jsonl"));
            let saved = records(&path);
            assert_eq!(
                saved
                    .iter()
                    .filter(|r| r["message"]["role"] == "assistant")
                    .count(),
                2
            );
            assert!(
                saved
                    .iter()
                    .filter(|r| r["message"]["role"] == "toolResult")
                    .all(|r| r["message"].get("details").is_none())
            );
            let persisted_events: Vec<_> = saved
                .iter()
                .filter_map(|r| r["data"].get("event"))
                .collect();
            assert!(
                !persisted_events
                    .iter()
                    .any(|e| e.get("assistant_delta").is_some()
                        || e.get("tool_started").is_some()
                        || e.get("tool_finished").is_some())
            );
            drop(journal);
            let restored =
                load_test(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
            assert!(
                !serde_json::to_string(&restored.checkpoint.session)
                    .unwrap()
                    .contains("failed prefix")
            );
            let values = event_values(&restored.replay);
            assert_eq!(
                values
                    .iter()
                    .filter(|e| e["assistant_delta"]["text"] == "failed prefix")
                    .count(),
                1
            );
            assert_eq!(
                values
                    .iter()
                    .filter(|e| e["assistant_finished"]["text"] == "answer")
                    .count(),
                1
            );
            let actual: Vec<_> = values
                .into_iter()
                .filter(|e| {
                    e.get("tool_started").is_some()
                        || e.get("file_changed").is_some()
                        || e.get("tool_finished").is_some()
                        || e.get("tool_output").is_some()
                })
                .collect();
            if terminal == "success" {
                expected.retain(|e| !matches!(e, AgentEvent::ToolOutput { .. }));
            }
            assert_eq!(
                actual,
                expected
                    .iter()
                    .map(|e| serde_json::to_value(e).unwrap())
                    .collect::<Vec<_>>()
            );
            assert_eq!(restored.journal.committed, SessionRef::from(&session).len());
        }
    }
}

#[test]
fn slimming_thinking_only_empty_cancel_crash_and_snapshot_failure() {
    for thinking in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = SessionJournal::new(dir.path().into());
        let mut session = Session::new(
            "model".into(),
            Protocol::Response,
            "system".into(),
            EditMode::Replace,
        );
        start(&mut journal, &mut session, dir.path(), "question");
        journal.event(&AgentEvent::AssistantDelta {
            text: String::new(),
            refusal: false,
        });
        journal.event(&AgentEvent::ThinkingDelta {
            text: String::new(),
        });
        journal.event(&AgentEvent::ReasoningSummaryDelta {
            text: String::new(),
        });
        if thinking {
            journal.event(&AgentEvent::ThinkingDelta {
                text: "thought".into(),
            });
        }
        let id = journal.id().unwrap().to_owned();
        let path = dir.path().join(format!("{id}.jsonl"));
        assert!(!fs::read_to_string(&path).unwrap().contains("thought"));
        journal.cancelled().unwrap();
        assert_eq!(
            records(&path)
                .iter()
                .filter(|r| r["message"]["role"] == "assistant")
                .count(),
            usize::from(thinking)
        );

        drop(journal);
        let restored = load_test(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
        assert_eq!(
            event_values(&restored.replay)
                .iter()
                .filter(|e| e.get("thinking_delta").is_some())
                .count(),
            usize::from(thinking)
        );
        assert!(
            !event_values(&restored.replay)
                .iter()
                .any(|e| e.get("assistant_finished").is_some())
        );
        drop(restored);
        let mut restored =
            load_test(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
        restored.journal.event(&AgentEvent::AssistantStarted);
        restored.journal.event(&AgentEvent::AssistantDelta {
            text: "lost-on-write".into(),
            refusal: false,
        });
        let before = fs::read(&path).unwrap();
        restored.journal.use_read_only_file(&path);
        assert!(restored.journal.cancelled().is_err());
        let first = restored.journal.check().unwrap_err().to_string();
        restored.journal.error("later");
        assert_eq!(restored.journal.check().unwrap_err().to_string(), first);
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}

#[test]
fn slimming_optional_states_validate_every_baseline_and_keep_full_same_length_replacements() {
    for protocol in [Protocol::Completion, Protocol::Response] {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = SessionJournal::new(dir.path().into());
        let mut session =
            Session::new("model".into(), protocol, "system".into(), EditMode::Replace);
        session.set_think_level(ThinkLevel::None);
        start(&mut journal, &mut session, dir.path(), "question");
        append_answer(&mut session, "one");
        journal.event(&AgentEvent::AssistantFinished { text: "one".into() });
        journal.commit((&session).into()).unwrap();
        let meta = metadata(dir.path());
        let workflow = WorkflowState::default();
        let mut state = StateRef::new(&meta, (&session).into(), &workflow);
        state.session = None;
        journal.synced_custom(CustomRef::State(state)).unwrap();
        let view = journal.tree_view().unwrap();
        let prepared = journal.prepare_tree(&view.tree.nodes[1].id).unwrap();
        assert_eq!(
            serde_json::to_value(prepared.session).unwrap(),
            serde_json::to_value(&session).unwrap()
        );
        let id = journal.id().unwrap().to_owned();
        let path = dir.path().join(format!("{id}.jsonl"));
        drop(journal);
        let restored = load_test(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
        assert_eq!(
            serde_json::to_value(&restored.checkpoint.session).unwrap(),
            serde_json::to_value(&session).unwrap()
        );
        drop(restored);
        // 同长度但中间内容变化，replace_history 必须完整落盘。
        let mut restored =
            load_test(dir.path(), &dir.path().join(".locks"), Some(&id), false).unwrap();
        let mut changed = serde_json::to_value(&session).unwrap();
        changed["request"]["model"] = "other-model".into();
        match protocol {
            Protocol::Completion => {
                changed["request"]["messages"][0]["content"] = "changed middle".into()
            }
            Protocol::Response => changed["request"]["instructions"] = "changed middle".into(),
        }
        let changed: Session = serde_json::from_value(changed).unwrap();
        assert_eq!(
            SessionRef::from(&changed).len(),
            SessionRef::from(&session).len()
        );
        restored
            .journal
            .replace_history(
                &metadata(dir.path()),
                (&changed).into(),
                &WorkflowState::default(),
            )
            .unwrap();
        drop(restored);
        assert!(
            records(&path).last().unwrap()["data"]
                .get("session")
                .is_some()
        );
        let original = fs::read(&path).unwrap();
        super::tests::rewrite_records(&path, |r| {
            if r["customType"] == "pp.session_state" && r["data"].get("session").is_none() {
                r["data"]["modelReference"] = "broken".into();
            }
        });
        assert!(load_test(dir.path(), &dir.path().join(".locks"), Some(&id), false).is_err());
        assert!(tree::Tree::read(&File::open(&path).unwrap()).is_err());
        fs::write(&path, &original).unwrap();
        super::tests::rewrite_records(&path, |r| {
            if r["customType"] == "pp.session_state" {
                r["data"].as_object_mut().unwrap().remove("session");
            }
        });
        assert!(load_test(dir.path(), &dir.path().join(".locks"), Some(&id), true).is_err());
    }
}

#[test]
fn slimming_omitted_state_inherits_tools_instructions_thinking_and_append_for_all_readers() {
    for protocol in [Protocol::Completion, Protocol::Response] {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = SessionJournal::new(dir.path().into());
        let mut session =
            Session::new("model".into(), protocol, "system".into(), EditMode::Replace);
        session.set_think_level(ThinkLevel::None);
        start(&mut journal, &mut session, dir.path(), "first");
        journal.event(&AgentEvent::AssistantFinished { text: "one".into() });
        append_answer(&mut session, "one");
        journal.commit((&session).into()).unwrap();
        journal
            .custom(
                CustomRef::Instructions {
                    system: "changed instructions",
                },
                false,
            )
            .unwrap();
        match &mut session {
            Session::Completion(request) => {
                let ChatCompletionRequestMessage::System(message) = &mut request.messages[0] else {
                    unreachable!()
                };
                message.content = "changed instructions".into();
            }
            Session::Response(request) => {
                request.instructions = Some("changed instructions".into())
            }
        }
        let definitions = Value::Null;
        journal
            .custom(
                CustomRef::ToolDefinitions {
                    version: 1,
                    tools: &definitions,
                },
                false,
            )
            .unwrap();
        session.restore_tools(definitions).unwrap();
        journal.think(ThinkLevel::High).unwrap();
        session.set_think_level(ThinkLevel::High);
        let meta = metadata(dir.path());
        let workflow = WorkflowState::default();
        let mut state = StateRef::new(&meta, (&session).into(), &workflow);
        state.session = None;
        journal.synced_custom(CustomRef::State(state)).unwrap();
        start(&mut journal, &mut session, dir.path(), "second");
        journal.event(&AgentEvent::AssistantFinished { text: "two".into() });
        append_answer(&mut session, "two");
        journal.commit((&session).into()).unwrap();
        let view = journal.tree_view().unwrap();
        let last = view.tree.nodes.last().unwrap();
        let prepared = journal.prepare_tree(&last.id).unwrap();
        assert_eq!(
            serde_json::to_value(&prepared.session).unwrap(),
            serde_json::to_value(&session).unwrap()
        );
        let id = journal.id().unwrap().to_owned();
        drop(journal);
        let restored = load_test(dir.path(), &dir.path().join(".locks"), Some(&id), false).unwrap();
        assert_eq!(
            serde_json::to_value(&restored.checkpoint.session).unwrap(),
            serde_json::to_value(&session).unwrap()
        );
    }
}

#[test]
fn slimming_failed_upgrade_keeps_legacy_writer_and_original_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let id = frozen_legacy(dir.path(), Protocol::Completion);
    let path = dir.path().join(format!("{id}.jsonl"));
    let before = fs::read(&path).unwrap();
    let mut restored = load_test(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
    restored.journal.use_read_only_file(&path);
    assert!(
        restored
            .journal
            .sent(
                &metadata(dir.path()),
                (&restored.checkpoint.session).into(),
                &WorkflowState::default(),
                None,
                None
            )
            .is_err()
    );
    assert_eq!(restored.journal.replay_version, 0);
    assert_eq!(fs::read(path).unwrap(), before);
    assert!(restored.journal.check().is_err());
}
