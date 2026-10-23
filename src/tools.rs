use crate::cancellation::{CancellationToken, Cancelled, CleanupError};
use crate::config::{EditMode, ToolLimits};
use crate::events::{AgentEvent, OutputStream};
use anyhow::{Context, Result, ensure};
use async_openai::types::chat::FunctionObject;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    sync::mpsc::SyncSender,
    thread,
};

mod bash;
pub(crate) use bash::BackgroundCleanup;
mod diff;
mod hashline;
mod read;
mod replace;
mod save;
mod search;

#[cfg(test)]
use std::{fs, sync::mpsc::sync_channel};

const OUTPUT_LIMIT: usize = 65_536;

pub(crate) struct Tools {
    pub(crate) ask_handler: Option<crate::interaction::AskHandler>,
    pub(crate) mcp: crate::mcp::Registry,
    pub(crate) skills: crate::skills::SharedSkills,
    cwd: PathBuf,
    edit_mode: EditMode,
    workspace: save::Workspace,
    search: search::SearchTools,
    background_tasks: bash::TaskManager,
    pub(crate) limits: ToolLimits,
}

/// Separates a tool's serialized result from a process cleanup failure that
/// must stop the agent rather than be treated as an ordinary command failure.
pub(crate) struct ToolExecution {
    pub(crate) output: String,
    pub(crate) stop_error: Option<anyhow::Error>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default = "first_line")]
    offset: usize,
    #[serde(default, deserialize_with = "read_limit")]
    limit: Option<usize>,
}
fn first_line() -> usize {
    1
}
fn read_limit<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<usize>, D::Error> {
    usize::deserialize(deserializer).map(Some)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HashlineEditArgs {
    path: String,
    edits: Vec<hashline::Edit>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceEditArgs {
    path: String,
    edits: Vec<replace::Edit>,
}
enum EditBatch {
    Hashline(Vec<hashline::Edit>),
    Replace(Vec<replace::Edit>),
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArgs {
    path: String,
    content: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashArgs {
    command: String,
    #[serde(default)]
    mode: BashMode,
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BashMode {
    #[default]
    Foreground,
    Background,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashTaskArgs {
    task_id: String,
    action: BashTaskAction,
}
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum BashTaskAction {
    Status,
    Stop,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    args: Vec<String>,
}

impl Tools {
    pub(crate) fn new(cwd: PathBuf, edit_mode: EditMode) -> Result<Self> {
        let workspace = save::Workspace::open(&cwd)?;
        let path = std::env::var_os("PATH");
        let search = search::SearchTools::detect(&workspace.path, path.as_deref());
        Ok(Self {
            ask_handler: None,
            mcp: Default::default(),
            skills: crate::skills::Skills::default().shared(),
            cwd: workspace.path.clone(),
            edit_mode,
            workspace,
            search,
            background_tasks: Default::default(),
            limits: ToolLimits::default(),
        })
    }
    pub(crate) fn stop_background_tasks(&self) -> Result<()> {
        self.background_tasks.stop_all()
    }

    pub(crate) fn background_cleanup(&self) -> BackgroundCleanup {
        self.background_tasks.cleanup()
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.cwd
    }
    pub(crate) fn edit_mode(&self) -> EditMode {
        self.edit_mode
    }

    pub(crate) fn all_definitions(&self) -> Vec<FunctionObject> {
        let mut definitions = Self::definitions(self.edit_mode);
        if self.ask_handler.is_some() {
            definitions.push(crate::interaction::definition());
        }
        definitions.extend(self.search.definitions());
        definitions.extend(self.mcp.definitions());
        definitions
    }

    pub(crate) fn definitions(edit_mode: EditMode) -> Vec<FunctionObject> {
        let (read_description, edit) = match edit_mode {
            EditMode::Hashline => (
                "Read complete lines within configured read_max_bytes/read_max_lines limits as N:hash|text (lossy UTF-8). offset is a 1-based source line, limit is the requested line count. Use next_offset to continue; anchors retain real file line numbers. A single oversized line returns an error. Paths are relative to startup cwd unless absolute.".into(),
                FunctionObject {
                    name: "edit".into(),
                    description: Some(r#"Atomically edit an existing UTF-8 file inside startup cwd (no symlink paths). Validate every edit before saving; failure before commit leaves the target unchanged. Use read's exact N:hash anchors. Operations: replace (inclusive start/end; lines:[] deletes), insert_before, insert_after. All anchors refer to the original snapshot; edits must not overlap or share insertion gaps, and insertions must not touch replacement boundaries. Each lines string is literal text without CR/LF or display prefixes. On stale anchors, read again. Example: {"path":"demo.txt","edits":[{"op":"replace","start":"1:a430d84680aabd0b","end":"1:a430d84680aabd0b","lines":["edited"]}]}"#.into()),
                    parameters: Some(json!({
                        "type":"object",
                        "properties":{
                            "path":{"type":"string"},
                            "edits":{"type":"array","minItems":1,"items":{"anyOf":[
                                {"type":"object","properties":{
                                    "op":{"type":"string","enum":["replace"]},
                                    "start":{"type":"string"},"end":{"type":"string"},
                                    "lines":{"type":"array","items":{"type":"string"}}
                                },"required":["op","start","end","lines"],"additionalProperties":false},
                                {"type":"object","properties":{
                                    "op":{"type":"string","enum":["insert_before"]},
                                    "anchor":{"type":"string"},
                                    "lines":{"type":"array","minItems":1,"items":{"type":"string"}}
                                },"required":["op","anchor","lines"],"additionalProperties":false},
                                {"type":"object","properties":{
                                    "op":{"type":"string","enum":["insert_after"]},
                                    "anchor":{"type":"string"},
                                    "lines":{"type":"array","minItems":1,"items":{"type":"string"}}
                                },"required":["op","anchor","lines"],"additionalProperties":false}
                            ]}}
                        },"required":["path","edits"],"additionalProperties":false
                    })),
                    ..Default::default()
                },
            ),
            EditMode::Replace => (
                "Read complete lines within configured read_max_bytes/read_max_lines limits as plain text (lossy UTF-8), preserving newlines without prefixes. offset is a 1-based source line, limit is the requested line count. Use next_offset to continue. A single oversized line returns an error. Copy text exactly for edit oldText. Paths are relative to startup cwd unless absolute.".into(),
                FunctionObject {
                    name: "edit".into(),
                    description: Some(r#"Atomically edit an existing UTF-8 file inside startup cwd with exact text replacements. Symlink paths are forbidden; all items are validated before saving and failure before commit leaves the target unchanged. Each item's oldText is matched against the original file content from read; oldText must be unique in the file and items must not overlap, so include surrounding context such as whole lines. Insert text by including a unique neighboring line in oldText and repeating it in newText. If an item cannot be found, read the file again. Example: {"path":"demo.txt","edits":[{"oldText":"old line","newText":"new line"}]}"#.into()),
                    parameters: Some(json!({
                        "type":"object",
                        "properties":{
                            "path":{"type":"string"},
                            "edits":{"type":"array","minItems":1,"items":{"type":"object","properties":{
                                "oldText":{"type":"string","minLength":1},
                                "newText":{"type":"string"}
                            },"required":["oldText","newText"],"additionalProperties":false}}
                        },"required":["path","edits"],"additionalProperties":false
                    })),
                    ..Default::default()
                },
            ),
        };
        vec![
            FunctionObject {
                name: "load_skill".into(),
                description: Some("Load the full instructions of an enabled skill by its exact name from the available skills list. Resolve relative resource paths against the returned base_dir.".into()),
                parameters: Some(json!({"type":"object","properties":{"name":{"type":"string"}},"required":["name"],"additionalProperties":false})),
                ..Default::default()
            },
            FunctionObject {
                name: "read".into(),
                description: Some(read_description),
                parameters: Some(json!({"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false})),
                ..Default::default()
            },
            edit,
            FunctionObject {
                name: "write".into(),
                description: Some("Atomically create or fully overwrite a UTF-8 file inside startup cwd, creating missing parent directories. Symlink paths are forbidden. Failure before commit leaves the target unchanged. Use edit for local changes to existing files.".into()),
                parameters: Some(json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"],"additionalProperties":false})),
                ..Default::default()
            },
            FunctionObject {
                name: "bash".into(),
                description: Some("Run bash in startup cwd. Default foreground mode uses configured bash_timeout_seconds. Background mode returns a managed task_id, survives tool calls without a fixed deadline, and is stopped at the end of this user task. Running does not mean ready: use subsequent requests to verify the service, bash_task status for cumulative logs, and stop after verification. Process groups are cleaned up; shell side effects are not rolled back, and deliberately detached descendants are not covered. No filesystem sandbox. stdout/stderr retain at most 500 lines combined and 65536 bytes each.".into()),
                parameters: Some(json!({"type":"object","properties":{"command":{"type":"string"},"mode":{"type":"string","enum":["foreground","background"]}},"required":["command"],"additionalProperties":false})),
                ..Default::default()
            },
            FunctionObject {
                name: "bash_task".into(),
                description: Some("Inspect cumulative output and current status of a managed background task, or stop it and wait for process and temporary-directory cleanup. Only task IDs from this user task are valid. Stop is idempotent; it does not cancel the user task.".into()),
                parameters: Some(json!({"type":"object","properties":{"task_id":{"type":"string","minLength":1},"action":{"type":"string","enum":["status","stop"]}},"required":["task_id","action"],"additionalProperties":false})),
                ..Default::default()
            },
        ]
    }

    pub(crate) fn execute(
        &self,
        name: &str,
        arguments: &str,
        call_id: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> ToolExecution {
        let started = std::time::Instant::now();
        let pid = std::process::id();
        let tool = match name {
            "ask" => "ask",
            "load_skill" => "load_skill",
            "read" => "read",
            "edit" => "edit",
            "write" => "write",
            "bash" => "bash",
            "bash_task" => "bash_task",
            "grep" => "grep",
            "find" => "find",
            _ => "external",
        };
        tracing::info!(event = "tool_started", pid, tool);
        emit(AgentEvent::ToolStarted {
            id: call_id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_owned(),
        });
        // The cancellation check runs after ToolStarted and before any argument
        // parsing, file access or process creation.
        let mut stop_error = None;
        let value = if cancellation.is_cancelled() {
            json!({"ok":false,"cancelled":true,"error":"Not executed: task cancelled."})
        } else {
            match self.execute_inner(name, arguments, call_id, cancellation, emit) {
                Ok(result) => result,
                Err(error) => {
                    let cleanup_failure = error.chain().any(|cause| cause.is::<CleanupError>());
                    let cancelled = error.chain().any(|cause| cause.is::<Cancelled>());
                    let message = format!("{error:#}");
                    if cleanup_failure {
                        // A tool that could not be stopped is a real failure;
                        // keep the typed error so the agent can report it.
                        stop_error = Some(error);
                        json!({"ok":false,"error":message})
                    } else if cancelled {
                        json!({"ok":false,"cancelled":true,"error":"Tool cancelled; side effects may have occurred."})
                    } else {
                        json!({"ok":false,"error":message})
                    }
                }
            }
        };
        let status = if value.get("cancelled").and_then(Value::as_bool) == Some(true) {
            "cancelled"
        } else if value.get("ok").and_then(Value::as_bool) == Some(true) {
            "ok"
        } else {
            "error"
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        if status == "error" {
            tracing::warn!(event = "tool_finished", pid, tool, status, elapsed_ms);
        } else {
            tracing::info!(event = "tool_finished", pid, tool, status, elapsed_ms);
        }
        let serialized = value.to_string();
        emit(AgentEvent::ToolFinished {
            id: call_id.to_owned(),
            result: value,
        });
        ToolExecution {
            output: serialized,
            stop_error,
        }
    }

    fn execute_inner(
        &self,
        name: &str,
        arguments: &str,
        call_id: &str,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Value> {
        match name {
            "ask" => {
                let handler = self
                    .ask_handler
                    .as_ref()
                    .context("ask requires interactive TUI mode")?;
                let args = std::sync::Arc::new(crate::interaction::AskArgs::parse(arguments)?);
                let answers = handler(
                    crate::interaction::AskRequest {
                        call_id: call_id.to_owned(),
                        args: args.clone(),
                    },
                    cancellation,
                )?;
                crate::cancellation::check(cancellation)?;
                args.validate_answers(&answers)?;
                Ok(crate::interaction::answer_result(
                    args.canonicalize_answers(answers),
                ))
            }
            "load_skill" => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Args {
                    name: String,
                }
                let args: Args =
                    serde_json::from_str(arguments).context("invalid load_skill arguments")?;
                self.skills
                    .lock()
                    .map_err(|_| anyhow::anyhow!("skill registry unavailable"))?
                    .load(&args.name)
            }
            "read" => {
                let args: ReadArgs =
                    serde_json::from_str(arguments).context("invalid read arguments")?;
                ensure!(args.offset > 0, "read offset must be greater than zero");
                let limit = args.limit.unwrap_or(self.limits.read_max_lines);
                ensure!(
                    limit > 0 && limit <= self.limits.read_max_lines,
                    "read limit must be between 1 and {}",
                    self.limits.read_max_lines
                );
                let path = self.path(&args.path)?;
                let page = read::read(&path, args.offset, limit, self.limits.read_max_bytes)?;
                let content = match self.edit_mode {
                    EditMode::Hashline => hashline::render_page(&page.bytes, args.offset),
                    EditMode::Replace => String::from_utf8_lossy(&page.bytes).into_owned(),
                };
                Ok(
                    json!({"ok":true,"content":content,"truncated":page.next_offset.is_some(),"lines_read":page.lines_read,"next_offset":page.next_offset}),
                )
            }
            "edit" => {
                let (path, edits) = match self.edit_mode {
                    EditMode::Hashline => {
                        let args: HashlineEditArgs =
                            serde_json::from_str(arguments).context("invalid edit arguments")?;
                        (args.path, EditBatch::Hashline(args.edits))
                    }
                    EditMode::Replace => {
                        let args: ReplaceEditArgs =
                            serde_json::from_str(arguments).context("invalid edit arguments")?;
                        (args.path, EditBatch::Replace(args.edits))
                    }
                };
                ensure!(!path.trim().is_empty(), "path must not be blank");
                let target = self.workspace.target(Path::new(&path), false)?;
                let bytes = target.read()?;
                let source = std::str::from_utf8(&bytes).context("edit requires a UTF-8 file")?;
                let (count, output) = match edits {
                    EditBatch::Hashline(edits) => (edits.len(), hashline::apply(source, &edits)?),
                    EditBatch::Replace(edits) => (edits.len(), replace::apply(source, &edits)?),
                };
                let warning = target.save(output.as_bytes())?;
                emit(AgentEvent::FileChanged {
                    id: call_id.to_owned(),
                    path,
                    diff: diff::build(Some(source), &output),
                });
                let mut result =
                    json!({"ok":true,"edits_applied":count,"bytes_written":output.len()});
                if let Some(warning) = warning {
                    result["warning"] = json!(warning);
                }
                Ok(result)
            }
            "write" => {
                let args: WriteArgs =
                    serde_json::from_str(arguments).context("invalid write arguments")?;
                ensure!(!args.path.trim().is_empty(), "path must not be blank");
                let target = self.workspace.target(Path::new(&args.path), true)?;
                let before = target.read_optional();
                let warning = target.save(args.content.as_bytes())?;
                let diff = match before {
                    Ok(None) => diff::build(None, &args.content),
                    Ok(Some(bytes)) => match std::str::from_utf8(&bytes) {
                        Ok(source) => diff::build(Some(source), &args.content),
                        Err(_) => crate::events::FileDiff::Unavailable {
                            reason: "Previous content is not UTF-8; text diff unavailable.".into(),
                        },
                    },
                    Err(_) => crate::events::FileDiff::Unavailable {
                        reason: "Previous content could not be read; text diff unavailable.".into(),
                    },
                };
                emit(AgentEvent::FileChanged {
                    id: call_id.to_owned(),
                    path: args.path,
                    diff,
                });
                let mut result = json!({"ok":true,"bytes_written":args.content.len()});
                if let Some(warning) = warning {
                    result["warning"] = json!(warning);
                }
                Ok(result)
            }
            "bash" => {
                let args: BashArgs =
                    serde_json::from_str(arguments).context("invalid bash arguments")?;
                ensure!(!args.command.trim().is_empty(), "command must not be blank");
                let cache = self.workspace.cache_dir()?;
                match args.mode {
                    BashMode::Foreground => bash::run(
                        &self.cwd,
                        &cache,
                        &args.command,
                        self.limits.bash_timeout_seconds,
                        call_id,
                        cancellation,
                        emit,
                    ),
                    BashMode::Background => {
                        let snapshot = self.background_tasks.start(
                            &self.cwd,
                            &cache,
                            &args.command,
                            cancellation,
                        )?;
                        emit_background_snapshot(call_id, &snapshot, emit);
                        Ok(snapshot)
                    }
                }
            }
            "bash_task" => {
                let args: BashTaskArgs =
                    serde_json::from_str(arguments).context("invalid bash_task arguments")?;
                ensure!(!args.task_id.trim().is_empty(), "task_id must not be blank");
                let snapshot = match args.action {
                    BashTaskAction::Status => self.background_tasks.status(&args.task_id)?,
                    BashTaskAction::Stop => self.background_tasks.stop(&args.task_id)?,
                };
                emit_background_snapshot(call_id, &snapshot, emit);
                Ok(snapshot)
            }
            "grep" | "find" => {
                let args: SearchArgs = serde_json::from_str(arguments)
                    .with_context(|| format!("invalid {name} arguments"))?;
                ensure!(
                    !args.args.iter().any(|arg| arg.contains('\0')),
                    "{name} args must not contain NUL bytes"
                );
                let mut command = self.search.command(name)?;
                command.args(&args.args);
                let backend = command.get_program().to_owned();
                let cache = self.workspace.cache_dir()?;
                let mut result = bash::run_command(
                    &self.cwd,
                    &cache,
                    command,
                    self.limits.bash_timeout_seconds,
                    call_id,
                    cancellation,
                    emit,
                )
                .with_context(|| format!("running {}", backend.to_string_lossy()))?;
                if name == "grep"
                    && result["exit_code"] == 1
                    && result["timed_out"] == false
                    && result["cancelled"] == false
                {
                    result["ok"] = json!(true);
                }
                Ok(result)
            }
            _ => self.mcp.call(name, arguments, cancellation),
        }
    }

    fn path(&self, path: &str) -> Result<PathBuf> {
        ensure!(!path.trim().is_empty(), "path must not be blank");
        // PathBuf::join preserves an absolute path. cwd is not a sandbox.
        Ok(self.cwd.join(path))
    }
}

fn emit_background_snapshot(call_id: &str, snapshot: &Value, emit: &mut dyn FnMut(AgentEvent)) {
    for (key, stream) in [
        ("stdout", OutputStream::Stdout),
        ("stderr", OutputStream::Stderr),
    ] {
        if let Some(text) = snapshot[key].as_str() {
            emit_output(call_id, stream, text.to_owned(), emit);
        }
    }
}

const PIPE_CHUNK_SIZE: usize = 4096;

fn drain_pipe(
    mut reader: impl Read,
    stream: OutputStream,
    sender: SyncSender<(OutputStream, Vec<u8>)>,
    stop: &std::sync::atomic::AtomicBool,
) -> io::Result<bool> {
    let mut buffer = [0; PIPE_CHUNK_SIZE];
    let mut retained = 0;
    let mut truncated = false;
    loop {
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            return Ok(truncated);
        }
        let count = match reader.read(&mut buffer) {
            Ok(0) => return Ok(truncated),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            Err(error) => return Err(error),
        };
        let keep = count.min(OUTPUT_LIMIT - retained);
        if keep > 0 {
            // After the cap, keep draining without allocating or sending chunks.
            if sender.send((stream, buffer[..keep].to_vec())).is_err() {
                return Ok(truncated);
            }
            retained += keep;
        }
        truncated |= count > keep;
    }
}

fn emit_output(
    call_id: &str,
    stream: OutputStream,
    text: String,
    emit: &mut dyn FnMut(AgentEvent),
) {
    if !text.is_empty() {
        emit(AgentEvent::ToolOutput {
            id: call_id.to_owned(),
            stream,
            text,
        });
    }
}

#[derive(Default)]
struct StreamText {
    text: String,
    tail: [u8; 4],
    tail_len: usize,
}

impl StreamText {
    fn append(&mut self, mut bytes: &[u8]) -> String {
        let mut delta = String::with_capacity(bytes.len() + self.tail_len);
        while self.tail_len > 0 && !bytes.is_empty() {
            let count = (self.tail.len() - self.tail_len).min(bytes.len());
            self.tail[self.tail_len..self.tail_len + count].copy_from_slice(&bytes[..count]);
            self.tail_len += count;
            bytes = &bytes[count..];
            let used = decode_prefix(&self.tail[..self.tail_len], &mut delta);
            self.tail.copy_within(used..self.tail_len, 0);
            self.tail_len -= used;
        }
        if self.tail_len == 0 {
            let used = decode_prefix(bytes, &mut delta);
            let tail = &bytes[used..];
            self.tail[..tail.len()].copy_from_slice(tail);
            self.tail_len = tail.len();
        }
        self.text.push_str(&delta);
        delta
    }

    fn finish(&mut self) -> String {
        let delta = String::from_utf8_lossy(&self.tail[..self.tail_len]).into_owned();
        self.tail_len = 0;
        self.text.push_str(&delta);
        delta
    }
}

// Decode only complete code points/errors. An incomplete suffix needs the next
// read (or EOF), otherwise split multibyte characters would become replacements.
fn decode_prefix(mut bytes: &[u8], output: &mut String) -> usize {
    let total = bytes.len();
    loop {
        match std::str::from_utf8(bytes) {
            Ok(text) => {
                output.push_str(text);
                return total;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                output.push_str(
                    std::str::from_utf8(&bytes[..valid]).expect("validated UTF-8 prefix"),
                );
                bytes = &bytes[valid..];
                match error.error_len() {
                    Some(count) => {
                        output.push(char::REPLACEMENT_CHARACTER);
                        bytes = &bytes[count..];
                    }
                    None => return total - bytes.len(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn execute(tools: &Tools, name: &str, args: Value) -> Value {
        let token = CancellationToken::new();
        let execution = tools.execute(name, &args.to_string(), "test-call", &token, &mut |_| {});
        assert!(
            execution.stop_error.is_none(),
            "unexpected cleanup failure: {:?}",
            execution.stop_error
        );
        serde_json::from_str(&execution.output).unwrap()
    }
    fn fallback_search_tools(dir: &Path) -> Tools {
        use std::os::unix::fs::symlink;
        let bin = dir.join("bin");
        fs::create_dir(&bin).unwrap();
        for name in ["grep", "find"] {
            let executable = ["/usr/bin", "/bin"]
                .into_iter()
                .map(|base| Path::new(base).join(name))
                .find(|path| path.is_file())
                .expect("system grep/find available");
            symlink(executable, bin.join(name)).unwrap();
        }
        let mut tools = Tools::new(dir.into(), EditMode::Replace).unwrap();
        tools.search = search::SearchTools::detect(tools.cwd(), Some(bin.as_os_str()));
        tools
    }

    #[test]
    fn search_tools_fallback_native_results_and_literal_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let tools = fallback_search_tools(dir.path());
        fs::create_dir(dir.path().join("space dir")).unwrap();
        fs::write(
            dir.path().join("space dir/demo.txt"),
            "needle\nother\nneedle\n",
        )
        .unwrap();
        let matched = execute(
            &tools,
            "grep",
            json!({"args":["-n","-e","needle","--","space dir/demo.txt"]}),
        );
        assert_eq!(matched["ok"], true, "{matched}");
        assert_eq!(matched["exit_code"], 0);
        assert_eq!(matched["stdout"], "1:needle\n3:needle\n");
        let absent = execute(
            &tools,
            "grep",
            json!({"args":["-e","absent","space dir/demo.txt"]}),
        );
        assert_eq!(absent["ok"], true, "{absent}");
        assert_eq!(absent["exit_code"], 1);
        assert_eq!(absent["stdout"], "");
        let invalid = execute(
            &tools,
            "grep",
            json!({"args":["-e","[","space dir/demo.txt"]}),
        );
        assert_eq!(invalid["ok"], false, "{invalid}");
        assert_ne!(invalid["exit_code"], 0);
        assert_ne!(invalid["exit_code"], 1);
        assert!(!invalid["stderr"].as_str().unwrap().is_empty());
        let found = execute(
            &tools,
            "find",
            json!({"args":["space dir","-type","f","-name","*.txt","-print"]}),
        );
        assert_eq!(found["ok"], true, "{found}");
        assert_eq!(found["stdout"], "space dir/demo.txt\n");
        let missing = execute(&tools, "find", json!({"args":["missing","-print"]}));
        assert_eq!(missing["ok"], false, "{missing}");
        assert!(!missing["stderr"].as_str().unwrap().is_empty());
        for pattern in ["$(touch INJECTED)", "needle; touch INJECTED"] {
            let result = execute(
                &tools,
                "grep",
                json!({"args":["-F","-e",pattern,"space dir/demo.txt"]}),
            );
            assert_eq!(result["exit_code"], 1, "{result}");
            assert_eq!(result["ok"], true);
            assert!(!dir.path().join("INJECTED").exists());
        }
        let empty = execute(
            &tools,
            "grep",
            json!({"args":["-e","","space dir/demo.txt"]}),
        );
        assert_eq!(empty["stdout"], "needle\nother\nneedle\n");
        let stdin = execute(&tools, "grep", json!({"args":[]}));
        assert_eq!(stdin["ok"], false, "{stdin}");
    }

    #[test]
    fn search_tools_reject_invalid_args_and_missing_backends_without_breaking_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = fallback_search_tools(dir.path());
        for name in ["grep", "find"] {
            for args in [
                json!({}),
                json!({"args":null}),
                json!({"args":[1]}),
                json!({"args":[],"extra":true}),
                json!({"args":["\u{0}"]}),
            ] {
                let result = execute(&tools, name, args);
                assert_eq!(result["ok"], false, "{name}: {result}");
                assert!(result["error"].as_str().unwrap().contains(name));
            }
        }
        tools.search = search::SearchTools::detect(tools.cwd(), None);
        for name in ["grep", "find"] {
            let result = execute(&tools, name, json!({"args":[]}));
            assert_eq!(result["ok"], false);
            assert!(result["error"].as_str().unwrap().contains("on PATH"));
        }
        assert_eq!(
            execute(
                &tools,
                "write",
                json!({"path":"kept","content":"available"})
            )["ok"],
            true
        );
        assert_eq!(
            execute(&tools, "read", json!({"path":"kept"}))["content"],
            "available"
        );
    }

    #[test]
    fn search_tools_timeout_and_cancel_reap_descendants() {
        use std::os::unix::fs::PermissionsExt;
        for cancelled in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let fixture = dir.path().join("rg");
            fs::write(&fixture, "#!/bin/bash\ntrap 'exit 1' TERM\nprintf 'search-ready\\n'\nprintf '%s' $$ > parent.pid\n/bin/sleep 30 & child=$!\nprintf '%s' $child > child.pid\nwait $child\nexit 1\n").unwrap();
            fs::set_permissions(&fixture, fs::Permissions::from_mode(0o755)).unwrap();
            let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
            tools.search = search::SearchTools::detect(tools.cwd(), Some(dir.path().as_os_str()));
            tools.limits.bash_timeout_seconds = if cancelled { 60 } else { 1 };
            let token = CancellationToken::new();
            let watcher = cancelled.then(|| {
                let cwd = dir.path().to_owned();
                let token = token.clone();
                thread::spawn(move || {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while !cwd.join("child.pid").exists() {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "fixture never started"
                        );
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    token.cancel();
                })
            });
            let execution =
                tools.execute("grep", "{\"args\":[]}", "search-stop", &token, &mut |_| {});
            assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
            let result: Value = serde_json::from_str(&execution.output).unwrap();
            assert_eq!(result["ok"], false, "{result}");
            assert_eq!(result["timed_out"], !cancelled, "{result}");
            assert_eq!(result["cancelled"], cancelled, "{result}");
            assert_eq!(result["stdout"], "search-ready\n");
            assert_eq!(result["exit_code"], 1, "{result}");
            if let Some(watcher) = watcher {
                watcher.join().unwrap();
            }
            for marker in ["parent.pid", "child.pid"] {
                let pid = fs::read_to_string(dir.path().join(marker))
                    .unwrap()
                    .parse()
                    .unwrap();
                wait_until_gone(pid, marker);
            }
            assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pp-tmp-")
            }));
        }
    }

    fn read_content(tools: &Tools, path: &str) -> (String, bool) {
        let read = execute(tools, "read", json!({"path": path}));
        assert_eq!(read["ok"], true, "{read}");
        (
            read["content"].as_str().unwrap().to_owned(),
            read["truncated"].as_bool().unwrap(),
        )
    }

    fn anchors(content: &str) -> Vec<String> {
        content
            .lines()
            .map(|line| line.split_once('|').unwrap().0.to_owned())
            .collect()
    }

    fn row_text(content: &str) -> Vec<&str> {
        content
            .lines()
            .map(|line| line.split_once('|').unwrap().1)
            .collect()
    }

    #[test]
    fn hashline_edit_rejects_invalid_edits_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
        let path = dir.path().join("demo.txt");
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let original = fs::read(&path).unwrap();
        let content = read_content(&tools, "demo.txt").0;
        assert_eq!(row_text(&content), ["one", "two", "three"]);
        let anchors = anchors(&content);
        let (two_number, digest) = anchors[1].split_once(':').unwrap();
        let upper = format!("{}:{}", two_number, digest.to_uppercase());
        let out_of_range = format!("{}:{}", anchors.len() + 6, digest);
        let wrong_hash = format!("{}:0000000000000001", two_number);
        let a1 = &anchors[0];
        let a2 = &anchors[1];
        let a3 = &anchors[2];

        let cases: Vec<(&str, Value)> = vec![
            (
                "anchor without separator",
                json!({"path":"demo.txt","edits":[{"op":"replace","start":"two","end":"two","lines":["x"]}]}),
            ),
            (
                "uppercase digest",
                json!({"path":"demo.txt","edits":[{"op":"replace","start":&upper,"end":a2,"lines":["x"]}]}),
            ),
            (
                "leading zero line number",
                json!({"path":"demo.txt","edits":[{"op":"replace","start":"02:0000000000000000","end":a2,"lines":["x"]}]}),
            ),
            (
                "zero line number",
                json!({"path":"demo.txt","edits":[{"op":"replace","start":"0:0000000000000000","end":a2,"lines":["x"]}]}),
            ),
            (
                "line number overflow",
                json!({"path":"demo.txt","edits":[{"op":"replace","start":"99999999999999999999:0000000000000000","end":a2,"lines":["x"]}]}),
            ),
            (
                "line number out of range",
                json!({"path":"demo.txt","edits":[{"op":"replace","start":&out_of_range,"end":&out_of_range,"lines":["x"]}]}),
            ),
            (
                "reversed range",
                json!({"path":"demo.txt","edits":[{"op":"replace","start":a3,"end":a2,"lines":["x"]}]}),
            ),
            (
                "hash mismatch",
                json!({"path":"demo.txt","edits":[{"op":"replace","start":&wrong_hash,"end":&wrong_hash,"lines":["x"]}]}),
            ),
            (
                "overlapping replacements",
                json!({"path":"demo.txt","edits":[
                    {"op":"replace","start":a1,"end":a2,"lines":["x"]},
                    {"op":"replace","start":a2,"end":a3,"lines":["y"]}
                ]}),
            ),
            (
                "insertion inside replaced range",
                json!({"path":"demo.txt","edits":[
                    {"op":"replace","start":a1,"end":a3,"lines":["x"]},
                    {"op":"insert_before","anchor":a2,"lines":["y"]}
                ]}),
            ),
            (
                "insertion at replace start boundary",
                json!({"path":"demo.txt","edits":[
                    {"op":"replace","start":a2,"end":a2,"lines":["x"]},
                    {"op":"insert_before","anchor":a2,"lines":["y"]}
                ]}),
            ),
            (
                "insertion at replace end boundary",
                json!({"path":"demo.txt","edits":[
                    {"op":"replace","start":a2,"end":a2,"lines":["x"]},
                    {"op":"insert_after","anchor":a2,"lines":["y"]}
                ]}),
            ),
            (
                "duplicate gap insertions",
                json!({"path":"demo.txt","edits":[
                    {"op":"insert_before","anchor":a1,"lines":["x"]},
                    {"op":"insert_before","anchor":a1,"lines":["y"]}
                ]}),
            ),
            (
                "same gap from both sides",
                json!({"path":"demo.txt","edits":[
                    {"op":"insert_after","anchor":a1,"lines":["x"]},
                    {"op":"insert_before","anchor":a2,"lines":["y"]}
                ]}),
            ),
            (
                "unknown op",
                json!({"path":"demo.txt","edits":[{"op":"delete","anchor":a1,"lines":["x"]}]}),
            ),
            (
                "extra top-level field",
                json!({"path":"demo.txt","edits":[{"op":"insert_before","anchor":a1,"lines":["x"]}],"reason":"nope"}),
            ),
            (
                "extra operation field",
                json!({"path":"demo.txt","edits":[{"op":"replace","start":a1,"end":a1,"lines":["x"],"why":"nope"}]}),
            ),
            (
                "carriage return inside lines",
                json!({"path":"demo.txt","edits":[{"op":"replace","start":a1,"end":a1,"lines":["a\rb"]}]}),
            ),
            (
                "line feed inside lines",
                json!({"path":"demo.txt","edits":[{"op":"insert_after","anchor":a1,"lines":["a\nb"]}]}),
            ),
            ("empty edits", json!({"path":"demo.txt","edits":[]})),
            (
                "empty insertion lines",
                json!({"path":"demo.txt","edits":[{"op":"insert_before","anchor":a1,"lines":[]}]}),
            ),
        ];
        for (name, args) in cases {
            let result = execute(&tools, "edit", args);
            assert_eq!(result["ok"], false, "{name}: {result}");
            assert_eq!(fs::read(&path).unwrap(), original, "{name}");
        }
    }

    #[test]
    fn hashline_edit_rejects_stale_anchor_in_batch_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
        let path = dir.path().join("demo.txt");
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let anchors = anchors(&read_content(&tools, "demo.txt").0);
        let stale = format!("{}:0000000000000001", anchors[1].split_once(':').unwrap().0);
        let result = execute(
            &tools,
            "edit",
            json!({"path":"demo.txt","edits":[
                {"op":"insert_before","anchor":&anchors[0],"lines":["zero"]},
                {"op":"replace","start":&stale,"end":&stale,"lines":["TWO"]}
            ]}),
        );
        assert_eq!(result["ok"], false, "{result}");
        assert_eq!(fs::read(&path).unwrap(), b"one\ntwo\nthree\n");
    }

    #[test]
    fn hashline_edit_rejects_missing_empty_directory_and_non_utf8_targets() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
        let edits = json!([{"op":"replace","start":"1:0000000000000000","end":"1:0000000000000000","lines":["x"]}]);

        let missing = execute(&tools, "edit", json!({"path":"absent.txt","edits":edits}));
        assert_eq!(missing["ok"], false, "{missing}");
        assert!(!dir.path().join("absent.txt").exists());

        fs::write(dir.path().join("empty.txt"), "").unwrap();
        let empty = execute(&tools, "edit", json!({"path":"empty.txt","edits":edits}));
        assert_eq!(empty["ok"], false, "{empty}");
        assert_eq!(fs::read(dir.path().join("empty.txt")).unwrap(), b"");

        fs::create_dir(dir.path().join("folder")).unwrap();
        let folder = execute(&tools, "edit", json!({"path":"folder","edits":edits}));
        assert_eq!(folder["ok"], false, "{folder}");

        fs::write(dir.path().join("bytes.txt"), b"ok\xff\n").unwrap();
        let bytes = execute(&tools, "edit", json!({"path":"bytes.txt","edits":edits}));
        assert_eq!(bytes["ok"], false, "{bytes}");
        assert_eq!(fs::read(dir.path().join("bytes.txt")).unwrap(), b"ok\xff\n");
    }

    #[test]
    fn hashline_read_cap_boundaries_and_editability() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
        let path = dir.path().join("cap.txt");

        // Exactly the cap without a final newline: the single full line keeps
        // an anchor that edit accepts, and the replacement stays unterminated.
        fs::write(&path, "x".repeat(OUTPUT_LIMIT)).unwrap();
        let (content, truncated) = read_content(&tools, "cap.txt");
        assert!(!truncated);
        let rows = row_text(&content);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].len(), OUTPUT_LIMIT);
        assert!(rows[0].bytes().all(|byte| byte == b'x'));
        let anchor = anchors(&content)[0].clone();
        let edited = execute(
            &tools,
            "edit",
            json!({"path":"cap.txt","edits":[
                {"op":"replace","start":&anchor,"end":&anchor,"lines":["edited"]}
            ]}),
        );
        assert_eq!(
            edited,
            json!({"ok":true,"edits_applied":1,"bytes_written":6})
        );
        assert_eq!(fs::read(&path).unwrap(), b"edited");
        let (again, truncated) = read_content(&tools, "cap.txt");
        assert!(!truncated);
        assert_eq!(row_text(&again), ["edited"]);
        assert_ne!(anchors(&again)[0], anchor);

        // 超长单行返回明确错误，避免空页无法继续。
        fs::write(&path, "y".repeat(OUTPUT_LIMIT + 1)).unwrap();
        let result = execute(&tools, "read", json!({"path":"cap.txt"}));
        assert_eq!(result["ok"], false);
        assert!(result["error"].as_str().unwrap().contains("read_max_bytes"));

        // A complete first line stays editable while a long tail is cut.
        fs::write(&path, format!("ok\n{}", "z".repeat(OUTPUT_LIMIT))).unwrap();
        let (content, truncated) = read_content(&tools, "cap.txt");
        assert!(truncated);
        assert_eq!(row_text(&content), ["ok"]);

        // Cap falling between CR and LF exposes only the preceding complete row.
        fs::write(
            &path,
            format!("ok\r\n{}\r\ntail\r\n", "z".repeat(OUTPUT_LIMIT - 5)),
        )
        .unwrap();
        let (content, truncated) = read_content(&tools, "cap.txt");
        assert!(truncated);
        assert_eq!(row_text(&content), ["ok"]);

        // Cap falling inside a multibyte character exposes no partial row.
        fs::write(&path, format!("ok\n{}\ntail\n", "中".repeat(21845))).unwrap();
        let (content, truncated) = read_content(&tools, "cap.txt");
        assert!(truncated);
        assert_eq!(row_text(&content), ["ok"]);
    }

    #[test]
    fn replace_read_returns_plain_text_with_original_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();

        fs::write(dir.path().join("plain.txt"), "one\ntwo\nthree\n").unwrap();
        assert_eq!(
            read_content(&tools, "plain.txt"),
            ("one\ntwo\nthree\n".to_owned(), false)
        );

        fs::write(dir.path().join("crlf.txt"), b"a\r\nb\r\n").unwrap();
        assert_eq!(
            read_content(&tools, "crlf.txt"),
            ("a\r\nb\r\n".to_owned(), false)
        );

        fs::write(dir.path().join("empty.txt"), b"").unwrap();
        assert_eq!(read_content(&tools, "empty.txt"), (String::new(), false));

        fs::write(dir.path().join("lossy.txt"), b"ok\xff\xf0\x9f\n").unwrap();
        assert_eq!(
            read_content(&tools, "lossy.txt"),
            ("ok\u{fffd}\u{fffd}\n".to_owned(), false)
        );

        fs::write(dir.path().join("cap.txt"), "x".repeat(OUTPUT_LIMIT)).unwrap();
        let (content, truncated) = read_content(&tools, "cap.txt");
        assert!(!truncated);
        assert_eq!(content, "x".repeat(OUTPUT_LIMIT));

        fs::write(dir.path().join("cap.txt"), "y".repeat(OUTPUT_LIMIT + 1)).unwrap();
        let result = execute(&tools, "read", json!({"path":"cap.txt"}));
        assert_eq!(result["ok"], false);

        // 字节上限落在多字节字符内时，也只保留前面的完整行。
        let bytes = format!("ok\n{}", "中".repeat(21_846));
        fs::write(dir.path().join("cap.txt"), bytes.as_bytes()).unwrap();
        let (content, truncated) = read_content(&tools, "cap.txt");
        assert!(truncated);
        assert_eq!(content, "ok\n");
    }

    #[test]
    fn replace_edit_roundtrip_counts_bytes_and_allows_mixed_noop() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let path = dir.path().join("demo.txt");
        fs::write(&path, "keep “Ｋ”  \nsay “Hi”—x  \ntail\n").unwrap();
        let result = execute(
            &tools,
            "edit",
            json!({"path":"demo.txt","edits":[
                {"oldText":"tail","newText":"TAIL"},
                {"oldText":"say \"Hi\"-x","newText":"said"}
            ]}),
        );
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["edits_applied"], 2);
        let expected = "keep “Ｋ”  \nsaid\nTAIL\n";
        assert_eq!(result["bytes_written"], expected.len());
        assert_eq!(fs::read_to_string(&path).unwrap(), expected);

        // A no-op item alongside a real change still succeeds for the batch.
        let result = execute(
            &tools,
            "edit",
            json!({"path":"demo.txt","edits":[
                {"oldText":"said","newText":"said"},
                {"oldText":"TAIL","newText":"recovered"}
            ]}),
        );
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["edits_applied"], 2);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "keep “Ｋ”  \nsaid\nrecovered\n"
        );
    }

    #[test]
    fn replace_edit_rejects_whole_batch_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let path = dir.path().join("demo.txt");
        fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let original = fs::read(&path).unwrap();

        let missing = execute(
            &tools,
            "edit",
            json!({"path":"absent.txt","edits":[{"oldText":"a","newText":"b"}]}),
        );
        assert_eq!(missing["ok"], false, "{missing}");
        assert!(!dir.path().join("absent.txt").exists());

        let cases = [
            ("empty edits", json!({"path":"demo.txt","edits":[]})),
            (
                "empty oldText",
                json!({"path":"demo.txt","edits":[{"oldText":"","newText":"b"}]}),
            ),
            (
                "missing oldText",
                json!({"path":"demo.txt","edits":[{"oldText":"nope","newText":"b"}]}),
            ),
            (
                "non-unique oldText",
                json!({"path":"demo.txt","edits":[{"oldText":"o","newText":"b"}]}),
            ),
            (
                "overlapping edits",
                json!({"path":"demo.txt","edits":[
                    {"oldText":"one","newText":"x"},
                    {"oldText":"ne\nt","newText":"y"}
                ]}),
            ),
            (
                "one valid one missing",
                json!({"path":"demo.txt","edits":[
                    {"oldText":"one","newText":"ONE"},
                    {"oldText":"nope","newText":"b"}
                ]}),
            ),
            (
                "whole batch is a no-op",
                json!({"path":"demo.txt","edits":[{"oldText":"one","newText":"one"}]}),
            ),
            (
                "unknown item field",
                json!({"path":"demo.txt","edits":[{"oldText":"a","newText":"b","why":"nope"}]}),
            ),
            (
                "missing newText",
                json!({"path":"demo.txt","edits":[{"oldText":"a"}]}),
            ),
            (
                "hashline operation shape",
                json!({"path":"demo.txt","edits":[{"op":"replace","start":"1:0","end":"1:0","lines":["x"]}]}),
            ),
            (
                "classic top-level oldText",
                json!({"path":"demo.txt","oldText":"one","newText":"two"}),
            ),
            (
                "string instead of edits array",
                json!({"path":"demo.txt","edits":"one"}),
            ),
            (
                "unknown top-level field",
                json!({"path":"demo.txt","edits":[{"oldText":"one","newText":"two"}],"reason":"nope"}),
            ),
        ];
        for (name, args) in cases {
            let result = execute(&tools, "edit", args);
            assert_eq!(result["ok"], false, "{name}: {result}");
            assert_eq!(fs::read(&path).unwrap(), original, "{name}");
        }

        fs::write(dir.path().join("empty.txt"), "").unwrap();
        let empty = execute(
            &tools,
            "edit",
            json!({"path":"empty.txt","edits":[{"oldText":"a","newText":"b"}]}),
        );
        assert_eq!(empty["ok"], false, "{empty}");
        assert_eq!(fs::read(dir.path().join("empty.txt")).unwrap(), b"");

        fs::create_dir(dir.path().join("folder")).unwrap();
        let folder = execute(
            &tools,
            "edit",
            json!({"path":"folder","edits":[{"oldText":"a","newText":"b"}]}),
        );
        assert_eq!(folder["ok"], false, "{folder}");

        fs::write(dir.path().join("bytes.txt"), b"ok\xff\n").unwrap();
        let bytes = execute(
            &tools,
            "edit",
            json!({"path":"bytes.txt","edits":[{"oldText":"ok","newText":"b"}]}),
        );
        assert_eq!(bytes["ok"], false, "{bytes}");
        assert_eq!(fs::read(dir.path().join("bytes.txt")).unwrap(), b"ok\xff\n");

        // Hashline-mode tools reject replace-shaped arguments instead of
        // trying the other mode's deserialization.
        let hashline = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
        let wrong_mode = execute(
            &hashline,
            "edit",
            json!({"path":"demo.txt","edits":[{"oldText":"one","newText":"two"}]}),
        );
        assert_eq!(wrong_mode["ok"], false, "{wrong_mode}");
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    fn background_wait(tools: &Tools, id: &str, ready: impl Fn(&Value) -> bool) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let snapshot = execute(tools, "bash_task", json!({"task_id":id,"action":"status"}));
            if ready(&snapshot) {
                return snapshot;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "background task did not reach expected state: {snapshot}"
            );
            thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn background_marker(path: &Path) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Ok(text) = fs::read_to_string(path)
                && !text.is_empty()
            {
                return text;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "missing marker: {}",
                path.display()
            );
            thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn background_natural_exit_cancellation_and_tools_drop_reap_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let natural = execute(
            &tools,
            "bash",
            json!({"mode":"background","command":"sleep 60 & child=$!; printf '%s' \"$child\" > natural.pid; printf retained; exit 7"}),
        );
        let terminal = background_wait(&tools, natural["task_id"].as_str().unwrap(), |value| {
            value["status"] != "running"
        });
        assert_eq!(terminal["status"], "failed");
        assert_eq!(terminal["exit_code"], 7);
        assert_eq!(terminal["stdout"], "retained");
        wait_until_gone(
            background_marker(&dir.path().join("natural.pid"))
                .parse()
                .unwrap(),
            "natural-exit descendant",
        );
        let token = CancellationToken::new();
        let execution = tools.execute("bash", &json!({"mode":"background","command":"trap '' TERM; sleep 60 & child=$!; printf '%s\\n%s' \"$$\" \"$child\" > cancelled.pending; mv cancelled.pending cancelled.pid; printf cancelled-output; wait \"$child\""}).to_string(), "cancel-start", &token, &mut |_| {});
        let started: Value = serde_json::from_str(&execution.output).unwrap();
        let pids = background_marker(&dir.path().join("cancelled.pid"));
        let other = execute(
            &tools,
            "bash",
            json!({"mode":"background","command":"printf '%s\\n%s' \"$$\" \"$TMPDIR\" > drop.pending; mv drop.pending drop.marker; exec sleep 60"}),
        );
        let drop_marker = background_marker(&dir.path().join("drop.marker"));
        token.cancel();
        let cancelled = background_wait(&tools, started["task_id"].as_str().unwrap(), |value| {
            value["status"] != "running"
        });
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["ok"], false);
        assert_eq!(cancelled["stdout"], "cancelled-output");
        assert_eq!(
            cancelled["error"],
            "Background task cancelled; side effects were not rolled back."
        );
        for pid in pids.lines() {
            wait_until_gone(pid.parse().unwrap(), "cancelled background");
        }
        assert_eq!(
            execute(
                &tools,
                "bash_task",
                json!({"task_id":other["task_id"],"action":"status"})
            )["status"],
            "running"
        );
        drop(tools);
        let (pid, scratch) = drop_marker.split_once('\n').unwrap();
        wait_until_gone(pid.parse().unwrap(), "Tools drop background");
        assert!(!Path::new(scratch).exists());
    }

    #[test]
    fn background_survives_foreground_deadline_and_stops_group() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        tools.limits.bash_timeout_seconds = 1;
        let started = execute(
            &tools,
            "bash",
            json!({"mode":"background","command":"trap '' TERM; printf '%s' \"$$\" > parent.pid; sleep 60 & child=$!; printf '%s' \"$child\" > child.pid; printf '%s' \"$TMPDIR\" > scratch.path; printf ready; wait \"$child\""}),
        );
        let id = started["task_id"].as_str().unwrap();
        let parent = background_marker(&dir.path().join("parent.pid"));
        let child = background_marker(&dir.path().join("child.pid"));
        let scratch = background_marker(&dir.path().join("scratch.path"));
        thread::sleep(std::time::Duration::from_millis(2200));
        let alive = execute(
            &tools,
            "bash",
            json!({"command":format!("kill -0 {parent} && kill -0 {child} && printf alive")}),
        );
        assert_eq!(alive["stdout"], "alive");
        assert_eq!(alive["exit_code"], 0);
        let running = background_wait(&tools, id, |value| value["stdout"] == "ready");
        assert_eq!(running["status"], "running");
        assert!(Path::new(&scratch).exists());
        let stopped = execute(&tools, "bash_task", json!({"task_id":id,"action":"stop"}));
        assert_eq!(stopped["status"], "stopped");
        assert_eq!(stopped["ok"], true);
        wait_until_gone(parent.parse().unwrap(), "background parent");
        wait_until_gone(child.parse().unwrap(), "background child");
        assert!(!Path::new(&scratch).exists());
        assert_eq!(
            execute(&tools, "bash_task", json!({"task_id":id,"action":"stop"})),
            stopped
        );
        let next = execute(&tools, "bash", json!({"command":"printf next"}));
        assert_eq!(next["stdout"], "next");
    }

    #[test]
    fn background_output_is_bounded_without_blocking_progress() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        for (command, lines) in [
            (
                "printf '%70000s' ''; printf '%70000s' '' >&2; printf done > completed; exit 7",
                false,
            ),
            (
                "for ((i=0;i<10000;i++)); do printf 'out\\n'; printf 'err\\n' >&2; done; printf done > completed; exit 7",
                true,
            ),
        ] {
            let started = execute(
                &tools,
                "bash",
                json!({"mode":"background","command":command}),
            );
            let id = started["task_id"].as_str().unwrap();
            let terminal = background_wait(&tools, id, |value| value["status"] != "running");
            assert_eq!(terminal["exit_code"], 7, "{terminal}");
            assert_eq!(terminal["stdout_truncated"], true);
            assert_eq!(terminal["stderr_truncated"], true);
            let stdout = terminal["stdout"].as_str().unwrap();
            let stderr = terminal["stderr"].as_str().unwrap();
            if lines {
                assert_eq!(stdout.lines().count() + stderr.lines().count(), 500);
                assert!(stdout.lines().all(|line| line == "out"));
                assert!(stderr.lines().all(|line| line == "err"));
            } else {
                assert_eq!(stdout, " ".repeat(OUTPUT_LIMIT));
                assert_eq!(stderr, " ".repeat(OUTPUT_LIMIT));
            }
            assert_eq!(
                fs::read_to_string(dir.path().join("completed")).unwrap(),
                "done"
            );
            fs::remove_file(dir.path().join("completed")).unwrap();
        }
        // Byte accounting precedes line accounting. An unterminated retained
        // line keeps its allowance across arbitrary pipe reads.
        for count in [OUTPUT_LIMIT, OUTPUT_LIMIT + 1] {
            let started = execute(
                &tools,
                "bash",
                json!({"mode":"background","command":format!("printf '%{count}s' ''")}),
            );
            let terminal = background_wait(&tools, started["task_id"].as_str().unwrap(), |value| {
                value["status"] != "running"
            });
            assert_eq!(terminal["stdout"], " ".repeat(OUTPUT_LIMIT));
            assert_eq!(terminal["stdout_truncated"], count > OUTPUT_LIMIT);
            assert_eq!(terminal["stderr_truncated"], false);
        }
        for count in [500, 501] {
            let started = execute(
                &tools,
                "bash",
                json!({"mode":"background","command":format!("for ((i=0;i<{count};i++)); do printf 'line\\n'; done")}),
            );
            let terminal = background_wait(&tools, started["task_id"].as_str().unwrap(), |value| {
                value["status"] != "running"
            });
            assert_eq!(terminal["stdout"], "line\n".repeat(500));
            assert_eq!(terminal["stdout_truncated"], count > 500);
        }
        let started = execute(
            &tools,
            "bash",
            json!({"mode":"background","command":"printf '\\344'; printf first >&2; while [ ! -f release ]; do sleep 0.01; done; printf '\\270\\255-tail\\360\\237'; printf second >&2; sleep 60"}),
        );
        let id = started["task_id"].as_str().unwrap();
        let partial = background_wait(&tools, id, |value| value["stderr"] == "first");
        assert_eq!(partial["stdout"], "");
        fs::write(dir.path().join("release"), "go").unwrap();
        let complete = background_wait(&tools, id, |value| {
            value["stderr"] == "firstsecond" && value["stdout"] == "中-tail"
        });
        assert_eq!(complete["stdout"], "中-tail");
        let stopped = execute(&tools, "bash_task", json!({"task_id":id,"action":"stop"}));
        assert_eq!(stopped["stdout"], "中-tail�");
        assert_eq!(stopped["stdout_truncated"], false);
    }

    #[test]
    fn background_terminal_status_retains_output_and_unknown_ids_fail() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let mut ids = Vec::new();
        for code in [7, 0] {
            let started = execute(
                &tools,
                "bash",
                json!({"command":format!("printf out; printf err >&2; exit {code}"),"mode":"background"}),
            );
            let id = started["task_id"].as_str().unwrap().to_owned();
            let terminal = background_wait(&tools, &id, |value| value["status"] != "running");
            assert_eq!(
                terminal["status"],
                if code == 0 { "exited" } else { "failed" }
            );
            assert_eq!(terminal["exit_code"], code);
            assert_eq!(terminal["stdout"], "out");
            assert_eq!(terminal["stderr"], "err");
            assert_eq!(
                execute(&tools, "bash_task", json!({"task_id":id,"action":"stop"})),
                terminal
            );
            assert_eq!(
                execute(&tools, "bash_task", json!({"task_id":id,"action":"status"})),
                terminal
            );
            ids.push(id);
        }
        assert_ne!(ids[0], ids[1]);
        tools.background_tasks.stop_all().unwrap();
        for id in [&ids[0], &"bg-unknown".to_owned()] {
            let unknown = execute(&tools, "bash_task", json!({"task_id":id,"action":"status"}));
            assert_eq!(unknown["ok"], false);
            assert_eq!(unknown["error"], format!("Unknown background task: {id}"));
        }
        for args in [
            json!({"command":"touch leaked","mode":"unknown"}),
            json!({"command":"touch leaked","mode":null}),
            json!({"command":"touch leaked","mode":"background","extra":true}),
            json!({"command":"   ","mode":"background"}),
        ] {
            assert_eq!(execute(&tools, "bash", args)["ok"], false);
        }
        for args in [
            json!({"task_id":"","action":"status"}),
            json!({"task_id":"  ","action":"stop"}),
            json!({"task_id":null,"action":"status"}),
            json!({"task_id":"bg-1","action":null}),
            json!({"task_id":"bg-1","action":"restart"}),
            json!({"task_id":"bg-1","action":"status","extra":true}),
        ] {
            assert_eq!(execute(&tools, "bash_task", args)["ok"], false);
        }
        assert!(!dir.path().join("leaked").exists());
        let next = execute(
            &tools,
            "bash",
            json!({"command":"exit 0","mode":"background"}),
        );
        assert!(!ids.contains(&next["task_id"].as_str().unwrap().to_owned()));
    }

    #[test]
    fn drains_both_large_pipes_without_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let result = execute(
            &tools,
            "bash",
            json!({"command":"for ((i=0;i<10000;i++)); do printf '0123456789'; printf 'abcdefghij' >&2; done"}),
        );
        assert_eq!(result["ok"], true);
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["stdout_truncated"], true);
        assert_eq!(result["stderr_truncated"], true);
        assert_eq!(result["stdout"].as_str().unwrap().len(), OUTPUT_LIMIT);
        assert_eq!(result["stderr"].as_str().unwrap().len(), OUTPUT_LIMIT);
    }

    #[test]
    fn bash_line_limit_is_shared_by_both_pipes_and_matches_live_output() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let mut stdout = String::new();
        let mut stderr = String::new();
        let token = CancellationToken::new();
        let execution = tools.execute(
            "bash",
            &json!({"command":"for ((i=0;i<10000;i++)); do printf 'out\\n'; printf 'err\\n' >&2; done; printf done > completed; exit 7"}).to_string(),
            "line-limit",
            &token,
            &mut |event| {
                if let AgentEvent::ToolOutput { stream, text, .. } = event {
                    match stream {
                        OutputStream::Stdout => stdout.push_str(&text),
                        OutputStream::Stderr => stderr.push_str(&text),
                    }
                }
            },
        );
        assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert_eq!(stdout.lines().count() + stderr.lines().count(), 500);
        assert_eq!(result["stdout"], stdout);
        assert_eq!(result["stderr"], stderr);
        assert_eq!(result["stdout_truncated"], true);
        assert_eq!(result["stderr_truncated"], true);
        assert_eq!(result["ok"], false);
        assert_eq!(result["exit_code"], 7);
        assert_eq!(result["timed_out"], false);
        assert_eq!(result["cancelled"], false);
        assert_eq!(
            fs::read_to_string(dir.path().join("completed")).unwrap(),
            "done"
        );

        let next = execute(&tools, "bash", json!({"command":"printf 'next\\n'"}));
        assert_eq!(next["stdout"], "next\n");
        assert_eq!(next["stdout_truncated"], false);
        assert_eq!(next["stderr_truncated"], false);
    }

    #[test]
    fn bash_exactly_500_lines_are_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        for redirect in ["", " >&2"] {
            let result = execute(
                &tools,
                "bash",
                json!({"command":format!("for ((i=0;i<500;i++)); do printf 'line\\n'{redirect}; done")}),
            );
            let stream = if redirect.is_empty() {
                "stdout"
            } else {
                "stderr"
            };
            assert_eq!(result[stream], "line\n".repeat(500));
            assert_eq!(result["stdout_truncated"], false);
            assert_eq!(result["stderr_truncated"], false);
            assert_eq!(result["ok"], true);
        }
    }

    #[test]
    fn file_boundaries_and_strict_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let content = "x".repeat(OUTPUT_LIMIT);
        assert_eq!(
            execute(
                &tools,
                "write",
                json!({"path":"nested/file","content":content})
            )["bytes_written"],
            OUTPUT_LIMIT
        );
        let exact = execute(&tools, "read", json!({"path":"nested/file"}));
        assert_eq!(exact["truncated"], false);
        fs::write(dir.path().join("nested/file"), "x".repeat(OUTPUT_LIMIT + 1)).unwrap();
        let over = execute(&tools, "read", json!({"path":"nested/file"}));
        assert_eq!(over["ok"], false);
        for (name, args) in [
            ("write", json!({"path":"forbidden","content":"x","extra":1})),
            ("read", json!({"path":" "})),
            ("read", json!({"path":"nested"})),
            ("bash", json!({"command":" "})),
        ] {
            assert_eq!(execute(&tools, name, args)["ok"], false);
        }
        assert!(!dir.path().join("forbidden").exists());
        let failure = execute(
            &tools,
            "bash",
            json!({"command":"printf out; printf err >&2; exit 7"}),
        );
        assert_eq!(failure["ok"], false);
        assert_eq!(failure["exit_code"], 7);
        assert_eq!(failure["stdout"], "out");
        assert_eq!(failure["stderr"], "err");
    }

    #[test]
    fn read_pagination_validates_arguments_and_retains_hashline_numbers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), "one\ntwo\nthree\n").unwrap();
        let mut tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
        tools.limits.read_max_lines = 1;
        let page = execute(&tools, "read", json!({"path":"file", "offset":2}));
        assert_eq!(page["lines_read"], 1);
        assert_eq!(page["next_offset"], 3);
        assert!(page["content"].as_str().unwrap().starts_with("2:"));
        let anchor = anchors(page["content"].as_str().unwrap())[0].clone();
        assert_eq!(
            execute(
                &tools,
                "edit",
                json!({"path":"file", "edits":[{"op":"replace","start":anchor,"end":anchor,"lines":["edited"]}]})
            )["ok"],
            true
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("file")).unwrap(),
            "one\nedited\nthree\n"
        );
        for args in [
            json!({"path":"file","offset":0}),
            json!({"path":"file","offset":-1}),
            json!({"path":"file","limit":0}),
            json!({"path":"file","limit":2}),
            json!({"path":"file","offset":"2"}),
            json!({"path":"file","limit":null}),
        ] {
            assert_eq!(execute(&tools, "read", args)["ok"], false);
        }
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "readable").unwrap();
        assert_eq!(
            execute(&tools, "read", json!({"path":outside.path()}))["ok"],
            true
        );
    }

    #[test]
    fn write_and_both_edit_modes_reject_external_and_symlink_paths() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("file");
        fs::write(&path, "original\n").unwrap();
        symlink(&path, dir.path().join("link")).unwrap();
        for mode in [EditMode::Replace, EditMode::Hashline] {
            let tools = Tools::new(dir.path().into(), mode).unwrap();
            for path in [path.clone(), dir.path().join("link")] {
                assert_eq!(
                    execute(&tools, "write", json!({"path":path,"content":"bad"}))["ok"],
                    false
                );
                let edits = match mode {
                    EditMode::Replace => json!([{"oldText":"original", "newText":"bad"}]),
                    EditMode::Hashline => {
                        json!([{"op":"replace","start":"1:deadbeefdeadbeef","end":"1:deadbeefdeadbeef","lines":["bad"]}])
                    }
                };
                assert_eq!(
                    execute(&tools, "edit", json!({"path":path,"edits":edits}))["ok"],
                    false
                );
            }
        }
        assert_eq!(fs::read(&path).unwrap(), b"original\n");
    }

    #[test]
    fn bash_deadline_stops_silent_and_term_resistant_process_groups() {
        use std::time::{Duration, Instant};
        for command in [
            "sleep 10; printf late > late",
            "trap '' TERM; printf before; printf error >&2; (trap '' TERM; sleep 10; printf late > late) & wait",
            "(sleep 10; printf late > late) & exit 0",
            "trap '' TERM; while :; do printf 0123456789; printf abcdefghij >&2; done",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
            tools.limits.bash_timeout_seconds = 1;
            let start = Instant::now();
            let result = execute(&tools, "bash", json!({"command":command}));
            assert_eq!(result["ok"], false, "{result}");
            assert_eq!(result["timed_out"], true, "{result}");
            assert_eq!(result["cancelled"], false, "{result}");
            assert!(start.elapsed() < Duration::from_secs(4));
            assert!(!dir.path().join("late").exists());
            if command.contains("printf before") {
                assert_eq!(result["stdout"], "before");
                assert_eq!(result["stderr"], "error");
            }
            assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pp-tmp-")
            }));
        }
    }

    #[test]
    fn bash_watchdog_runs_while_output_callback_is_blocked_and_temp_defaults_are_local() {
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        tools.limits.bash_timeout_seconds = 1;
        let mut blocked = false;
        let start = Instant::now();
        let token = CancellationToken::new();
        let execution = tools.execute(
            "bash",
            &json!({"command":"trap '' TERM; printf ready; sleep 3; printf late > late"})
                .to_string(),
            "call",
            &token,
            &mut |event| {
                if matches!(event, AgentEvent::ToolOutput { .. }) && !blocked {
                    blocked = true;
                    thread::sleep(Duration::from_millis(3200));
                }
            },
        );
        assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert!(blocked);
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["cancelled"], false);
        assert!(!dir.path().join("late").exists());
        assert!(start.elapsed() < Duration::from_secs(5));
        let result = execute(
            &tools,
            "bash",
            json!({"command":"printf '%s\n%s\n' \"$TMPDIR\" \"$XDG_CACHE_HOME\"; printf cache > \"$XDG_CACHE_HOME/kept\"; printf scratch > \"$TMPDIR/removed\""}),
        );
        assert_eq!(result["ok"], true, "{result}");
        for path in result["stdout"].as_str().unwrap().lines() {
            assert!(Path::new(path).starts_with(tools.cwd()));
        }
        assert!(dir.path().join(".pp-cache/kept").exists());
        assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".pp-tmp-")
        }));
    }

    fn wait_until_gone(pid: u32, what: &str) {
        use rustix::process::{Pid, test_kill_process};
        let pid = Pid::from_raw(pid as i32).expect("positive pid");
        for _ in 0..1000 {
            if test_kill_process(pid).is_err() {
                return;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("{what} (pid {pid}) still alive after cancellation");
    }

    #[test]
    fn bash_cancel_stops_term_resistant_group_and_reports_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        tools.limits.bash_timeout_seconds = 60;
        let token = CancellationToken::new();
        // Cancel only after both pid markers exist, so the tool result can be
        // asserted against a process that was definitely still running.
        let marker = {
            let watch = dir.path().to_owned();
            let token = token.clone();
            thread::spawn(move || {
                loop {
                    if watch.join("bash.pid").exists() && watch.join("bash-child.pid").exists() {
                        token.cancel();
                        return;
                    }
                    thread::sleep(std::time::Duration::from_millis(5));
                }
            })
        };
        let execution = tools.execute(
            "bash",
            &json!({"command":"trap '' TERM; printf 'cancel-ready\\n'; printf '%s' \"$$\" > bash.pid; sleep 30 & child=$!; printf '%s' \"$child\" > bash-child.pid; wait \"$child\""}).to_string(),
            "cancel-call",
            &token,
            &mut |_| {},
        );
        assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert_eq!(result["ok"], false, "{result}");
        assert_eq!(result["cancelled"], true, "{result}");
        assert_eq!(result["timed_out"], false, "{result}");
        assert!(result["stdout"].as_str().unwrap().contains("cancel-ready"));
        marker.join().unwrap();
        let bash_pid: u32 = fs::read_to_string(dir.path().join("bash.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let child_pid: u32 = fs::read_to_string(dir.path().join("bash-child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        wait_until_gone(bash_pid, "bash");
        wait_until_gone(child_pid, "bash child");
        assert!(!fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".pp-tmp-")
        }));
    }

    #[test]
    fn file_changed_follows_commit_and_never_changes_model_tool_result() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("demo.txt"), b"old\nkeep\n").unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let token = CancellationToken::new();
        let mut events = Vec::new();
        let output = tools
            .execute(
                "edit",
                &json!({"path":"demo.txt","edits":[{"oldText":"old","newText":"new"}]}).to_string(),
                "edit-1",
                &token,
                &mut |event| {
                    if matches!(event, AgentEvent::FileChanged { .. }) {
                        assert_eq!(
                            fs::read(dir.path().join("demo.txt")).unwrap(),
                            b"new\nkeep\n"
                        );
                    }
                    events.push(event);
                },
            )
            .output;
        assert!(matches!(
            &events[..],
            [
                AgentEvent::ToolStarted { .. },
                AgentEvent::FileChanged {
                    diff: crate::events::FileDiff::Text {
                        added: 1,
                        removed: 1,
                        created: false,
                        ..
                    },
                    ..
                },
                AgentEvent::ToolFinished { .. }
            ]
        ));
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap(),
            json!({"ok":true,"edits_applied":1,"bytes_written":9})
        );
        events.clear();
        tools.execute(
            "edit",
            &json!({"path":"demo.txt","edits":[{"oldText":"missing","newText":"bad"}]}).to_string(),
            "edit-fail",
            &token,
            &mut |event| events.push(event),
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, AgentEvent::FileChanged { .. }))
        );
        fs::write(dir.path().join("binary.txt"), [0xff, 0xfe]).unwrap();
        events.clear();
        let output = tools
            .execute(
                "write",
                &json!({"path":"binary.txt","content":"text\n"}).to_string(),
                "write-binary",
                &token,
                &mut |event| events.push(event),
            )
            .output;
        assert_eq!(fs::read(dir.path().join("binary.txt")).unwrap(), b"text\n");
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap(),
            json!({"ok":true,"bytes_written":5})
        );
        assert!(
            matches!(&events[1], AgentEvent::FileChanged { diff: crate::events::FileDiff::Unavailable { reason }, .. } if reason == "Previous content is not UTF-8; text diff unavailable.")
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_snapshot_permission_failure_does_not_block_commit() {
        use std::os::unix::fs::PermissionsExt;
        if rustix::process::getuid().as_raw() == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unreadable.txt");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let mut events = Vec::new();
        let output = tools
            .execute(
                "write",
                &json!({"path":"unreadable.txt","content":"new"}).to_string(),
                "unreadable",
                &CancellationToken::new(),
                &mut |event| events.push(event),
            )
            .output;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(serde_json::from_str::<Value>(&output).unwrap()["ok"], true);
        assert!(
            matches!(&events[1], AgentEvent::FileChanged { diff: crate::events::FileDiff::Unavailable { reason }, .. } if reason == "Previous content could not be read; text diff unavailable.")
        );
    }

    #[test]
    fn cancelled_token_skips_execution_but_reports_started_and_finished() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let token = CancellationToken::new();
        token.cancel();
        let mut events = 0;
        let execution = tools.execute(
            "write",
            &json!({"path":"skipped.txt","content":"must-not-exist"}).to_string(),
            "skip-call",
            &token,
            &mut |event| match event {
                AgentEvent::ToolStarted { .. } | AgentEvent::ToolFinished { .. } => events += 1,
                _ => panic!("unexpected event during skipped tool"),
            },
        );
        assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
        assert_eq!(events, 2);
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert_eq!(
            result,
            json!({"ok":false,"cancelled":true,"error":"Not executed: task cancelled."})
        );
        assert!(!dir.path().join("skipped.txt").exists());
        assert!(!dir.path().join("skipped").exists());
    }

    struct ChunkedReader<'a> {
        remaining: &'a [u8],
        chunk_size: usize,
    }

    impl Read for ChunkedReader<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let count = self.remaining.len().min(buffer.len()).min(self.chunk_size);
            buffer[..count].copy_from_slice(&self.remaining[..count]);
            self.remaining = &self.remaining[count..];
            Ok(count)
        }
    }

    fn stream_capture(bytes: &[u8], chunk_size: usize) -> (String, String, bool, usize) {
        let mut reader = ChunkedReader {
            remaining: bytes,
            chunk_size,
        };
        let (sender, receiver) = sync_channel(16);
        let mut captured = StreamText::default();
        let mut live = String::new();
        let mut received = 0;
        let truncated = thread::scope(|scope| {
            let reader = &mut reader;
            let worker = scope.spawn(move || {
                drain_pipe(
                    reader,
                    OutputStream::Stdout,
                    sender,
                    &std::sync::atomic::AtomicBool::new(false),
                )
            });
            for (stream, bytes) in receiver {
                assert!(matches!(stream, OutputStream::Stdout));
                assert!(bytes.len() <= PIPE_CHUNK_SIZE);
                received += bytes.len();
                live.push_str(&captured.append(&bytes));
            }
            live.push_str(&captured.finish());
            worker.join().unwrap().unwrap()
        });
        assert!(
            reader.remaining.is_empty(),
            "over-limit bytes must be drained"
        );
        (captured.text, live, truncated, received)
    }

    #[test]
    fn streaming_utf8_fragments_match_whole_lossy_decoding() {
        let cases: &[&[u8]] = &[
            "中😀".as_bytes(),
            b"a\xe4\xb8x\xf0\x9f\xff\x80z",
            b"\xf0\x9f\x98",
            b"\xe4\xf0\x9f\x98\x80",
            b"\xed\xa0\x80\xc0\xaf\xf4\x90\x80\x80",
            b"",
        ];
        for bytes in cases {
            for chunk_size in 1..=9 {
                let (text, live, truncated, received) = stream_capture(bytes, chunk_size);
                let expected = String::from_utf8_lossy(bytes);
                assert_eq!(text, expected, "chunk size {chunk_size}");
                assert_eq!(live, expected, "chunk size {chunk_size}");
                assert!(!truncated);
                assert_eq!(received, bytes.len());
            }
        }
    }

    #[test]
    fn streaming_exact_cap_and_truncated_utf8_keep_live_and_result_equal() {
        for count in [OUTPUT_LIMIT - 1, OUTPUT_LIMIT, OUTPUT_LIMIT + 1] {
            let bytes = vec![b'x'; count];
            let (text, live, truncated, received) = stream_capture(&bytes, PIPE_CHUNK_SIZE);
            assert_eq!(received, count.min(OUTPUT_LIMIT));
            assert_eq!(text, "x".repeat(count.min(OUTPUT_LIMIT)));
            assert_eq!(live, text);
            assert_eq!(truncated, count > OUTPUT_LIMIT);
        }
        // The retained boundary can split a character. EOF of retained bytes
        // must produce the same single replacement as one-shot lossy decoding.
        for prefix in [OUTPUT_LIMIT - 1, OUTPUT_LIMIT - 2, OUTPUT_LIMIT - 3] {
            let mut bytes = vec![b'a'; prefix];
            bytes.extend_from_slice("中😀".as_bytes());
            bytes.resize(bytes.len() + OUTPUT_LIMIT * 2, b'z');
            for chunk_size in [1, 7, PIPE_CHUNK_SIZE] {
                let (text, live, truncated, received) = stream_capture(&bytes, chunk_size);
                assert_eq!(text, String::from_utf8_lossy(&bytes[..OUTPUT_LIMIT]));
                assert_eq!(live, text);
                assert!(truncated);
                assert_eq!(received, OUTPUT_LIMIT);
            }
        }
    }

    #[test]
    fn streaming_bash_reports_both_pipes_before_exit_without_newlines() {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let args = json!({
            "command":"printf 'out-first'; printf 'err-first' >&2; for ((i=0;i<200;i++)); do [[ -e release ]] && break; sleep 0.01; done; printf 'out-last'"
        })
        .to_string();
        let mut stdout = String::new();
        let mut stderr = String::new();
        let mut released = false;
        let mut started = false;
        let mut finished = false;
        let token = CancellationToken::new();
        let execution = tools.execute(
            "bash",
            &args,
            "live-call",
            &token,
            &mut |event| match event {
                AgentEvent::ToolStarted {
                    id,
                    name,
                    arguments,
                } => {
                    assert!(!started && !finished);
                    assert_eq!(id, "live-call");
                    assert_eq!(name, "bash");
                    assert_eq!(arguments, args);
                    started = true;
                }
                AgentEvent::ToolOutput { id, stream, text } => {
                    assert!(started && !finished);
                    assert_eq!(id, "live-call");
                    assert!(!text.is_empty());
                    if !released {
                        assert!(!text.contains("out-last"), "output arrived only after exit");
                    }
                    match stream {
                        OutputStream::Stdout => stdout.push_str(&text),
                        OutputStream::Stderr => stderr.push_str(&text),
                    }
                    if !released && stdout == "out-first" && stderr == "err-first" {
                        fs::write(dir.path().join("release"), "").unwrap();
                        released = true;
                    }
                }
                AgentEvent::ToolFinished { id, result } => {
                    assert!(started && released && !finished);
                    assert_eq!(id, "live-call");
                    assert_eq!(result["stdout"], stdout);
                    assert_eq!(result["stderr"], stderr);
                    assert_eq!(result["exit_code"], 0);
                    finished = true;
                }
                _ => panic!("unexpected assistant event"),
            },
        );
        assert!(execution.stop_error.is_none(), "{:?}", execution.stop_error);
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert!(released && finished);
        assert_eq!(stdout, "out-firstout-last");
        assert_eq!(stderr, "err-first");
        assert_eq!(result["ok"], true);
    }
}
