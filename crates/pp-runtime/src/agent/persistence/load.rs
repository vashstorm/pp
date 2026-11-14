use super::format::{Custom, Entry, Header, NativeAppend, Payload};
use super::*;
use anyhow::ensure;
use async_openai::types::chat::ChatCompletionRequestMessage;
use pp_core::session_jsonl::Reader as SessionReader;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::BufReader;
use std::os::unix::fs::OpenOptionsExt;

pub fn load(
    dir: &Path,
    lock_dir: &Path,
    id: Option<&str>,
    include_replay: bool,
    stats_db: &Path,
) -> Result<RestoredSession> {
    let id = match id {
        Some(id) => {
            validate_id(id)?;
            id.to_owned()
        }
        None => latest(dir)?,
    };
    let session_lock = SessionLock::acquire(lock_dir, &id)?;
    let path = dir.join(format!("{id}.jsonl"));
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .custom_flags(
            rustix::fs::OFlags::NOFOLLOW.bits() as i32 | rustix::fs::OFlags::NONBLOCK.bits() as i32,
        )
        .open(&path)
        .with_context(|| format!("opening saved session {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "saved session is not a regular file: {}",
        path.display()
    );
    let mut reader = SessionReader::new(BufReader::new(&file));
    let mut line = Vec::new();
    let mut line_number = 0;
    let mut offset = 0;
    let mut truncated = false;
    let mut header: Option<Header> = None;
    let mut checkpoint: Option<Checkpoint> = None;
    let mut tail: Option<String> = None;
    let mut used_ids = HashSet::new();
    let mut projector = replay::Projector::default();
    let mut has_tree = false;
    loop {
        line.clear();
        let count = reader.read_record(&mut line).with_context(|| {
            format!(
                "reading session {} line {}",
                path.display(),
                line_number + 1
            )
        })?;
        if count == 0 {
            break;
        }
        line_number += 1;
        if !line.ends_with(b"\n") {
            truncated = true;
            break;
        }
        let result = (|| -> Result<()> {
            if header.is_none() {
                #[derive(Deserialize)]
                struct Kind<'a> {
                    #[serde(borrow, rename = "type")]
                    kind: &'a str,
                }
                let kind: Kind<'_> = serde_json::from_slice(&line)
                    .map_err(|_| anyhow::anyhow!("first record must be a session header"))?;
                ensure!(
                    kind.kind != "checkpoint",
                    "unsupported legacy session format"
                );
                ensure!(
                    kind.kind == "session",
                    "first record must be a session header"
                );
                let saved: Header = serde_json::from_slice(&line)
                    .map_err(|_| anyhow::anyhow!("invalid session record"))?;
                ensure!(
                    saved.kind == "session",
                    "first record must be a session header"
                );
                ensure!(saved.id == id, "session ID does not match filename");
                ensure!(
                    saved.cwd.is_absolute(),
                    "session working directory must be absolute"
                );
                parse_timestamp(&saved.timestamp)?;
                header = Some(saved);
                return Ok(());
            }
            let entry: Entry = serde_json::from_slice(&line)
                .map_err(|_| anyhow::anyhow!("invalid session record"))?;
            ensure!(
                !entry.id.is_empty() && !used_ids.contains(&entry.id) && entry.parent_id == tail,
                "invalid session entry chain"
            );
            parse_timestamp(&entry.timestamp)?;
            if include_replay && !has_tree {
                projector.feed(&entry.payload, &line)?;
            }
            if let Payload::Custom {
                custom: Custom::State(state),
            } = &entry.payload
                && state.tree.is_some()
            {
                has_tree = true;
                projector = replay::Projector::default();
            }
            apply_native(
                &mut checkpoint,
                header.as_ref().expect("header checked"),
                entry.payload,
            )?;
            used_ids.insert(entry.id.clone());
            tail = Some(entry.id);
            Ok(())
        })();
        result.with_context(|| format!("loading session {} line {line_number}", path.display()))?;
        offset += count as u64;
    }
    ensure!(
        header.is_some(),
        "loading session {} line 1: missing session header",
        path.display()
    );
    let checkpoint = checkpoint.with_context(|| {
        format!(
            "loading session {} line {line_number}: missing pp session state",
            path.display()
        )
    })?;
    if truncated {
        eprintln!("Ignoring incomplete final record in {}", path.display());
    }
    let committed = SessionRef::from(&checkpoint.session).len();
    let created_at_ms = checkpoint.created_at_ms;
    let model_reference = checkpoint.model_reference.clone();
    let capture_thinking = matches!(checkpoint.session, Session::Completion(_));
    let wire_model = match &checkpoint.session {
        Session::Completion(request) => request.model.clone(),
        Session::Response(request) => request.model.clone().unwrap_or_default(),
    };
    let storage = reader.into_storage();
    let mut replay = projector.finish()?;
    if has_tree {
        let tree = tree::Tree::read(&file)?;
        if include_replay {
            replay = tree.replay(&file, tree.leaf, true)?;
        }
    }
    Ok(RestoredSession {
        checkpoint,
        journal: SessionJournal {
            dir: dir.to_owned(),
            lock_dir: lock_dir.to_owned(),
            stats_db: stats_db.to_owned(),
            active: Some(Active {
                id,
                created_at_ms,
                file,
                _lock: session_lock,
                truncate_to: truncated.then_some(offset),
                header_written: true,
                tail,
                used_ids,
                storage,
            }),
            committed,
            capture: replay::Capture::default(),
            buffer: Vec::new(),
            first_error: None,
            notice: None,
            model_reference,
            wire_model,
            thinking: String::new(),
            capture_thinking,
            pending_checkpoint: false,
            pending_tools: false,
            pending_stats: None,
            last_stats: None,
            tool_started: HashMap::new(),
            pending_tool_stats: HashMap::new(),
            unlinked_stats: Default::default(),
            first_request: Arc::new(AtomicBool::new(false)),
        },
        replay,
    })
}

/// 逐条应用原生增量，每条 state 都验证有效基线与元数据，不能只验证最终结果。
pub(super) fn apply_native(
    checkpoint: &mut Option<Checkpoint>,
    header: &Header,
    payload: Payload,
) -> Result<()> {
    match payload {
        Payload::Custom {
            custom: Custom::State(state),
        } => {
            let saved = Checkpoint {
                role_route: state.role_route,
                id: header.id.clone(),
                created_at_ms: parse_timestamp(&header.timestamp)?,
                cwd: header.cwd.clone(),
                model_reference: state.model_reference,
                think_level: state.think_level,
                edit_mode: state.edit_mode,
                tool_limits: state.tool_limits,
                base_system: state.base_system,
                prompt_source: state.prompt_source,
                session: state
                    .session
                    .or_else(|| checkpoint.take().map(|saved| saved.session))
                    .context("missing pp session baseline")?,
                workflow: state.workflow,
            };
            validate_checkpoint(&saved, &header.id)?;
            *checkpoint = Some(saved);
        }
        Payload::Custom {
            custom: Custom::Workflow(record),
        } => {
            record.workflow.validate()?;
            let state = checkpoint
                .as_mut()
                .context("state change before pp session state")?;
            state.workflow = record.workflow;
        }
        Payload::Custom {
            custom: Custom::ToolDefinitions { tools },
        } => {
            checkpoint
                .as_mut()
                .context("tool definitions before pp session state")?
                .session
                .restore_tools(tools)?;
        }
        Payload::Custom {
            custom: Custom::Instructions { system },
        } => {
            let saved = checkpoint
                .as_mut()
                .context("state change before pp session state")?;
            match &mut saved.session {
                Session::Completion(request) => {
                    let Some(ChatCompletionRequestMessage::System(message)) =
                        request.messages.first_mut()
                    else {
                        bail!("missing system message");
                    };
                    message.content = system.into();
                }
                Session::Response(request) => request.instructions = Some(system),
            }
        }
        Payload::ThinkingLevelChange { thinking_level } => {
            let saved = checkpoint
                .as_mut()
                .context("state change before pp session state")?;
            saved.think_level = thinking_level;
            saved.session.set_think_level(thinking_level);
        }
        Payload::Custom {
            custom: Custom::NativeAppend(append),
        } => {
            let saved = checkpoint
                .as_mut()
                .context("state change before pp session state")?;
            match (&mut saved.session, append) {
                (Session::Completion(request), NativeAppend::Completion { messages }) => {
                    request.messages.extend(messages)
                }
                (Session::Response(request), NativeAppend::Response { items }) => {
                    let InputParam::Items(input) = &mut request.input else {
                        bail!("Responses input must be items");
                    };
                    input.extend(items);
                }
                _ => bail!("append protocol mismatch"),
            }
        }
        Payload::ModelChange { model, role } => {
            let format::DefaultRole::Default = role;
            ensure!(
                model
                    .split_once('/')
                    .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty()),
                "invalid saved model reference"
            );
            // Auditing only: a subsequent native state is the model switch commit point.
        }
        Payload::Message { .. }
        | Payload::Custom {
            custom: Custom::Event { .. } | Custom::Error { .. } | Custom::Cancelled {},
        } => {}
    }
    Ok(())
}

fn validate_checkpoint(saved: &Checkpoint, id: &str) -> Result<()> {
    ensure!(saved.id == id, "session ID does not match filename");
    ensure!(
        saved.cwd.is_absolute(),
        "session working directory must be absolute"
    );
    ensure!(
        saved
            .model_reference
            .split_once('/')
            .is_some_and(|(provider, model)| !provider.is_empty() && !model.is_empty()),
        "invalid saved model reference"
    );
    // 角色路由只作审计：恢复后的模型由进程级 `ModelState::resolve()`（a → b）决定，
    // 不再要求保存的模型等于路由主模型或备用模型。
    if let Some(route) = &saved.role_route {
        pp_config::config::RoleAssignment {
            enabled: true,
            model: route.primary.model.clone(),
            effort: route.primary.effort,
            fallback: route.fallback.clone(),
        }
        .validate()?;
    }
    saved.tool_limits.validate()?;
    saved.workflow.validate()?;
    if let Some(source) = &saved.prompt_source {
        source.validate()?;
    }
    match &saved.session {
        Session::Completion(request) => {
            ensure!(!request.model.is_empty(), "missing native model");
            ensure!(
                matches!(
                    request.messages.first(),
                    Some(ChatCompletionRequestMessage::System(_))
                ),
                "Completion first message must be system"
            );
        }
        Session::Response(request) => {
            ensure!(
                request
                    .model
                    .as_ref()
                    .is_some_and(|model| !model.is_empty()),
                "missing native model"
            );
            ensure!(
                matches!(request.input, InputParam::Items(_)),
                "Responses input must be items"
            );
        }
    }
    Ok(())
}
