use super::*;
use crate::models::Protocol;

#[test]
fn session_id_requires_calendar_time_and_lowercase_random_suffix() {
    for time in ["2026-10-17T01-02-03", "2024-02-29T23-59-59"] {
        assert!(validate_id(&format!("{time}-{}", "0123456789abcdef".repeat(2))).is_ok());
    }
    for time in [
        "1700000000000",
        "2026-02-29T01-02-03",
        "2026-04-31T01-02-03",
        "2026-13-17T01-02-03",
        "2026-10-17T24-02-03",
        "2026-10-17T01-60-03",
        "2026-10-17T01:02:03",
        "2026-10-17t01-02-03",
        "2026-1-17T01-02-03",
        "../2026-10-17T01-02-03",
    ] {
        assert!(
            validate_id(&format!("{time}-{}", "00".repeat(16))).is_err(),
            "{time}"
        );
    }
    for random in [
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcde",
        "0123456789abcdef0123456789abcdeg",
        "0123456789abcdef0123456789abcdef/../escape",
    ] {
        assert!(validate_id(&format!("2026-10-17T01-02-03-{random}")).is_err());
    }
}

#[test]
fn listing_reads_preview_size_and_latest_turn_status_without_native_content() {
    let dir = tempfile::tempdir().unwrap();
    let id = format!("2026-10-17T01-02-03-{}", "a".repeat(32));
    let path = dir.path().join(format!("{id}.jsonl"));
    let header = serde_json::json!({"type":"session", "version":3, "id":id,
            "timestamp":"2026-10-17T01:02:03Z", "cwd":"/project"});
    let mut data = format!(
        "{header}\n{}\n{}\n",
        serde_json::json!({"type":"message",
            "message":{"role":"user", "content":"  修复\n测试 "}}),
        serde_json::json!({"type":"message",
            "message":{"role":"assistant", "content":[{"type":"thinking", "thinking":"private thought"},
            {"type":"text", "text":"已经\n修复"}], "stopReason":"stop"}})
    );
    fs::write(&path, &data).unwrap();
    let listing = indexed_listing(dir.path()).unwrap();
    let summary = &listing.sessions[0];
    assert_eq!(summary.summary, "修复 测试");
    assert_eq!(summary.preview, "已经 修复");
    assert_eq!(summary.bytes, data.len() as u64);
    assert!(summary.done);
    for tail in [
        "{\"type\":\"custom\",\"customType\":\"pp.error\",\"data\":{\"message\":\"safe error\"}}\n",
        "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"next turn\"}}\n",
        "{\"type\":\"custom\",\"customType\":\"pp.cancelled\",\"data\":{}}\n",
        "{partial",
    ] {
        let original = data.clone();
        data.push_str(tail);
        fs::write(&path, &data).unwrap();
        assert!(!indexed_listing(dir.path()).unwrap().sessions[0].done);
        data = original;
    }
}

#[test]
fn deletion_rejects_locked_sessions_symlinks_and_invalid_ids() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let id = format!("2026-10-17T01-02-03-{}", "a".repeat(32));
    let link_id = format!("2026-10-17T01-02-03-{}", "b".repeat(32));
    let path = dir.path().join(format!("{id}.jsonl"));
    fs::write(&path, "original bytes").unwrap();
    let guard = SessionLock::acquire(&dir.path().join(".locks"), &id).unwrap();
    assert!(
        delete(dir.path(), &dir.path().join(".locks"), &id)
            .unwrap_err()
            .to_string()
            .contains("already in use")
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), "original bytes");
    drop(guard);
    let link = dir.path().join(format!("{link_id}.jsonl"));
    symlink(&path, &link).unwrap();
    assert!(delete(dir.path(), &dir.path().join(".locks"), &link_id).is_err());
    assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    assert!(delete(dir.path(), &dir.path().join(".locks"), "../escape").is_err());
    delete(dir.path(), &dir.path().join(".locks"), &id).unwrap();
    assert!(!path.exists());
    assert!(delete(dir.path(), &dir.path().join(".locks"), &id).is_err());
}

#[test]
fn listing_is_read_only_sorted_and_isolates_invalid_sessions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    assert!(
        indexed_listing(&dir.path().join("missing"))
            .unwrap()
            .sessions
            .is_empty()
    );
    let mut paths = Vec::new();
    for n in 0..5 {
        let id = format!("2026-10-17T01-02-03-{n:032x}");
        let path = dir.path().join(format!("{id}.jsonl"));
        let header = serde_json::json!({"type":"session", "version":3, "id":id, "timestamp":"2026-10-17T01:02:03Z", "cwd":format!("/project/{n}")});
        let user = if n == 0 {
            "中文".repeat(50)
        } else {
            "  first\n\t user\u{0001} message  ".into()
        };
        let mut data = format!("{header}\n");
        if n != 4 {
            data.push_str(&format!(
                "{}\n",
                serde_json::json!({"type":"custom","data":{"huge":"ignored"}})
            ));
            data.push_str(&format!(
                "{}\n",
                serde_json::json!({"type":"message","message":{"role":"assistant","content":[]}})
            ));
            if n == 3 {
                data.push_str("broken before user\n");
            }
            data.push_str(&format!(
                "{}\n",
                serde_json::json!({"type":"message","message":{"role":"user","content":user}})
            ));
            data.push_str("broken after user\n{partial");
        }
        fs::write(&path, data).unwrap();
        File::open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(
                UNIX_EPOCH + std::time::Duration::from_secs(if n == 0 { 20 } else { 10 }),
            ))
            .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        paths.push(path);
    }
    fs::write(dir.path().join("invalid.jsonl"), "broken").unwrap();
    let bad = dir
        .path()
        .join(format!("2026-10-17T01-02-03-{}.jsonl", "f".repeat(32)));
    fs::write(&bad, "bad header\n").unwrap();
    let link = dir
        .path()
        .join(format!("2026-10-17T01-02-03-{}.jsonl", "e".repeat(32)));
    symlink(&paths[0], link).unwrap();
    let before: Vec<_> = paths
        .iter()
        .map(|p| {
            (
                fs::read(p).unwrap(),
                fs::metadata(p).unwrap().modified().unwrap(),
            )
        })
        .collect();
    let _locked = SessionLock::acquire(
        &dir.path().join(".locks"),
        &format!("2026-10-17T01-02-03-{:032x}", 0),
    )
    .unwrap();
    let listing = indexed_listing(dir.path()).unwrap();
    assert_eq!(
        listing
            .sessions
            .iter()
            .map(|s| s.id.as_str())
            .collect::<Vec<_>>(),
        [0, 2, 1].map(|n| format!("2026-10-17T01-02-03-{n:032x}"))
    );
    assert_eq!(
        listing.sessions[0].summary,
        format!("{}…", "中文".repeat(40))
    );
    assert_eq!(listing.sessions[1].summary, "first user message");
    assert_eq!(listing.sessions[1].cwd, Path::new("/project/2"));
    for suffix in [
        "0".repeat(32),
        format!("{:032x}", 1),
        format!("{:032x}", 2),
        format!("{:032x}", 3),
        "f".repeat(32),
    ] {
        assert!(
            listing
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains(&suffix))
        );
    }
    assert!(
        listing
            .diagnostics
            .iter()
            .all(|d| !d.contains("broken before") && !d.contains("bad header"))
    );
    for (path, (bytes, modified)) in paths.iter().zip(before) {
        assert_eq!(fs::read(path).unwrap(), bytes);
        assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
    }
}

#[test]
fn listing_rejects_invalid_headers_and_ignores_unfinished_user_records() {
    let dir = tempfile::tempdir().unwrap();
    let id = format!("2026-10-17T01-02-03-{}", "0".repeat(32));
    let path = dir.path().join(format!("{id}.jsonl"));
    let header = serde_json::json!({"type":"session", "version":3, "id":id, "timestamp":"2026-10-17T01:02:03Z", "cwd":"/project"});
    let user = serde_json::json!({"type":"message", "message":{"role":"user", "content":"first"}});
    for (key, value) in [
        ("type", serde_json::json!("message")),
        ("version", serde_json::json!(2)),
        ("id", serde_json::json!("mismatch")),
        ("timestamp", serde_json::json!("invalid")),
        ("cwd", serde_json::json!("relative")),
    ] {
        let mut invalid = header.clone();
        invalid[key] = value;
        fs::write(&path, format!("{invalid}\n{user}\n")).unwrap();
        let listing = indexed_listing(dir.path()).unwrap();
        assert!(listing.sessions.is_empty(), "{key}");
        assert_eq!(listing.diagnostics.len(), 1, "{key}");
    }
    fs::write(&path, format!("{header}\n{user}")).unwrap();
    let listing = indexed_listing(dir.path()).unwrap();
    assert!(listing.sessions.is_empty());
    assert!(listing.diagnostics.is_empty());
    fs::write(&path, format!("{header}\n{user}\n{{partial")).unwrap();
    let listing = indexed_listing(dir.path()).unwrap();
    assert_eq!(listing.sessions[0].summary, "first");
    assert!(listing.diagnostics.is_empty());
    fs::write(
        &path,
        format!(
            "{header}\n{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":[]}}}}\n"
        ),
    )
    .unwrap();
    let listing = indexed_listing(dir.path()).unwrap();
    assert!(listing.sessions.is_empty());
    assert_eq!(listing.diagnostics.len(), 1);
}

#[test]
fn collision_preserves_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let time = DateTime::from_timestamp_millis(42)
        .unwrap()
        .with_timezone(&Local)
        .format(ID_TIME_FORMAT)
        .to_string();
    let path = dir.path().join(format!("{time}-{}.jsonl", "00".repeat(16)));
    fs::write(&path, "original").unwrap();
    let _held = SessionLock::acquire(
        &dir.path().join(".locks"),
        &format!("{time}-{}", "00".repeat(16)),
    )
    .unwrap();
    let mut candidate = 0;
    let active = create(dir.path(), &dir.path().join(".locks"), 42, || {
        let bytes = [candidate; 16];
        candidate += 1;
        Ok(bytes)
    })
    .unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), "original");
    assert_eq!(active.id, format!("{time}-{}", "01".repeat(16)));
}

#[test]
fn replay_native_history_and_repair_only_on_append() {
    for protocol in [Protocol::Completion, Protocol::Response] {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            "fixture".into(),
            protocol,
            "system".into(),
            EditMode::Replace,
        );
        session.set_think_level(ThinkLevel::None);
        let metadata = Metadata {
            role_route: None,
            cwd: dir.path(),
            model_reference: "fixture/primary",
            think_level: ThinkLevel::None,
            edit_mode: EditMode::Replace,
            tool_limits: ToolLimits::default(),
            base_system: "system",
            prompt_source: None,
        };
        let mut journal = SessionJournal::new(dir.path().to_owned());
        journal
            .sent(
                &metadata,
                (&session).into(),
                &WorkflowState::default(),
                Some("user"),
                None,
            )
            .unwrap();
        let id = journal.id().unwrap().to_owned();
        let path = dir.path().join(format!("{id}.jsonl"));
        drop(journal);
        let complete = fs::read(&path).unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{unfinished")
            .unwrap();
        let mut restored = load(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
        assert_eq!(
            serde_json::to_value(&restored.checkpoint.session).unwrap(),
            serde_json::to_value(&session).unwrap()
        );
        assert!(matches!(restored.replay.as_slice(), [ReplayRecord::User(text)] if text == "user"));
        assert_eq!(fs::read(&path).unwrap().len(), complete.len() + 11);
        restored.journal.think(ThinkLevel::High).unwrap();
        drop(restored);
        let restored = load(dir.path(), &dir.path().join(".locks"), Some(&id), false).unwrap();
        assert_eq!(restored.checkpoint.think_level, ThinkLevel::High);
        assert!(restored.replay.is_empty());
    }
}

#[test]
fn incomplete_native_commit_never_restores_projections_or_model_audit() {
    for protocol in [Protocol::Completion, Protocol::Response] {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new(
            "fixture".into(),
            protocol,
            "system".into(),
            EditMode::Replace,
        );
        session.set_think_level(ThinkLevel::None);
        let metadata = Metadata {
            role_route: None,
            cwd: dir.path(),
            model_reference: "fixture/primary",
            think_level: ThinkLevel::None,
            edit_mode: EditMode::Replace,
            tool_limits: ToolLimits::default(),
            base_system: "system",
            prompt_source: None,
        };
        let mut journal = SessionJournal::new(dir.path().to_owned());
        journal
            .sent(
                &metadata,
                (&session).into(),
                &WorkflowState::default(),
                Some("user"),
                None,
            )
            .unwrap();
        journal
            .record(
                PayloadRef::Message {
                    message: &Message::Assistant {
                        api: "openai-completions",
                        provider: "fixture",
                        model: "fixture",
                        content: vec![format::Content::Text {
                            text: "not native committed".into(),
                        }],
                        stop_reason: "stop",
                        timestamp: 42,
                    },
                },
                false,
            )
            .unwrap();
        journal
            .record(
                PayloadRef::ModelChange {
                    model: "fixture/not-committed",
                    role: "default",
                },
                false,
            )
            .unwrap();
        let previous_tail = journal.active.as_ref().unwrap().tail.clone();
        let id = journal.id().unwrap().to_owned();
        let path = dir.path().join(format!("{id}.jsonl"));
        drop(journal);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":\"custom\",\"customType\":\"pp.native_append\"")
            .unwrap();
        let mut restored = load(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
        assert_eq!(restored.checkpoint.model_reference, "fixture/primary");
        assert_eq!(
            serde_json::to_value(&restored.checkpoint.session).unwrap(),
            serde_json::to_value(&session).unwrap()
        );
        assert!(matches!(restored.replay.as_slice(), [ReplayRecord::User(text)] if text == "user"));
        assert_eq!(
            restored.journal.active.as_ref().unwrap().tail,
            previous_tail
        );
        restored.journal.think(ThinkLevel::High).unwrap();
        drop(restored);
        let records: Vec<serde_json::Value> = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.last().unwrap()["parentId"], previous_tail.unwrap());
        assert_eq!(
            records
                .iter()
                .filter(|record| record["type"] == "session")
                .count(),
            1
        );
        assert_eq!(
            load(dir.path(), &dir.path().join(".locks"), Some(&id), false)
                .unwrap()
                .checkpoint
                .think_level,
            ThinkLevel::High
        );
    }
}

#[test]
fn context_failed_history_replacement_restores_native_memory_and_disk() {
    use crate::{
        agent::Agent, cancellation::CancellationToken, config::ModelCatalog, tools::Tools,
    };
    use serde_json::json;
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let models = dir.path().join("models.yml");
        fs::write(&models, json!({"providers":{"fixture": {"protocol":protocol,"api_key":"fixture-key","base_url":format!("http://{}/v1",server.server_addr()),"models":{"primary": {"model":"fixture-model"}}}}}).to_string()).unwrap();
        let catalog = ModelCatalog::from_files(&dir.path().join("settings.yml"), &models).unwrap();
        let mut agent = Agent::new(
            catalog,
            Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
            dir.path().join("sessions"),
        )
        .unwrap();
        for index in 0..3 {
            let text = format!("old-{index} {}", "retained-data ".repeat(5000));
            match &mut agent.session {
                Session::Completion(request) => {
                    request.messages.push(
                        serde_json::from_value(json!({"role":"user","content":text})).unwrap(),
                    );
                    request.messages.push(
                        serde_json::from_value(
                            json!({"role":"assistant","content":format!("answer-{index}")}),
                        )
                        .unwrap(),
                    );
                }
                Session::Response(request) => {
                    let InputParam::Items(input) = &mut request.input else {
                        panic!()
                    };
                    input.push(
                        serde_json::from_value(json!({"role":"user","content":text})).unwrap(),
                    );
                    input.push(serde_json::from_value(json!({"type":"message","id":format!("msg-{index}"),"role":"assistant","status":"completed","content":[{"type":"output_text","text":format!("answer-{index}"),"annotations":[],"logprobs":null}]})).unwrap());
                }
            }
        }
        let metadata = Metadata {
            role_route: None,
            cwd: agent.tools.cwd(),
            model_reference: &agent.current_model.reference,
            think_level: agent.think_level,
            edit_mode: agent.tools.edit_mode(),
            tool_limits: agent.tools.limits,
            base_system: &agent.base_system,
            prompt_source: agent.prompt_source.as_ref(),
        };
        agent
            .journal
            .sent(
                &metadata,
                (&agent.session).into(),
                &WorkflowState::default(),
                None,
                None,
            )
            .unwrap();
        let id = agent.session_id().unwrap().to_owned();
        let path = agent.journal.dir.join(format!("{id}.jsonl"));
        let before = fs::read(&path).unwrap();
        let native = serde_json::to_value(&agent.session).unwrap();
        agent.journal.active.as_mut().unwrap().file = File::open(&path).unwrap();
        let responder = std::thread::spawn(move || {
            let mut request = server
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap()
                .expect("summary request");
            let body: serde_json::Value = serde_json::from_reader(request.as_reader()).unwrap();
            assert!(body.get("tools").is_none_or(serde_json::Value::is_null));
            assert!(
                !body.to_string().contains("old-2"),
                "latest exchange must not enter summary request"
            );
            let sse = if protocol == "completion" {
                format!(
                    "data: {}\n\ndata: [DONE]\n\n",
                    json!({"id":"summary","object":"chat.completion.chunk","created":1,"model":"fixture-model","choices":[{"index":0,"delta":{"role":"assistant","content":"short factual summary"},"finish_reason":"stop"}]})
                )
            } else {
                format!(
                    "data: {}\n\n",
                    json!({"type":"response.completed","sequence_number":1,"response":{"id":"summary","object":"response","created_at":1,"model":"fixture-model","status":"completed","output":[{"type":"message","id":"summary-message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"short factual summary","annotations":[],"logprobs":null}]}]}})
                )
            };
            request
                .respond(tiny_http::Response::from_string(sse).with_header(
                    tiny_http::Header::from_bytes("Content-Type", "text/event-stream").unwrap(),
                ))
                .unwrap();
            assert!(
                server
                    .recv_timeout(std::time::Duration::from_millis(100))
                    .unwrap()
                    .is_none()
            );
        });
        let mut finished = false;
        let error = agent
            .compact(&CancellationToken::new(), &mut |event| {
                finished |= matches!(event, AgentEvent::CompactionFinished { .. });
            })
            .unwrap_err()
            .to_string();
        responder.join().unwrap();
        assert!(error.contains("persisting session"), "{error}");
        assert!(
            !finished,
            "failed checkpoint cannot announce compaction success"
        );
        assert_eq!(serde_json::to_value(&agent.session).unwrap(), native);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(agent.journal.check().is_err());
        drop(agent);
        let restored = load(
            &dir.path().join("sessions"),
            &dir.path().join("sessions/.locks"),
            Some(&id),
            false,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&restored.checkpoint.session).unwrap(),
            native
        );
    }
}

#[test]
fn write_failure_preserves_settings_and_blocks_requests_until_reset() {
    use crate::{agent::Agent, config::ModelCatalog, tools::Tools};
    let dir = tempfile::tempdir().unwrap();
    let models = dir.path().join("models.yml");
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    fs::write(&models, serde_json::json!({"providers":{
            "a": {"protocol":"completion","api_key":"fixture", "base_url":format!("http://{}/v1", server.server_addr()),
             "models":{"one": {"model":"one"},"two": {"model":"two","thinking":{"defaultLevel":"max"}}}},
            "b": {"protocol":"response","api_key":"fixture", "models":{"one": {"model":"other","thinking":{"defaultLevel":"high"}},"two": {"model":"two","thinking":{"defaultLevel":"max"}}}}}}).to_string()).unwrap();
    crate::agent_store::initialize(dir.path(), &crate::cancellation::CancellationToken::new())
        .unwrap();
    for (source, target) in [
        ("a/one", "a/two"),
        ("a/one", "b/one"),
        ("b/one", "b/two"),
        ("b/one", "a/one"),
        ("a/one", "effort"),
    ] {
        fs::write(dir.path().join("settings.yml"), "{}\n").unwrap();
        crate::agent_store::AgentStore::open(&dir.path().join("agent.db"))
            .unwrap()
            .set_default_model(source, crate::models::ModelThinking::default())
            .unwrap();
        let catalog = ModelCatalog::from_files(&dir.path().join("settings.yml"), &models).unwrap();
        let mut agent = Agent::new(
            catalog,
            Tools::new(dir.path().to_owned(), EditMode::Replace).unwrap(),
            dir.path().join("sessions"),
        )
        .unwrap();
        let original_level = agent.think_level;
        if let Session::Response(request) = &mut agent.session {
            request.reasoning.as_mut().unwrap().summary =
                Some(async_openai::types::responses::ReasoningSummary::Detailed);
        }
        let metadata = Metadata {
            role_route: None,
            cwd: agent.tools.cwd(),
            model_reference: &agent.current_model.reference,
            think_level: agent.think_level,
            edit_mode: agent.tools.edit_mode(),
            tool_limits: agent.tools.limits,
            base_system: &agent.base_system,
            prompt_source: agent.prompt_source.as_ref(),
        };
        agent
            .journal
            .sent(
                &metadata,
                (&agent.session).into(),
                &WorkflowState::default(),
                None,
                None,
            )
            .unwrap();
        let path = agent
            .journal
            .dir
            .join(format!("{}.jsonl", agent.session_id().unwrap()));
        let before = fs::read(&path).unwrap();
        let native = serde_json::to_value(&agent.session).unwrap();
        // A read-only descriptor deterministically rejects writes even as root.
        agent.journal.active.as_mut().unwrap().file = File::open(&path).unwrap();
        let error = if target == "effort" {
            agent
                .set_think_level(ThinkLevel::High)
                .unwrap_err()
                .to_string()
        } else {
            agent.switch_model(target).unwrap_err().to_string()
        };
        assert!(error.contains("persisting session"));
        assert_eq!(agent.current_model.reference, source);
        assert_eq!(serde_json::to_value(&agent.session).unwrap(), native);
        assert_eq!(
            agent
                .set_think_level(ThinkLevel::High)
                .unwrap_err()
                .to_string(),
            error
        );
        assert_eq!(agent.think_level, original_level);
        assert_eq!(serde_json::to_value(&agent.session).unwrap(), native);
        assert_eq!(
            agent
                .continue_session(
                    &crate::cancellation::CancellationToken::new(),
                    None,
                    &mut |_| {},
                )
                .unwrap_err()
                .to_string(),
            error
        );
        assert!(
            server
                .recv_timeout(std::time::Duration::from_millis(30))
                .unwrap()
                .is_none()
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        agent.reset();
        assert!(agent.session_id().is_none());
        agent.set_think_level(ThinkLevel::High).unwrap();
        assert_eq!(agent.think_level, ThinkLevel::High);
    }
}

#[test]
fn initial_creation_failure_is_latched_without_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("sessions");
    fs::write(&blocked, "not a directory").unwrap();
    let session = Session::new(
        "fixture".into(),
        Protocol::Response,
        "system".into(),
        EditMode::Replace,
    );
    let metadata = Metadata {
        role_route: None,
        cwd: dir.path(),
        model_reference: "fixture/primary",
        think_level: ThinkLevel::None,
        edit_mode: EditMode::Replace,
        tool_limits: ToolLimits::default(),
        base_system: "system",
        prompt_source: None,
    };
    let mut journal = SessionJournal::new(blocked.clone());
    let error = journal
        .sent(
            &metadata,
            (&session).into(),
            &WorkflowState::default(),
            Some("task"),
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("persisting session"));
    assert_eq!(journal.check().unwrap_err().to_string(), error);
    assert!(journal.id().is_none());
    assert_eq!(fs::read_to_string(blocked).unwrap(), "not a directory");
}

#[test]
fn rejects_complete_corruption_and_locked_files() {
    let dir = tempfile::tempdir().unwrap();
    let session = Session::new(
        "fixture".into(),
        Protocol::Completion,
        "system".into(),
        EditMode::Replace,
    );
    let metadata = Metadata {
        role_route: None,
        cwd: dir.path(),
        model_reference: "fixture/primary",
        think_level: ThinkLevel::None,
        edit_mode: EditMode::Replace,
        tool_limits: ToolLimits::default(),
        base_system: "system",
        prompt_source: None,
    };
    let mut journal = SessionJournal::new(dir.path().to_owned());
    journal
        .sent(
            &metadata,
            (&session).into(),
            &WorkflowState::default(),
            None,
            None,
        )
        .unwrap();
    let id = journal.id().unwrap().to_owned();
    assert!(
        load(dir.path(), &dir.path().join(".locks"), Some(&id), false)
            .err()
            .unwrap()
            .to_string()
            .contains("already in use")
    );
    drop(journal);
    let path = dir.path().join(format!("{id}.jsonl"));
    let next_line = fs::read(&path)
        .unwrap()
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        + 1;
    OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"invalid\n")
        .unwrap();
    let error = load(dir.path(), &dir.path().join(".locks"), Some(&id), false)
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains(&format!("line {next_line}")));
}

fn plan_workflow() -> WorkflowState {
    WorkflowState {
        mode: WorkMode::Plan,
        plan: Some(crate::plan::PlanDocument {
            id: "0123456789abcdef0123456789abcdef".into(),
            file_name: None,
            location: crate::plan::PlanLocation::Plans,
            approved_tasks: None,
            revision: 1,
            title: "Create file".into(),
            content: "- [ ] Create hello.txt; done when bytes match.\n".into(),
            status: crate::plan::PlanStatus::Ready,
            origin_prompt: "Create hello.txt".into(),
        }),
        ..Default::default()
    }
}

fn test_metadata(dir: &std::path::Path) -> Metadata<'_> {
    Metadata {
        role_route: None,
        cwd: dir,
        model_reference: "fixture/primary",
        think_level: ThinkLevel::None,
        edit_mode: EditMode::Replace,
        tool_limits: ToolLimits::default(),
        base_system: "system",
        prompt_source: None,
    }
}

#[test]
fn instruction_loading_cards_journal_failure_keeps_unpublished_snapshot() {
    for protocol in [Protocol::Completion, Protocol::Response] {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = SessionJournal::new(dir.path().to_owned());
        let metadata = test_metadata(dir.path());
        let session = Session::new("model".into(), protocol, "system".into(), EditMode::Replace);
        journal
            .sent(
                &metadata,
                (&session).into(),
                &WorkflowState::default(),
                Some("accepted input"),
                None,
            )
            .unwrap();
        let id = journal.id().unwrap().to_owned();
        let path = dir.path().join(format!("{id}.jsonl"));
        // 用只读句柄制造真实写入失败，不能把未落盘的加载报告为成功。
        journal.active.as_mut().unwrap().file = File::open(&path).unwrap();
        let mut pending = Some("frozen project body".to_owned());
        let mut reported = None;
        let mut events = Vec::new();
        assert!(
            super::super::turn::publish_project_load(
                &mut pending,
                &mut reported,
                &mut journal,
                dir.path(),
                &mut |event| events.push(event),
            )
            .is_err()
        );
        assert!(events.is_empty());
        assert_eq!(pending.as_deref(), Some("frozen project body"));
        assert!(reported.is_none());
        drop(journal);

        let mut restored = load(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
        assert!(!restored.replay.iter().any(|record| matches!(
            record,
            ReplayRecord::Event(AgentEvent::ProjectInstructionsLoaded { .. })
        )));
        super::super::turn::publish_project_load(
            &mut pending,
            &mut reported,
            &mut restored.journal,
            dir.path(),
            &mut |event| events.push(event),
        )
        .unwrap();
        assert!(matches!(
            events.as_slice(),
            [AgentEvent::ProjectInstructionsLoaded { content, path }]
                if content == "frozen project body"
                    && path == &dir.path().join("AGENTS.md").display().to_string()
        ));
        drop(restored);
        let restored = load(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
        assert!(matches!(
            restored.replay.as_slice(),
            [ReplayRecord::User(user), ReplayRecord::Event(
                AgentEvent::ProjectInstructionsLoaded { content, .. }
            )] if user == "accepted input" && content == "frozen project body"
        ));
    }
}

#[test]
fn stats_context_early_journal_does_not_consume_capture_and_restore_does_not_rearm() {
    for protocol in [Protocol::Completion, Protocol::Response] {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = SessionJournal::new(dir.path().to_owned());
        let metadata = test_metadata(dir.path());
        let session = Session::new("model".into(), protocol, "system".into(), EditMode::Replace);
        journal
            .sent(
                &metadata,
                (&session).into(),
                &WorkflowState::default(),
                Some("accepted input"),
                None,
            )
            .unwrap();
        let context = journal.stats_context(&metadata, "task", &[]);
        assert!(context.first_request.load(Ordering::Relaxed));
        let native = match &session {
            Session::Completion(request) => serde_json::to_value(request).unwrap(),
            Session::Response(request) => serde_json::to_value(request).unwrap(),
        };
        assert_eq!(context.capture(&native).unwrap().request, native);
        assert!(
            journal
                .stats_context(&metadata, "task", &[])
                .capture(&native)
                .is_none()
        );
        let id = journal.id().unwrap().to_owned();
        drop(journal);
        let restored = load(dir.path(), &dir.path().join(".locks"), Some(&id), false).unwrap();
        assert!(
            restored
                .journal
                .stats_context(&metadata, "task", &[])
                .capture(&native)
                .is_none()
        );
        let mut journal = restored.journal;
        journal.reset();
        assert!(
            journal
                .stats_context(&metadata, "task", &[])
                .capture(&native)
                .is_some()
        );
    }
}

fn rewrite_records(path: &Path, mutate: impl Fn(&mut serde_json::Value)) {
    let mut out = String::new();
    for line in fs::read_to_string(path).unwrap().lines() {
        let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
        mutate(&mut record);
        if record.is_null() {
            continue;
        }
        out.push_str(&record.to_string());
        out.push('\n');
    }
    fs::write(path, out).unwrap();
}

fn read_only(path: &Path, journal: &mut SessionJournal) {
    journal.use_read_only_file(path);
}

#[test]
fn workflow_state_survives_restore_without_moving_native_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let metadata = test_metadata(dir.path());
    let mut session = Session::new(
        "fixture".into(),
        Protocol::Response,
        "system".into(),
        EditMode::Replace,
    );
    session.set_think_level(ThinkLevel::None);
    super::super::turn::response_input(match &mut session {
        Session::Response(request) => request,
        _ => unreachable!(),
    })
    .push(
        serde_json::from_value(serde_json::json!({"role":"user","content":"plan task"})).unwrap(),
    );
    let workflow = plan_workflow();
    let mut journal = SessionJournal::new(dir.path().to_owned());
    journal
        .sent(
            &metadata,
            (&session).into(),
            &workflow,
            Some("plan task"),
            None,
        )
        .unwrap();
    let id = journal.id().unwrap().to_owned();
    let path = dir.path().join(format!("{id}.jsonl"));
    let before = fs::read(&path).unwrap();
    let nodes_before = journal.tree().unwrap().nodes.len();

    // Workflow-only updates append their own strict versioned record and
    // never touch native history or the logical tree.
    let mut updated = workflow.clone();
    updated.plan.as_mut().unwrap().revision = 2;
    updated.plan.as_mut().unwrap().content = "- [x] Create hello.txt\n".into();
    journal.write_workflow(&updated).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains(r#""customType":"pp.workflow""#));
    assert!(text.contains(r#""version":1"#));
    assert!(fs::read(&path).unwrap().starts_with(&before));
    assert_eq!(journal.tree().unwrap().nodes.len(), nodes_before);
    drop(journal);

    let restored = load(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
    assert_eq!(restored.work_mode(), WorkMode::Plan);
    assert_eq!(restored.checkpoint.workflow, updated);
    assert!(
        matches!(restored.replay.as_slice(), [ReplayRecord::User(text)] if text == "plan task")
    );
    // The workflow record is not a replay action and does not move the
    // native cursor or the restore checkpoint's native history.
    assert_eq!(
        serde_json::to_value(&restored.checkpoint.session).unwrap(),
        serde_json::to_value(&session).unwrap()
    );
    assert_eq!(
        restored.journal.committed,
        SessionRef::from(&restored.checkpoint.session).len()
    );

    drop(restored);
    // A state record without a workflow field restores as Execute/None.
    rewrite_records(&path, |record| {
        if record["customType"] == "pp.session_state" {
            record["data"]
                .as_object_mut()
                .unwrap()
                .remove("workflow")
                .expect("state embeds workflow");
        }
        if record["customType"] == "pp.workflow" {
            *record = serde_json::Value::Null;
        }
    });
    let restored = load(dir.path(), &dir.path().join(".locks"), Some(&id), false).unwrap();
    assert_eq!(restored.checkpoint.workflow, WorkflowState::default());
    assert_eq!(restored.work_mode(), WorkMode::Execute);
}

#[test]
fn workflow_records_fail_closed_on_version_unknown_fields_and_privilege() {
    let dir = tempfile::tempdir().unwrap();
    let metadata = test_metadata(dir.path());
    let session = Session::new(
        "fixture".into(),
        Protocol::Completion,
        "system".into(),
        EditMode::Replace,
    );
    let mut journal = SessionJournal::new(dir.path().to_owned());
    journal
        .sent(
            &metadata,
            (&session).into(),
            &WorkflowState::default(),
            None,
            None,
        )
        .unwrap();
    let id = journal.id().unwrap().to_owned();
    let path = dir.path().join(format!("{id}.jsonl"));
    drop(journal);

    let mut ready = plan_workflow();
    ready.plan.as_mut().unwrap().content = "sensitive-plan-body".into();
    let ready_json = serde_json::to_value(&ready).unwrap();
    let base = fs::read_to_string(&path).unwrap();
    let records = [
        (
            serde_json::json!({"type":"custom", "customType":"pp.workflow",
                    "data":{"version":2, "workflow":{"mode":"execute", "plan":null, "review":null}}}),
            ("unsupported saved workflow version", None),
        ),
        (
            serde_json::json!({"type":"custom", "customType":"pp.workflow",
                    "data":{"version":1, "workflow":{"mode":"plan", "plan":null, "review":null}, "extra":1}}),
            ("invalid session record", None),
        ),
        (
            serde_json::json!({"type":"custom", "customType":"pp.workflow",
                    "data":{"version":1, "workflow":{"mode":"admin", "plan":null, "review":null}}}),
            ("invalid session record", None),
        ),
        (
            serde_json::json!({"type":"custom", "customType":"pp.workflow",
                    "data":{"version":1, "workflow":{"mode":"execute", "plan":ready_json["plan"], "review":null}}}),
            ("Invalid saved workflow state.", Some("sensitive-plan-body")),
        ),
    ];
    for (mut record, (expected, secret)) in records {
        let entries: Vec<serde_json::Value> = base
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        record["id"] = serde_json::json!("workflow-corruption");
        record["parentId"] = entries.last().unwrap()["id"].clone();
        record["timestamp"] = entries[0]["timestamp"].clone();
        fs::write(&path, format!("{base}{record}\n")).unwrap();
        let error = format!(
            "{:#}",
            load(dir.path(), &dir.path().join(".locks"), Some(&id), false)
                .err()
                .unwrap()
        );
        assert!(error.contains(expected), "{expected}: {error}");
        if let Some(secret) = secret {
            assert!(!error.contains(secret), "leaked saved content: {error}");
        }
    }

    // Corrupted embedded workflow inside pp.session_state also fails closed.
    rewrite_records(&path, |record| {
        if record["customType"] == "pp.session_state" {
            record["data"]["workflow"] = serde_json::json!({"mode":"admin"});
        }
        if record["customType"] == "pp.workflow" {
            *record = serde_json::Value::Null;
        }
    });
    let error = format!(
        "{:#}",
        load(dir.path(), &dir.path().join(".locks"), Some(&id), false)
            .err()
            .unwrap()
    );
    assert!(error.contains("invalid session record"), "{error}");
}

#[test]
fn failed_workflow_sync_rolls_back_and_blocks_until_reset() {
    let dir = tempfile::tempdir().unwrap();
    let metadata = test_metadata(dir.path());
    let session = Session::new(
        "fixture".into(),
        Protocol::Completion,
        "system".into(),
        EditMode::Replace,
    );
    let mut journal = SessionJournal::new(dir.path().to_owned());
    journal
        .sent(
            &metadata,
            (&session).into(),
            &WorkflowState::default(),
            None,
            None,
        )
        .unwrap();
    let path = dir.path().join(format!("{}.jsonl", journal.id().unwrap()));
    let id = journal.id().unwrap().to_owned();
    let mut updated = plan_workflow();
    updated.plan.as_mut().unwrap().revision = 3;
    journal.write_workflow(&updated).unwrap();
    let mut next = plan_workflow();
    next.plan.as_mut().unwrap().revision = 4;
    let before = fs::read(&path).unwrap();

    // A read-only descriptor deterministically rejects writes even as root;
    // the failed sync must neither grant the newer revision nor corrupt the
    // file, and the last successfully synced state still restores.
    read_only(&path, &mut journal);
    let error = journal.write_workflow(&next).unwrap_err().to_string();
    assert!(error.contains("persisting session"), "{error}");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(journal.check().is_err());
    drop(journal);
    let restored = load(dir.path(), &dir.path().join(".locks"), Some(&id), false).unwrap();
    assert_eq!(restored.work_mode(), WorkMode::Plan);
    assert_eq!(
        restored.checkpoint.workflow.plan.as_ref().unwrap().revision,
        3
    );
}

#[test]
fn commit_plan_handoff_commits_projection_and_state_atomically() {
    for protocol in [Protocol::Completion, Protocol::Response] {
        let dir = tempfile::tempdir().unwrap();
        let metadata = test_metadata(dir.path());
        let workflow = plan_workflow();
        let mut session = Session::new(
            "fixture".into(),
            protocol,
            "system".into(),
            EditMode::Replace,
        );
        match &mut session {
            Session::Completion(request) => request.messages.push(
                serde_json::from_value(serde_json::json!(
                    {"role":"user", "content":"implement the approved plan"}
                ))
                .unwrap(),
            ),
            Session::Response(request) => {
                let input = super::super::turn::response_input(request);
                input.push(
                    serde_json::from_value(serde_json::json!(
                        {"role":"user", "content":"implement the approved plan"}
                    ))
                    .unwrap(),
                );
            }
        }
        session.set_think_level(ThinkLevel::None);
        let mut journal = SessionJournal::new(dir.path().to_owned());
        journal
            .commit_plan_handoff(
                &metadata,
                (&session).into(),
                &workflow,
                "implement the approved plan",
            )
            .unwrap();
        let id = journal.id().unwrap().to_owned();
        assert_eq!(journal.take_notice().as_deref(), Some(id.as_str()));
        let path = dir.path().join(format!("{id}.jsonl"));
        let text = fs::read_to_string(&path).unwrap();
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first["type"], "session");
        drop(journal);
        let restored = load(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
        assert_eq!(restored.work_mode(), WorkMode::Plan);
        assert_eq!(restored.checkpoint.workflow, workflow);
        // The user projection appears exactly once; the native history
        // already contains the same message at the sync commit point.
        assert!(
            matches!(restored.replay.as_slice(), [ReplayRecord::User(text)] if text == "implement the approved plan")
        );
        assert_eq!(
            serde_json::to_value(&restored.checkpoint.session).unwrap(),
            serde_json::to_value(&session).unwrap()
        );
        assert_eq!(
            restored.journal.committed,
            SessionRef::from(&restored.checkpoint.session).len()
        );

        // fork_empty shares only the sessions directory.
        let mut planner = SessionJournal::new(dir.path().to_owned());
        planner
            .sent(
                &metadata,
                (&session).into(),
                &WorkflowState::default(),
                None,
                None,
            )
            .unwrap();
        let planned_id = planner.id().unwrap().to_owned();
        let planned_path = dir.path().join(format!("{planned_id}.jsonl"));
        let planned_before = fs::read(&planned_path).unwrap();
        let mut forked = planner.fork_empty();
        assert!(forked.id().is_none());
        forked
            .commit_plan_handoff(
                &metadata,
                (&session).into(),
                &workflow,
                "implement the approved plan",
            )
            .unwrap();
        let forked_id = forked.id().unwrap().to_owned();
        assert_ne!(forked_id, planned_id);
        drop(forked);
        // The planning journal and its file are untouched by the fork.
        assert_eq!(fs::read(&planned_path).unwrap(), planned_before);
    }
}

#[test]
fn commit_plan_handoff_failure_rolls_back_the_whole_group() {
    let dir = tempfile::tempdir().unwrap();
    let metadata = test_metadata(dir.path());
    let mut session = Session::new(
        "fixture".into(),
        Protocol::Response,
        "system".into(),
        EditMode::Replace,
    );
    let mut journal = SessionJournal::new(dir.path().to_owned());
    journal
        .sent(
            &metadata,
            (&session).into(),
            &WorkflowState::default(),
            Some("first"),
            None,
        )
        .unwrap();
    let path = dir.path().join(format!("{}.jsonl", journal.id().unwrap()));
    let id = journal.id().unwrap().to_owned();
    if let Session::Response(request) = &mut session {
        super::super::turn::response_input(request).push(
            serde_json::from_value(
                serde_json::json!({"role":"user", "content":"implement the approved plan"}),
            )
            .unwrap(),
        );
    }
    let before = fs::read(&path).unwrap();
    read_only(&path, &mut journal);
    let error = journal
        .commit_plan_handoff(
            &metadata,
            (&session).into(),
            &plan_workflow(),
            "implement the approved plan",
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("persisting session"), "{error}");
    assert_eq!(fs::read(&path).unwrap(), before);
    assert!(journal.check().is_err());
    drop(journal);
    let restored = load(dir.path(), &dir.path().join(".locks"), Some(&id), true).unwrap();
    // Neither the execution projection nor the workflow grant survived.
    assert!(matches!(restored.replay.as_slice(), [ReplayRecord::User(text)] if text == "first"));
    assert_eq!(restored.work_mode(), WorkMode::Execute);
    assert!(restored.checkpoint.workflow.plan.is_none());
}

#[test]
fn commit_plan_handoff_creation_failure_is_latched_without_a_session() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("sessions");
    fs::write(&blocked, "not a directory").unwrap();
    let session = Session::new(
        "fixture".into(),
        Protocol::Response,
        "system".into(),
        EditMode::Replace,
    );
    let mut journal = SessionJournal::new(blocked.clone());
    let error = journal
        .commit_plan_handoff(
            &test_metadata(dir.path()),
            (&session).into(),
            &plan_workflow(),
            "implement the approved plan",
        )
        .unwrap_err()
        .to_string();
    assert!(error.contains("persisting session"), "{error}");
    assert!(journal.id().is_none());
    assert_eq!(fs::read_to_string(blocked).unwrap(), "not a directory");
}
