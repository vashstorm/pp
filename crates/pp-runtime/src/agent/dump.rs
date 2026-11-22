//! 会话与子 Agent 数据导出：支持导出主 transcript、原生 LLM 请求 JSON、
//! 逐子 Agent JSONL 文件，以及主 Agent 最终结论 Markdown。

use super::{
    Agent, Session,
    persistence::{ReplayRecord, SessionRef},
};
use anyhow::{Context, Result, bail};
use async_openai::types::{
    chat::{
        ChatCompletionRequestAssistantMessageContent,
        ChatCompletionRequestAssistantMessageContentPart, ChatCompletionRequestMessage,
    },
    responses::{InputItem, InputParam, Item, MessageItem, OutputMessageContent},
};
use pp_core::{events::AgentEvent, session_jsonl::Reader as SessionReader};
use serde_json::{Value, json};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

const DEFAULT_DUMP_DIR: &str = "/tmp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpMode {
    All,
    Result,
}

impl DumpMode {
    pub const ALL: [Self; 2] = [Self::All, Self::Result];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Result => "result",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::All => "Dump main transcript, LLM request JSON, and one JSONL per subagent",
            Self::Result => "Dump main agent's latest conclusion as Markdown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpFile {
    pub label: String,
    pub path: PathBuf,
    pub file_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpOutput {
    pub mode: DumpMode,
    pub directory: PathBuf,
    pub files: Vec<DumpFile>,
}

impl DumpOutput {
    pub fn summary(&self) -> String {
        let mut lines = Vec::with_capacity(self.files.len() + 1);
        let heading = match self.mode {
            DumpMode::All => format!("Dumped session (all) to {}:", self.directory.display()),
            DumpMode::Result => format!(
                "Dumped main agent conclusion (result) to {}:",
                self.directory.display()
            ),
        };
        lines.push(heading);
        for file in &self.files {
            lines.push(format!(
                "- {}: {} ({})",
                file.label,
                file.path.display(),
                file.file_name
            ));
        }
        lines.join("\n")
    }

    pub fn notification(&self) -> String {
        if let [file] = self.files.as_slice() {
            return format!("Dumped to {} ({})", file.path.display(), file.file_name);
        }
        let names: Vec<&str> = self
            .files
            .iter()
            .map(|file| file.file_name.as_str())
            .collect();
        format!(
            "Dumped {} files to {}: {}",
            self.files.len(),
            self.directory.display(),
            names.join(", ")
        )
    }
}

impl Agent {
    /// 将当前会话数据或主 Agent 结论导出到 `/tmp`。
    pub fn dump(&mut self, mode: DumpMode) -> Result<DumpOutput> {
        self.dump_to(mode, Path::new(DEFAULT_DUMP_DIR))
    }

    /// 导出到指定目录（供生产 `/tmp` 与隔离测试共用）。
    pub fn dump_to(&mut self, mode: DumpMode, dir: &Path) -> Result<DumpOutput> {
        let id = match self.journal.id() {
            Some(id) => id.to_owned(),
            None => {
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .context("system clock before UNIX epoch")?
                    .as_millis();
                format!("unsaved-{ms}")
            }
        };
        let files = match mode {
            DumpMode::All => self.dump_all(&id, dir)?,
            DumpMode::Result => vec![self.dump_result(&id, dir)?],
        };
        Ok(DumpOutput {
            mode,
            directory: dir.to_path_buf(),
            files,
        })
    }

    fn dump_all(&mut self, id: &str, dir: &Path) -> Result<Vec<DumpFile>> {
        let mut files = Vec::new();

        // 1. Main transcript (展开共享值表后的标准 JSONL)
        let main_name = format!("pp-{id}-main.jsonl");
        let main_bytes = self.build_main_transcript_jsonl()?;
        let main_path = write_dump_file(dir, &main_name, &main_bytes)?;
        files.push(DumpFile {
            label: "Main transcript".into(),
            path: main_path,
            file_name: main_name,
        });

        // 2. LLM request JSON
        let request_name = format!("pp-{id}-request.json");
        let mut request_bytes = serde_json::to_vec_pretty(&SessionRef::from(&self.session))
            .context("serializing LLM request JSON")?;
        request_bytes.push(b'\n');
        let request_path = write_dump_file(dir, &request_name, &request_bytes)?;
        files.push(DumpFile {
            label: "LLM request JSON".into(),
            path: request_path,
            file_name: request_name,
        });

        // 3. One JSONL file per subagent
        if self.journal.id().is_some() || self.subagent.is_some() {
            self.ensure_subagents()?;
        }
        if let Some(binding) = self.subagents() {
            let snapshot = binding.hub_snapshot();
            let subagent_dir = binding.directory();
            for hub_agent in &snapshot.agents {
                let agent_id = &hub_agent.record.id;
                let runs: Vec<_> = snapshot
                    .runs
                    .iter()
                    .filter(|run| run.agent_id == *agent_id)
                    .collect();
                let mut bytes = Vec::new();
                push_json_line(
                    &mut bytes,
                    &json!({
                        "type": "subagent",
                        "agent": hub_agent,
                        "runs": runs,
                    }),
                )?;
                let session_path = subagent_dir.join(format!("{agent_id}.session.json"));
                match fs::read(&session_path) {
                    Ok(raw) => {
                        let snapshot_value: Value = serde_json::from_slice(&raw)
                            .with_context(|| format!("parsing {}", session_path.display()))?;
                        push_json_line(
                            &mut bytes,
                            &json!({
                                "type": "llm_request",
                                "agent_id": agent_id,
                                "snapshot": snapshot_value,
                            }),
                        )?;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error)
                            .with_context(|| format!("reading {}", session_path.display()));
                    }
                }
                let events_path = subagent_dir.join(format!("{agent_id}.events.jsonl"));
                append_events_jsonl(&events_path, &mut bytes)?;

                let sub_name = format!("pp-{id}-subagent-{agent_id}.jsonl");
                let sub_path = write_dump_file(dir, &sub_name, &bytes)?;
                files.push(DumpFile {
                    label: format!("Subagent {agent_id}"),
                    path: sub_path,
                    file_name: sub_name,
                });
            }
        }

        Ok(files)
    }

    fn dump_result(&self, id: &str, dir: &Path) -> Result<DumpFile> {
        let Some(mut markdown) = latest_main_conclusion(&self.session, &self.replay) else {
            bail!("No main agent conclusion available to dump.");
        };
        if !markdown.ends_with('\n') {
            markdown.push('\n');
        }
        let file_name = format!("pp-{id}-result.md");
        let path = write_dump_file(dir, &file_name, markdown.as_bytes())?;
        Ok(DumpFile {
            label: "Main conclusion Markdown".into(),
            path,
            file_name,
        })
    }

    fn build_main_transcript_jsonl(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        if let Some(path) = self.journal.active_transcript_path()? {
            let file = File::open(&path)
                .with_context(|| format!("opening session transcript {}", path.display()))?;
            let mut reader = SessionReader::new(BufReader::new(file));
            let mut line = Vec::new();
            while reader.read_record(&mut line)? != 0 {
                if !line.ends_with(b"\n") {
                    break;
                }
                out.extend_from_slice(&line);
            }
        } else {
            push_json_line(
                &mut out,
                &json!({
                    "type": "session",
                    "id": Value::Null,
                    "cwd": self.tools.cwd(),
                    "model_reference": self.current_model.reference,
                    "think_level": self.think_level,
                    "mode": self.workflow.mode_name(),
                }),
            )?;
        }
        Ok(out)
    }
}

fn push_json_line(out: &mut Vec<u8>, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *out, value).context("serializing JSONL record")?;
    out.push(b'\n');
    Ok(())
}

fn append_events_jsonl(path: &Path, out: &mut Vec<u8>) -> Result<()> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("opening {}", path.display())),
    };
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .with_context(|| format!("reading {}", path.display()))?;
        if read == 0 {
            break;
        }
        if !line.ends_with('\n') {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            push_json_line(out, &value)?;
        }
    }
    Ok(())
}

fn latest_main_conclusion(session: &Session, replay: &[ReplayRecord]) -> Option<String> {
    let from_session = match session {
        Session::Completion(request) => request.messages.iter().rev().find_map(|message| {
            let ChatCompletionRequestMessage::Assistant(assistant) = message else {
                return None;
            };
            let text = match assistant.content.as_ref()? {
                ChatCompletionRequestAssistantMessageContent::Text(text) => text.clone(),
                ChatCompletionRequestAssistantMessageContent::Array(parts) => parts
                    .iter()
                    .filter_map(|part| match part {
                        ChatCompletionRequestAssistantMessageContentPart::Text(part) => {
                            Some(part.text.as_str())
                        }
                        ChatCompletionRequestAssistantMessageContentPart::Refusal(_) => None,
                    })
                    .collect::<String>(),
            };
            (!text.trim().is_empty()).then_some(text)
        }),
        Session::Response(request) => {
            let InputParam::Items(items) = &request.input else {
                return None;
            };
            items.iter().rev().find_map(|item| {
                let InputItem::Item(Item::Message(MessageItem::Output(message))) = item else {
                    return None;
                };
                let text = message
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        OutputMessageContent::OutputText(part) => Some(part.text.as_str()),
                        OutputMessageContent::Refusal(_) => None,
                    })
                    .collect::<String>();
                (!text.trim().is_empty()).then_some(text)
            })
        }
    };
    if from_session.is_some() {
        return from_session;
    }
    replay.iter().rev().find_map(|record| match record {
        ReplayRecord::Event(AgentEvent::AssistantFinished { text }) if !text.trim().is_empty() => {
            Some(text.clone())
        }
        _ => None,
    })
}

fn write_dump_file(dir: &Path, file_name: &str, bytes: &[u8]) -> Result<PathBuf> {
    fs::create_dir_all(dir)
        .with_context(|| format!("creating dump directory {}", dir.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating temp dump file in {}", dir.display()))?;
    tmp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .context("setting dump file permissions")?;
    tmp.write_all(bytes)
        .with_context(|| format!("writing dump file {file_name}"))?;
    tmp.flush()
        .with_context(|| format!("flushing dump file {file_name}"))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing dump file {file_name}"))?;
    let target = dir.join(file_name);
    tmp.persist(&target)
        .map_err(|error| error.error)
        .with_context(|| format!("persisting dump file {}", target.display()))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::persistence::Metadata;
    use crate::subagent::{Spec, atomic_json};
    use pp_config::config::{EditMode, ModelCatalog};
    use pp_config::models::Protocol;
    use pp_core::model::ThinkLevel;
    use pp_tools::Tools;

    fn make_agent(protocol: Protocol) -> (tempfile::TempDir, Agent) {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let sessions = dir.path().join("sessions");
        let settings = dir.path().join("settings.yml");
        let models = dir.path().join("models.yml");
        fs::write(&settings, "rate_limit_retry_budget_seconds: 0\n").unwrap();
        let protocol_name = match protocol {
            Protocol::Completion => "completion",
            Protocol::Response => "response",
        };
        fs::write(
            &models,
            json!({
                "providers": {
                    "fixture": {
                        "api_key": "secret",
                        "protocol": protocol_name,
                        "base_url": "http://127.0.0.1:1/v1",
                        "models": {
                            "primary": {
                                "model": "primary-model"
                            }
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let catalog = ModelCatalog::from_files(&settings, &models).unwrap();
        let tools = Tools::new(workspace, EditMode::Replace).unwrap();
        let agent = Agent::new(catalog, tools, sessions).unwrap();
        (dir, agent)
    }

    fn append_turn(agent: &mut Agent, user: &str, assistant: &str) {
        agent.append_user(user);
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
                &agent.workflow,
                Some(user),
                None,
            )
            .unwrap();
        agent.journal.event(&AgentEvent::AssistantStarted);
        agent.journal.event(&AgentEvent::AssistantFinished {
            text: assistant.into(),
        });
        match &mut agent.session {
            Session::Completion(request) => request.messages.push(
                serde_json::from_value(json!({"role":"assistant","content":assistant})).unwrap(),
            ),
            Session::Response(request) => {
                crate::agent::turn::response_input(request).push(
                    serde_json::from_value(json!({
                        "type":"message",
                        "id":"msg-1",
                        "role":"assistant",
                        "status":"completed",
                        "content":[{"type":"output_text","text":assistant,"annotations":[]}]
                    }))
                    .unwrap(),
                );
            }
        }
        agent.journal.commit((&agent.session).into()).unwrap();
    }

    #[test]
    fn dump_all_writes_expanded_main_jsonl_request_json_and_subagent_files() {
        for protocol in [Protocol::Response, Protocol::Completion] {
            let (_dir, mut agent) = make_agent(protocol);
            append_turn(&mut agent, "Analyze repo", "## Summary\n\nAll good.");
            let session_id = agent.session_id().unwrap().to_owned();

            // 构造两个子 Agent（registry + session.json + events.jsonl）
            let sub_dir = agent
                .journal
                .directory()
                .join(format!("{session_id}.subagents"));
            fs::create_dir_all(&sub_dir).unwrap();
            let spec = Spec {
                model_role: None,
                role_route: None,
                definition: "explorer".into(),
                prompt: "Explore".into(),
                context: String::new(),
                model: agent.current_model.reference.clone(),
                thinking: ThinkLevel::None,
                tools: vec!["read".into()],
                spawns: vec![],
                output_schema: None,
                cwd: agent.tools.cwd().to_path_buf(),
                depth: 1,
                readonly: true,
                isolated: false,
                cost: None,
            };
            atomic_json(
                &sub_dir.join("registry.json"),
                &json!({
                    "version": 1,
                    "agents": {
                        "a1": {
                            "id": "a1",
                            "name": "explorer-1",
                            "parent": "Main",
                            "generation": 1,
                            "status": "idle",
                            "spec": spec,
                            "session_file": sub_dir.join("a1.session.json"),
                            "updated_ms": 1
                        },
                        "a2": {
                            "id": "a2",
                            "name": "explorer-2",
                            "parent": "Main",
                            "generation": 1,
                            "status": "idle",
                            "spec": spec,
                            "session_file": null,
                            "updated_ms": 2
                        }
                    },
                    "runs": {
                        "r1": {
                            "id": "r1",
                            "agent_id": "a1",
                            "owner": "Main",
                            "status": "completed",
                            "task": "check files",
                            "result": {"ok": true, "output": "found"},
                            "consumed": true,
                            "duration_ms": 12,
                            "started_ms": 1,
                            "requests": 1,
                            "tokens": 100,
                            "cost": null
                        }
                    },
                    "next_id": 2
                }),
            )
            .unwrap();
            atomic_json(
                &sub_dir.join("a1.session.json"),
                &json!({"version": 2, "model": agent.current_model.reference, "thinking": "none", "session": {"protocol": "response", "request": {"model": "m", "input": []}}}),
            )
            .unwrap();
            fs::write(
                sub_dir.join("a1.events.jsonl"),
                "{\"agent_id\":\"a1\",\"generation\":1,\"sequence\":1,\"event\":{\"assistant_started\":null}}\n",
            )
            .unwrap();

            let dump_dir = tempfile::tempdir().unwrap();
            let output = agent.dump_to(DumpMode::All, dump_dir.path()).unwrap();
            assert_eq!(output.files.len(), 4);

            let main_path = dump_dir.path().join(format!("pp-{session_id}-main.jsonl"));
            let req_path = dump_dir
                .path()
                .join(format!("pp-{session_id}-request.json"));
            let a1_path = dump_dir
                .path()
                .join(format!("pp-{session_id}-subagent-a1.jsonl"));
            let a2_path = dump_dir
                .path()
                .join(format!("pp-{session_id}-subagent-a2.jsonl"));

            for path in [&main_path, &req_path, &a1_path, &a2_path] {
                assert!(path.exists(), "missing dump file {}", path.display());
            }

            let main_lines: Vec<Value> = fs::read_to_string(&main_path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(main_lines[0]["type"], "session");
            assert!(main_lines.iter().all(|v| v.get("values").is_none()));
            assert!(
                main_lines
                    .iter()
                    .any(|v| v["customType"] == "pp.session_state")
            );

            let req_json: Value =
                serde_json::from_str(&fs::read_to_string(&req_path).unwrap()).unwrap();
            assert_eq!(
                req_json["protocol"],
                match protocol {
                    Protocol::Completion => "completion",
                    Protocol::Response => "response",
                }
            );
            assert!(req_json.get("request").is_some());

            let a1_lines: Vec<Value> = fs::read_to_string(&a1_path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(a1_lines[0]["type"], "subagent");
            assert_eq!(a1_lines[0]["runs"].as_array().unwrap().len(), 1);
            assert_eq!(a1_lines[1]["type"], "llm_request");
            assert_eq!(a1_lines[2]["agent_id"], "a1");

            let a2_lines: Vec<Value> = fs::read_to_string(&a2_path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(a2_lines.len(), 1);
            assert_eq!(a2_lines[0]["type"], "subagent");

            let summary = output.summary();
            for file in &output.files {
                assert!(summary.contains(&file.file_name), "{summary}");
                assert!(
                    summary.contains(&file.path.display().to_string()),
                    "{summary}"
                );
            }
        }
    }

    #[test]
    fn dump_result_writes_latest_main_conclusion_markdown_or_errors_when_empty() {
        let (_dir, mut agent) = make_agent(Protocol::Response);
        let dump_dir = tempfile::tempdir().unwrap();
        let error = agent
            .dump_to(DumpMode::Result, dump_dir.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("No main agent conclusion available to dump"),
            "{error}"
        );

        append_turn(&mut agent, "First", "Initial draft");
        append_turn(
            &mut agent,
            "Second",
            "# Final Conclusion\n\n- Item 1\n- Item 2",
        );
        let session_id = agent.session_id().unwrap().to_owned();

        let output = agent.dump_to(DumpMode::Result, dump_dir.path()).unwrap();
        assert_eq!(output.files.len(), 1);
        let expected_name = format!("pp-{session_id}-result.md");
        assert_eq!(output.files[0].file_name, expected_name);
        assert_eq!(
            fs::read_to_string(&output.files[0].path).unwrap(),
            "# Final Conclusion\n\n- Item 1\n- Item 2\n"
        );
        assert!(output.summary().contains(&expected_name));
        assert!(
            output
                .summary()
                .contains(&output.files[0].path.display().to_string())
        );
    }

    #[test]
    fn unsaved_session_dump_all_writes_metadata_and_request_json() {
        let (_dir, mut agent) = make_agent(Protocol::Completion);
        let dump_dir = tempfile::tempdir().unwrap();
        let output = agent.dump_to(DumpMode::All, dump_dir.path()).unwrap();
        assert_eq!(output.files.len(), 2);
        assert!(output.files[0].file_name.starts_with("pp-unsaved-"));
        assert!(output.files[0].file_name.ends_with("-main.jsonl"));
        assert!(output.files[1].file_name.ends_with("-request.json"));
        let line: Value =
            serde_json::from_str(fs::read_to_string(&output.files[0].path).unwrap().trim())
                .unwrap();
        assert_eq!(line["type"], "session");
    }
}
