// Consumer-visible coverage for JSONL session persistence: lazy creation,
// resume (-r), continue (-c), loader boundaries, settings persistence, and
// persistence-failure behavior. Tests assert native request content and tool
// effect boundaries, never just file counts.

use super::*;
use std::{
    fs,
    io::Read,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
};

const APIS: [Api; 2] = [Api::Chat, Api::Responses];

// Local protocol dispatch: the streaming module keeps its own private helper,
// so this module stays self-contained instead of widening that visibility.
#[derive(Clone, Copy, PartialEq)]
enum Api {
    Chat,
    Responses,
}

impl Api {
    fn path(self) -> &'static str {
        match self {
            Self::Chat => "/v1/chat/completions",
            Self::Responses => "/v1/responses",
        }
    }
    fn models(self, server: &Server) -> String {
        match self {
            Self::Chat => fixture_models(server),
            Self::Responses => response_models(server, Some("response")),
        }
    }
    fn receive(self, server: &Server) -> (Request, Value) {
        receive_at(server, self.path())
    }
    fn final_reply(self, request: Request, text: &str) {
        match self {
            Self::Chat => final_reply(request, text),
            Self::Responses => response_final(request, text),
        }
    }
    fn calls_reply(self, request: Request, calls: Value) {
        match self {
            Self::Chat => calls_reply(request, calls),
            Self::Responses => response_reply(request, calls),
        }
    }
    fn call(self, id: &str, name: &str, args: Value) -> Value {
        match self {
            Self::Chat => call(id, name, args),
            Self::Responses => response_call(id, name, args),
        }
    }
    fn history(self, body: &Value) -> &[Value] {
        match self {
            Self::Chat => body["messages"].as_array().unwrap(),
            Self::Responses => body["input"].as_array().unwrap(),
        }
    }
    // Ordered user prompts and non-empty assistant text turns, ignoring
    // tool-call plumbing that the native-equality checks cover separately.
    fn turns(self, body: &Value) -> (Vec<String>, Vec<String>) {
        let mut users = Vec::new();
        let mut assistants = Vec::new();
        for item in self.history(body) {
            if item["role"] == "user" {
                users.push(slash_message_text(item));
            } else if item["role"] == "assistant" && !item["content"].is_null() {
                let text = slash_message_text(item);
                if !text.is_empty() {
                    assistants.push(text);
                }
            }
        }
        (users, assistants)
    }
    fn tool_result(self, body: &Value, id: &str) -> Value {
        let item = self
            .history(body)
            .iter()
            .find(|item| match self {
                Self::Chat => item["role"] == "tool" && item["tool_call_id"] == id,
                Self::Responses => item["type"] == "function_call_output" && item["call_id"] == id,
            })
            .expect("missing tool result")
            .clone();
        match self {
            Self::Chat => result(&item, id),
            Self::Responses => response_result(&item, id),
        }
    }
    fn assert_level(self, body: &Value, level: &str) {
        match self {
            Self::Chat => assert_eq!(body["reasoning_effort"], level),
            Self::Responses => {
                assert_eq!(body["reasoning"]["effort"], level);
                if level == "none" {
                    assert!(body["reasoning"].get("summary").is_none());
                } else {
                    assert_eq!(body["reasoning"]["summary"], "auto");
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Pipe {
    Out,
    Err,
}

// Both pipes are drained from spawn onward: a test observing one pipe must
// never make the child block on the other.
struct Live {
    cli: Cli,
    rx: Receiver<(Pipe, Vec<u8>)>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    readers: Vec<thread::JoinHandle<()>>,
}

impl Live {
    fn new(mut cli: Cli) -> Self {
        let child = cli.0.as_mut().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let (tx, rx) = mpsc::channel();
        fn reader(
            mut pipe: impl Read + Send + 'static,
            kind: Pipe,
            tx: mpsc::Sender<(Pipe, Vec<u8>)>,
        ) -> thread::JoinHandle<()> {
            thread::spawn(move || {
                let mut bytes = [0; 4096];
                loop {
                    match pipe.read(&mut bytes) {
                        Ok(0) => break,
                        Ok(count) => {
                            if tx.send((kind, bytes[..count].to_vec())).is_err() {
                                break;
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(error) => panic!("reading CLI pipe: {error}"),
                    }
                }
            })
        }
        let readers = vec![
            reader(stdout, Pipe::Out, tx.clone()),
            reader(stderr, Pipe::Err, tx),
        ];
        Self {
            cli,
            rx,
            stdout: Vec::new(),
            stderr: Vec::new(),
            readers,
        }
    }

    fn input(&mut self, text: &str) {
        self.cli.input(text);
    }

    fn record(&mut self, (pipe, bytes): (Pipe, Vec<u8>)) {
        match pipe {
            Pipe::Out => self.stdout.extend(bytes),
            Pipe::Err => self.stderr.extend(bytes),
        }
    }

    fn text(&self, pipe: Pipe) -> String {
        String::from_utf8_lossy(match pipe {
            Pipe::Out => &self.stdout,
            Pipe::Err => &self.stderr,
        })
        .into_owned()
    }

    fn wait_for(&mut self, pipe: Pipe, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.text(pipe).contains(expected) {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = self.rx.recv_timeout(remaining).unwrap_or_else(|error| {
                panic!(
                    "missing live output {expected:?}: {error}; stdout={:?}; stderr={:?}",
                    self.text(Pipe::Out),
                    self.text(Pipe::Err)
                )
            });
            self.record(event);
        }
    }

    fn finish(self) -> Output {
        let Self {
            cli,
            rx,
            mut stdout,
            mut stderr,
            readers,
        } = self;
        // Cli owns panic cleanup and a bounded wait; its pipes were taken above.
        let mut output = cli.finish();
        for (pipe, bytes) in rx {
            match pipe {
                Pipe::Out => stdout.extend(bytes),
                Pipe::Err => stderr.extend(bytes),
            }
        }
        for reader in readers {
            reader.join().unwrap();
        }
        output.stdout = stdout;
        output.stderr = stderr;
        output
    }
}

fn sessions_dir(home: &Path) -> PathBuf {
    home.join(".pp/agent/sessions")
}

fn configure(home: &Path, settings: &str, models: &str) {
    std::fs::write(config_path(home, "settings.jsonc"), settings).unwrap();
    std::fs::write(config_path(home, "models.jsonc"), models).unwrap();
}

fn command(home: &Path, cwd: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pp"));
    command
        .current_dir(cwd)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args(args);
    command
}

fn start(home: &Path, cwd: &Path, args: &[&str]) -> Live {
    Live::new(Cli(Some(command(home, cwd, args).spawn().unwrap())))
}

fn start_configured(api: Api, server: &Server, home: &Path, cwd: &Path, args: &[&str]) -> Live {
    configure(home, fixture_settings(), &api.models(server));
    start(home, cwd, args)
}

fn valid_session_id(id: &str) -> bool {
    let Some((time, random)) = id.rsplit_once('-') else {
        return false;
    };
    time.len() == 19
        && chrono::NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H-%M-%S").is_ok()
        && random.len() == 32
        && random
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn session_ids(home: &Path) -> Vec<String> {
    let Ok(iter) = fs::read_dir(sessions_dir(home)) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = iter
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().into_string().ok()?;
            let id = name.strip_suffix(".jsonl")?;
            valid_session_id(id).then(|| id.to_owned())
        })
        .collect();
    ids.sort();
    ids
}

fn only_session_id(home: &Path) -> String {
    let ids = session_ids(home);
    assert_eq!(ids.len(), 1, "expected exactly one session: {ids:?}");
    ids[0].clone()
}

// Every entry under the sessions dir with its content (or link target), so a
// failed restore can be proven not to touch or fork saved files.
fn snapshot(home: &Path) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    if let Ok(iter) = fs::read_dir(sessions_dir(home)) {
        for entry in iter {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let kind = entry.file_type().unwrap();
            let content = if kind.is_symlink() {
                format!("link:{}", entry.path().read_link().unwrap().display())
            } else if kind.is_file() {
                String::from_utf8_lossy(&fs::read(entry.path()).unwrap()).into_owned()
            } else {
                "directory".to_owned()
            };
            entries.push((name, content));
        }
    }
    entries.sort();
    entries
}

// Full-line JSONL records; an unterminated trailing record (the only
// ignorable corruption) is excluded here on purpose.
fn records(path: &Path) -> Vec<Value> {
    let text = fs::read_to_string(path).unwrap();
    let terminated = match text.rfind('\n') {
        Some(index) if !text.ends_with('\n') => &text[..index + 1],
        _ => text.as_str(),
    };
    terminated
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("invalid record line: {error}"))
        })
        .collect()
}

// Only native custom records and effort changes may rebuild model context;
// canonical messages and display events deliberately never contribute.
fn rebuild_request(records: &[Value]) -> Value {
    let mut request = Value::Null;
    let mut completion = false;
    for record in records {
        let data = &record["data"];
        match (
            record["type"].as_str().unwrap(),
            record["customType"].as_str(),
        ) {
            ("custom", Some("pp.session_state")) => {
                completion = data["session"]["protocol"] == "completion";
                request = data["session"]["request"].clone();
            }
            ("custom", Some("pp.instructions")) => {
                let system = data["system"].clone();
                if completion {
                    request["messages"][0]["content"] = system;
                } else {
                    request["instructions"] = system;
                }
            }
            ("thinking_level_change", _) => {
                let level = record["thinkingLevel"].as_str().unwrap();
                if completion {
                    request["reasoning_effort"] = json!(level);
                } else {
                    if !request["reasoning"].is_object() {
                        request["reasoning"] = json!({});
                    }
                    let reasoning = request["reasoning"].as_object_mut().unwrap();
                    reasoning.insert("effort".to_owned(), json!(level));
                    if level == "none" {
                        reasoning.remove("summary");
                    } else {
                        reasoning.insert("summary".to_owned(), json!("auto"));
                    }
                }
            }
            ("custom", Some("pp.native_append")) => {
                let (history, appended) = if completion {
                    assert_eq!(data["protocol"], "completion");
                    ("messages", "messages")
                } else {
                    assert_eq!(data["protocol"], "response");
                    ("input", "items")
                };
                request[history]
                    .as_array_mut()
                    .unwrap()
                    .extend(data[appended].as_array().unwrap().iter().cloned());
            }
            _ => {}
        }
    }
    assert!(!request.is_null(), "no pp.session_state in records");
    request
}

fn session_state_mut(records: &mut [Value]) -> &mut Value {
    &mut records
        .iter_mut()
        .find(|record| record["customType"] == "pp.session_state")
        .expect("missing native state")["data"]
}

fn canonical_messages(records: &[Value]) -> Vec<Value> {
    records
        .iter()
        .filter(|record| record["type"] == "message")
        .map(|record| record["message"].clone())
        .collect()
}

fn assert_session_contract(records: &[Value], id: &str, cwd: &Path) {
    let header = &records[0];
    assert_eq!(header["type"], "session");
    assert_eq!(header["version"], 3);
    assert_eq!(header["id"], id);
    assert_eq!(
        header["cwd"],
        cwd.canonicalize().unwrap().display().to_string()
    );
    assert_eq!(header.as_object().unwrap().len(), 5);
    let mut ids = std::collections::HashSet::new();
    let mut parent = Value::Null;
    for (index, record) in records.iter().enumerate() {
        let timestamp = record["timestamp"].as_str().unwrap();
        let parsed = chrono::DateTime::parse_from_rfc3339(timestamp).unwrap();
        assert!(parsed.timestamp_millis() >= 0);
        assert_eq!(
            timestamp,
            parsed
                .to_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        );
        if index == 0 {
            continue;
        }
        let entry_id = record["id"].as_str().unwrap();
        assert_eq!(entry_id.len(), 8);
        assert!(
            entry_id
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert!(ids.insert(entry_id), "duplicate entry id");
        assert!(record.get("parentId").is_some());
        assert_eq!(record["parentId"], parent);
        parent = record["id"].clone();
        match record["type"].as_str().unwrap() {
            "message" => {
                let message = &record["message"];
                assert!(message["timestamp"].as_i64().is_some_and(|time| time >= 0));
                assert!(matches!(
                    message["role"].as_str(),
                    Some("user" | "assistant" | "toolResult")
                ));
                assert!(message.get("usage").is_none());
                assert!(message.get("cost").is_none());
            }
            "model_change" => assert_eq!(record["role"], "default"),
            "thinking_level_change" => assert!(record["thinkingLevel"].is_string()),
            "custom" => {
                assert!(matches!(
                    record["customType"].as_str(),
                    Some(
                        "pp.session_state"
                            | "pp.native_append"
                            | "pp.instructions"
                            | "pp.event"
                            | "pp.error"
                    )
                ));
                let text = record.to_string();
                for secret in ["fixture-key", "api_key", "headers", "\"Config\""] {
                    assert!(!text.contains(secret), "custom payload leaked {secret}");
                }
            }
            other => panic!("unsupported top-level record: {other}"),
        }
    }
    assert_eq!(
        records
            .iter()
            .filter(|record| record["type"] == "session")
            .count(),
        1
    );
}

// Mutated semantic-error records still need valid entry envelopes, otherwise
// chain validation can hide the loader boundary the test means to exercise.
fn push_entry(records: &mut Vec<Value>, mut entry: Value) {
    entry["id"] = json!(format!("fixture-entry-{}", records.len()));
    entry["parentId"] = records
        .last()
        .filter(|record| record["type"] != "session")
        .map_or(Value::Null, |record| record["id"].clone());
    entry["timestamp"] = records[0]["timestamp"].clone();
    records.push(entry);
}

fn relink_entries(records: &mut [Value]) {
    let mut parent = Value::Null;
    for record in records.iter_mut().skip(1) {
        record["parentId"] = parent;
        parent = record["id"].clone();
    }
}

fn make_saved_session(
    api: Api,
    server: &Server,
    home: &Path,
    cwd: &Path,
    prompt: &str,
    answer: &str,
) -> (String, PathBuf) {
    let cli = start_configured(api, server, home, cwd, &["-p", prompt]);
    let (request, _) = api.receive(server);
    api.final_reply(request, answer);
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, format!("{answer}\n").as_bytes());
    let id = only_session_id(home);
    let path = sessions_dir(home).join(format!("{id}.jsonl"));
    (id, path)
}

// A restore that must fail: nonzero exit, all expected message fragments,
// none of the forbidden secrets echoed, zero model requests, and untouched
// saved files.
fn expect_restore_failure(
    server: &Server,
    home: &Path,
    cwd: &Path,
    args: &[&str],
    expected: &[&str],
    forbidden: &[&str],
) {
    let before = snapshot(home);
    let cli = start(home, cwd, args);
    let output = cli.finish();
    assert!(
        !output.status.success(),
        "restore unexpectedly succeeded: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for &fragment in expected {
        assert!(
            stderr.contains(fragment),
            "missing {fragment:?} in: {stderr}"
        );
    }
    for &secret in forbidden {
        assert!(!stderr.contains(secret), "leaked {secret:?} in: {stderr}");
    }
    no_more(server);
    assert_eq!(
        snapshot(home),
        before,
        "failed restore must not modify or fork saved files"
    );
}

fn with_mutated_records(path: &Path, mutate: impl FnOnce(&mut Vec<Value>)) {
    let mut records = records(path);
    mutate(&mut records);
    let text = records
        .iter()
        .map(|record| format!("{record}\n"))
        .collect::<String>();
    fs::write(path, text).unwrap();
}

// A freshly saved Chat session with one mutation applied to its records.
fn corrupted_base(
    server: &Server,
    cwd: &Path,
    mutate: impl FnOnce(&mut Vec<Value>),
) -> (tempfile::TempDir, String) {
    let home = tempfile::tempdir().unwrap();
    let (id, path) = make_saved_session(
        Api::Chat,
        server,
        home.path(),
        cwd,
        "remember base",
        "base noted",
    );
    with_mutated_records(&path, mutate);
    (home, id)
}

fn insert_raw_line(path: &Path, index: usize, line: &str) {
    let text = fs::read_to_string(path).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    lines.insert(index, line);
    fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
}

fn copy_session(from: &Path, to: &Path, id: &str) {
    fs::create_dir_all(sessions_dir(to)).unwrap();
    fs::copy(
        sessions_dir(from).join(format!("{id}.jsonl")),
        sessions_dir(to).join(format!("{id}.jsonl")),
    )
    .unwrap();
}

#[test]
fn session_lazy_creation_persist_and_unique_ids_for_both_apis() {
    for api in APIS {
        // Line commands, blank input, /new and /quit never create the
        // sessions directory.
        {
            let server = server();
            let home = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let mut cli = start_configured(api, &server, home.path(), cwd.path(), &[]);
            cli.input("\n/help\n/model\n/effort high\n/effort\n/skills\n/new\n/quit\n");
            let output = cli.finish();
            assert!(output.status.success());
            assert!(!sessions_dir(home.path()).exists());
            no_more(&server);
        }
        // Before any prompt the process is idle: no request, no file.
        {
            let server = server();
            let home = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let cli = start_configured(api, &server, home.path(), cwd.path(), &[]);
            no_more(&server);
            assert!(!sessions_dir(home.path()).exists());
            cli.finish();
        }
        // A real prompt creates exactly one session, only after the HTTP
        // request reaches the provider: headers plus the first visible frame
        // prove the create-on-open hook ran before the answer completed.
        {
            let server = server();
            let home = tempfile::tempdir().unwrap();
            let cwd = tempfile::tempdir().unwrap();
            let mut cli = start_configured(
                api,
                &server,
                home.path(),
                cwd.path(),
                &["-p", "remember alpha"],
            );
            let (request, _) = api.receive(&server);
            assert!(!sessions_dir(home.path()).exists());
            let mut stream = SseReply::new(request);
            match api {
                Api::Chat => stream.event(chat_chunk(
                    json!({"role":"assistant","content":"alpha noted"}),
                    None,
                )),
                Api::Responses => stream.event(json!({
                    "type":"response.output_text.delta","sequence_number":0,
                    "item_id":"msg-final","output_index":0,"content_index":0,
                    "delta":"alpha noted","logprobs":[]
                })),
            }
            cli.wait_for(Pipe::Out, "alpha noted");
            let id = only_session_id(home.path());
            let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
            assert_eq!(
                fs::metadata(sessions_dir(home.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            let saved = records(&path);
            assert_session_contract(&saved, &id, cwd.path());
            let state = &saved
                .iter()
                .find(|record| record["customType"] == "pp.session_state")
                .unwrap()["data"];
            assert_eq!(state["version"], 1);
            assert_eq!(state["modelReference"], "fixture/primary");
            assert!(state["thinkLevel"].is_string());
            assert!(state["editMode"].is_string());
            assert!(state["toolLimits"].is_object());
            assert!(state["baseSystem"].is_string());
            assert_eq!(state.as_object().unwrap().len(), 7);
            assert_eq!(
                state["session"]["protocol"],
                if api == Api::Chat {
                    "completion"
                } else {
                    "response"
                }
            );
            let messages = canonical_messages(&saved);
            assert_eq!(messages.len(), 1);
            assert_eq!(messages[0]["role"], "user");
            assert_eq!(messages[0]["content"], "remember alpha");
            assert_eq!(saved[1]["customType"], "pp.session_state");
            assert_eq!(saved[2]["type"], "model_change");
            assert_eq!(saved[2]["model"], "fixture/primary");
            assert_eq!(saved[3]["type"], "thinking_level_change");
            assert!(
                saved
                    .iter()
                    .any(|record| record["customType"] == "pp.event")
            );
            // Complete the stream and finish the task.
            match api {
                Api::Chat => {
                    stream.event(chat_chunk(json!({}), Some("stop")));
                    stream.frame("[DONE]");
                }
                Api::Responses => {
                    let part = response_text("alpha noted");
                    let message = response_message("msg-final", json!([part.clone()]));
                    stream.event(json!({
                        "type":"response.content_part.done","sequence_number":1,
                        "item_id":"msg-final","output_index":0,"content_index":0,"part":part
                    }));
                    stream.event(json!({
                        "type":"response.output_item.done","sequence_number":2,
                        "output_index":0,"item":message
                    }));
                    stream.event(json!({
                        "type":"response.completed","sequence_number":3,
                        "response":response_body(json!([message]))
                    }));
                }
            }
            stream.finish();
            let output = cli.finish();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(output.stdout, b"alpha noted\n");
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert_eq!(stderr.matches(&format!("Session: {id}")).count(), 1);
            let saved = records(&path);
            assert_session_contract(&saved, &id, cwd.path());
            let messages = canonical_messages(&saved);
            assert_eq!(messages.len(), 2);
            let assistant_message = &messages[1];
            assert_eq!(assistant_message["role"], "assistant");
            assert_eq!(assistant_message["provider"], "fixture");
            assert_eq!(assistant_message["model"], "fixture-model");
            assert_eq!(
                assistant_message["api"],
                if api == Api::Chat {
                    "openai-completions"
                } else {
                    "openai-responses"
                }
            );
            assert_eq!(assistant_message["stopReason"], "stop");
            assert_eq!(
                assistant_message["content"],
                json!([{"type":"text","text":"alpha noted"}])
            );
            // The completed answer is loadable native history.
            let (user, assistant) = match api {
                Api::Chat => {
                    let messages = rebuild_request(&records(&path))["messages"]
                        .as_array()
                        .unwrap()
                        .clone();
                    (
                        messages[1]["content"].as_str().unwrap().to_owned(),
                        messages[2]["content"].as_str().unwrap().to_owned(),
                    )
                }
                Api::Responses => {
                    let input = rebuild_request(&records(&path))["input"]
                        .as_array()
                        .unwrap()
                        .clone();
                    (
                        input[0]["content"].as_str().unwrap().to_owned(),
                        input[1]["content"][0]["text"].as_str().unwrap().to_owned(),
                    )
                }
            };
            assert_eq!(user, "remember alpha");
            assert_eq!(assistant, "alpha noted");
        }
        // Two processes sharing one HOME create distinct sessions, proven by
        // distinct valid IDs whose checkpoints carry each process's prompt.
        {
            let server = server();
            let home = tempfile::tempdir().unwrap();
            let first = tempfile::tempdir().unwrap();
            let second = tempfile::tempdir().unwrap();
            let cli_a = start_configured(
                api,
                &server,
                home.path(),
                first.path(),
                &["-p", "from first"],
            );
            let cli_b = start_configured(
                api,
                &server,
                home.path(),
                second.path(),
                &["-p", "from second"],
            );
            for _ in 0..2 {
                let (request, body) = api.receive(&server);
                let (users, _) = api.turns(&body);
                assert_eq!(users.len(), 1);
                if users[0] == "from first" {
                    api.final_reply(request, "first done");
                } else {
                    assert_eq!(users[0], "from second");
                    api.final_reply(request, "second done");
                }
            }
            let output_a = cli_a.finish();
            let output_b = cli_b.finish();
            assert!(
                output_a.status.success(),
                "{}",
                String::from_utf8_lossy(&output_a.stderr)
            );
            assert!(
                output_b.status.success(),
                "{}",
                String::from_utf8_lossy(&output_b.stderr)
            );
            assert_eq!(output_a.stdout, b"first done\n");
            assert_eq!(output_b.stdout, b"second done\n");
            let ids = session_ids(home.path());
            assert_eq!(ids.len(), 2, "both processes must persist a session");
            assert_ne!(ids[0], ids[1]);
            let texts: Vec<String> = ids
                .iter()
                .map(|id| {
                    fs::read_to_string(sessions_dir(home.path()).join(format!("{id}.jsonl")))
                        .unwrap()
                })
                .collect();
            for prompt in ["from first", "from second"] {
                assert_eq!(
                    texts.iter().filter(|text| text.contains(prompt)).count(),
                    1,
                    "prompt {prompt:?} must appear in exactly one session"
                );
            }
            for (id, text) in ids.iter().zip(&texts) {
                let header = &records(&sessions_dir(home.path()).join(format!("{id}.jsonl")))[0];
                assert_eq!(header["id"], *id);
                assert!(text.contains(&format!("\"id\":\"{id}\"")));
            }
        }
    }
    // A blank -p prompt is rejected before any session exists.
    {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let cli = start_configured(Api::Chat, &server, home.path(), cwd.path(), &["-p", "   "]);
        let output = cli.finish();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("prompt must not be blank"));
        assert!(!sessions_dir(home.path()).exists());
        no_more(&server);
    }
    // A broken configuration still runs an interactive line mode without
    // creating any session.
    {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        configure(home.path(), fixture_settings(), "{ not valid jsonc");
        let mut cli = start(home.path(), cwd.path(), &[]);
        cli.wait_for(Pipe::Err, "Configuration unavailable");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(output.status.success());
        assert!(!sessions_dir(home.path()).exists());
        no_more(&server);
    }
}

#[test]
fn session_resume_sends_no_request_and_appends_in_place() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();
        let (id, path) = make_saved_session(
            api,
            &server,
            home.path(),
            project.path(),
            "remember alpha",
            "alpha noted",
        );
        let before = fs::read(&path).unwrap();

        // Resume without a prompt: no HTTP, no new session, original file
        // untouched while only viewing.
        let mut cli = start_configured(api, &server, home.path(), launch.path(), &["-r", &id]);
        cli.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
        no_more(&server);
        assert_eq!(fs::read(&path).unwrap(), before);

        // The next prompt keeps the old context and appends once.
        cli.input("what did I say\n");
        let (request, body) = api.receive(&server);
        let (users, assistants) = api.turns(&body);
        assert_eq!(users, ["remember alpha", "what did I say"]);
        assert_eq!(assistants, ["alpha noted"]);
        api.final_reply(request, "you said alpha");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"you said alpha\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&format!("Resumed session: {id}")));
        // Restoring an existing session never emits a new-ID notice.
        assert!(!stderr.contains("Session: "));

        // The same file was appended in place; no forked session appeared.
        assert_eq!(only_session_id(home.path()), id);
        let after = fs::read(&path).unwrap();
        assert!(
            after.starts_with(&before) && after.len() > before.len(),
            "session file must be appended, not rewritten"
        );
        let saved = records(&path);
        assert_session_contract(&saved, &id, project.path());
        let messages = canonical_messages(&saved);
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "user")
                .count(),
            2
        );
        assert_eq!(
            messages
                .iter()
                .filter(
                    |message| message["role"] == "user" && message["content"] == "what did I say"
                )
                .count(),
            1
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "assistant")
                .count(),
            2
        );
        let (users, assistants) = api.turns(&rebuild_request(&saved));
        assert_eq!(users, ["remember alpha", "what did I say"]);
        assert_eq!(assistants.len(), 2);
        assert_eq!(assistants[0], "alpha noted");
        assert_eq!(assistants[1], "you said alpha");
        no_more(&server);
    }
}

#[test]
fn session_file_command_expansion_survives_resume() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();
        let template = install_command(
            home.path(),
            "review.md",
            "---\ndescription: Review source\n---\nReview $1 first.\n",
        );

        let mut cli = start_configured(api, &server, home.path(), project.path(), &[]);
        // Scanning the directory and rendering help never creates a session.
        cli.input("/help\n");
        cli.wait_for(Pipe::Err, "/review [arguments] — Review source");
        assert!(session_ids(home.path()).is_empty());
        cli.input("/review auth\n");
        let (request, body) = api.receive(&server);
        let (users, _) = api.turns(&body);
        assert_eq!(users, ["Review auth first.\n"]);
        api.final_reply(request, "first noted");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"first noted\n");
        let id = only_session_id(home.path());
        let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));

        // The template changes on disk after the session was saved.
        fs::write(
            &template,
            "---\ndescription: Review source\n---\nReview $1 second.\n",
        )
        .unwrap();

        // Resuming from another cwd restores without any HTTP traffic.
        let mut cli = start_configured(api, &server, home.path(), launch.path(), &["-r", &id]);
        cli.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
        no_more(&server);
        assert_eq!(only_session_id(home.path()), id);

        // A plain task keeps the previously expanded prompt in history.
        cli.input("plain task\n");
        let (request, body) = api.receive(&server);
        let (users, assistants) = api.turns(&body);
        assert_eq!(users, ["Review auth first.\n", "plain task"]);
        assert_eq!(assistants, ["first noted"]);
        api.final_reply(request, "plain noted");
        cli.wait_for(Pipe::Out, "plain noted");

        // A new invocation expands with the current template body.
        cli.input("/review auth\n");
        let (request, body) = api.receive(&server);
        let (users, assistants) = api.turns(&body);
        assert_eq!(
            users,
            [
                "Review auth first.\n",
                "plain task",
                "Review auth second.\n"
            ]
        );
        assert_eq!(assistants, ["first noted", "plain noted"]);
        api.final_reply(request, "second noted");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"plain noted\nsecond noted\n");

        // The saved log rebuilds to exactly these user texts.
        let saved = records(&path);
        assert_session_contract(&saved, &id, project.path());
        let (users, assistants) = api.turns(&rebuild_request(&saved));
        assert_eq!(
            users,
            [
                "Review auth first.\n",
                "plain task",
                "Review auth second.\n"
            ]
        );
        assert_eq!(assistants, ["first noted", "plain noted", "second noted"]);
        no_more(&server);
    }
}

#[test]
fn session_continue_sends_exactly_the_saved_native_request() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();

        // Save a session containing a completed write+read tool round trip
        // (and opaque reasoning for Responses).
        let cli = start_configured(
            api,
            &server,
            home.path(),
            project.path(),
            &["-p", "create and verify a file"],
        );
        let (request, _) = api.receive(&server);
        let write_call = api.call(
            "w",
            "write",
            json!({"path":"marker.txt","content":"tool evidence"}),
        );
        let read_call = api.call("r", "read", json!({"path":"marker.txt"}));
        let batch = match api {
            Api::Chat => json!([write_call, read_call]),
            Api::Responses => json!([
                {
                    "type":"reasoning","id":"rs-session",
                    "summary":[{"type":"summary_text","text":"plan the writes"}],
                    "content":[{"type":"reasoning_text","text":"plan"}],
                    "encrypted_content":"opaque-session-reasoning","status":"completed"
                },
                write_call, read_call
            ]),
        };
        match api {
            Api::Chat => reply(
                request,
                json!({
                    "role":"assistant", "content":null,
                    "reasoning_content":"plan the writes", "tool_calls":batch
                }),
            ),
            Api::Responses => api.calls_reply(request, batch),
        }
        let (request, body) = api.receive(&server);
        assert_eq!(api.tool_result(&body, "w")["bytes_written"], 13);
        assert_eq!(api.tool_result(&body, "r")["content"], "tool evidence");
        api.final_reply(request, "file done");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"file done\n");
        assert_eq!(
            fs::read_to_string(project.path().join("marker.txt")).unwrap(),
            "tool evidence"
        );
        let id = only_session_id(home.path());
        let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
        let saved = records(&path);
        assert_session_contract(&saved, &id, project.path());
        let original_messages = canonical_messages(&saved);
        assert_eq!(
            original_messages
                .iter()
                .map(|message| message["role"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["user", "assistant", "toolResult", "toolResult", "assistant"]
        );
        let assistant = &original_messages[1];
        assert_eq!(
            assistant["api"],
            if api == Api::Chat {
                "openai-completions"
            } else {
                "openai-responses"
            }
        );
        assert_eq!(assistant["provider"], "fixture");
        assert_eq!(assistant["model"], "fixture-model");
        assert_eq!(assistant["stopReason"], "toolUse");
        let content = assistant["content"].as_array().unwrap();
        let thinking: Vec<_> = content
            .iter()
            .filter(|part| part["type"] == "thinking")
            .collect();
        if api == Api::Chat {
            assert_eq!(thinking.len(), 1);
            assert_eq!(thinking[0]["thinking"], "plan the writes");
            assert!(thinking[0].get("summary").is_none());
        } else {
            assert_eq!(thinking.len(), 2);
            assert_eq!(thinking[0]["thinking"], "plan the writes");
            assert_eq!(thinking[0]["summary"], true);
            assert_eq!(thinking[1]["thinking"], "plan");
            assert_eq!(thinking[1]["summary"], false);
        }
        let calls: Vec<_> = content
            .iter()
            .filter(|part| part["type"] == "toolCall")
            .collect();
        assert_eq!(calls.len(), 2);
        for (index, (id, name, arguments)) in [
            (
                "w",
                "write",
                json!({"path":"marker.txt","content":"tool evidence"}),
            ),
            ("r", "read", json!({"path":"marker.txt"})),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                *calls[index],
                json!({"type":"toolCall","id":id,"name":name,"arguments":arguments})
            );
            let tool = &original_messages[index + 2];
            assert_eq!(tool["toolCallId"], id);
            assert_eq!(tool["toolName"], name);
            assert_eq!(tool["isError"], false);
            let raw = tool["content"][0]["text"].as_str().unwrap();
            assert_eq!(tool["content"][0]["type"], "text");
            let details: Value = serde_json::from_str(raw).unwrap();
            assert_eq!(tool["details"], details);
            assert_eq!(details["ok"], true);
            if index == 0 {
                assert_eq!(details["bytes_written"], 13);
            } else {
                assert_eq!(details["content"], "tool evidence");
            }
        }
        assert_eq!(
            original_messages[4]["content"],
            json!([{"type":"text","text":"file done"}])
        );
        assert!(
            !serde_json::to_string(&original_messages)
                .unwrap()
                .contains("opaque-session-reasoning")
        );
        if api == Api::Responses {
            let opaque: Vec<_> = saved
                .iter()
                .filter(|record| record.to_string().contains("opaque-session-reasoning"))
                .collect();
            assert!(!opaque.is_empty());
            assert!(
                opaque
                    .iter()
                    .all(|record| record["customType"] == "pp.native_append")
            );
        }
        let rebuilt = rebuild_request(&saved);

        // Remove the tool's file: if restoring replayed old tool calls the
        // file would reappear before any new model answer.
        fs::remove_file(project.path().join("marker.txt")).unwrap();

        let mut cli = start_configured(api, &server, home.path(), launch.path(), &["-c", &id]);
        cli.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
        let (request, body) = api.receive(&server);
        // The continued request is the saved native request rebuilt from the
        // JSONL (instructions, think level, checkpoints and appended slices
        // included): no new user message, no "please continue" text, and the
        // last completed assistant output present.
        assert_eq!(
            body, rebuilt,
            "continued request must equal the saved native request"
        );
        let (users, assistants) = api.turns(&body);
        assert_eq!(users, ["create and verify a file"]);
        assert_eq!(assistants, ["file done"]);
        if api == Api::Responses {
            let input = body["input"].as_array().unwrap();
            let reasoning = input
                .iter()
                .find(|item| item["type"] == "reasoning")
                .expect("reasoning item must survive restore");
            assert_eq!(reasoning["encrypted_content"], "opaque-session-reasoning");
            assert_eq!(reasoning["id"], "rs-session");
            let call = input
                .iter()
                .find(|item| item["type"] == "function_call" && item["call_id"] == "w")
                .expect("function call must keep its call_id");
            assert_eq!(call["call_id"], "w");
            assert_eq!(body["store"], false);
            assert!(
                body["include"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| { value.as_str() == Some("reasoning.encrypted_content") })
            );
        }
        // Restoring executes no old tools.
        assert!(!project.path().join("marker.txt").exists());

        // A new tool call from the model does execute after restore, in the
        // saved working directory rather than the launch directory.
        api.calls_reply(
            request,
            json!([api.call(
                "w2",
                "write",
                json!({"path":"second.txt","content":"after restore"})
            )]),
        );
        let (request, body) = api.receive(&server);
        assert_eq!(api.tool_result(&body, "w2")["bytes_written"], 13);
        api.final_reply(request, "done after restore");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(project.path().join("second.txt")).unwrap(),
            "after restore"
        );
        assert!(!launch.path().join("second.txt").exists());
        assert!(!launch.path().join("marker.txt").exists());
        // Continue adds no user projection and never reprojects old native history.
        let continued = records(&path);
        assert_session_contract(&continued, &id, project.path());
        assert_eq!(&continued[..saved.len()], saved.as_slice());
        let messages = canonical_messages(&continued);
        assert_eq!(
            &messages[..original_messages.len()],
            original_messages.as_slice()
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| message["role"] == "user")
                .count(),
            1
        );
        assert!(
            messages[original_messages.len()..]
                .iter()
                .all(|message| { !message["content"].to_string().contains("plan the writes") }),
            "thinking buffers must not leak into subsequent turns"
        );
        // Still the same session file: continuing never forks.
        assert_eq!(only_session_id(home.path()), id);
        no_more(&server);
    }
}

#[test]
fn session_continue_selects_latest_and_restores_original_cwd() {
    let server = server();
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project_a = root.path().join("a");
    let project_b = root.path().join("b");
    let launch = root.path().join("launch");
    for dir in [&home, &project_a, &project_b, &launch] {
        fs::create_dir_all(dir).unwrap();
    }
    configure(&home, fixture_settings(), &fixture_models(&server));

    // Two sessions in two different projects.
    for (project, prompt, answer) in [
        (&project_a, "project alpha", "alpha noted"),
        (&project_b, "project beta", "beta noted"),
    ] {
        let cli = start(&home, project, &["-p", prompt]);
        let (request, _) = receive(&server);
        final_reply(request, answer);
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(session_ids(&home).len(), 2);
    let id_of = |text: &str| {
        session_ids(&home)
            .into_iter()
            .find(|id| {
                fs::read_to_string(sessions_dir(&home).join(format!("{id}.jsonl")))
                    .unwrap()
                    .contains(text)
            })
            .unwrap_or_else(|| panic!("no session contains {text}"))
    };
    let id_a = id_of("project alpha");
    let id_b = id_of("project beta");
    assert_ne!(id_a, id_b);
    let path_b = sessions_dir(&home).join(format!("{id_b}.jsonl"));

    // Merely viewing the newest session must not bump its modification time.
    let mtime_before = fs::metadata(&path_b).unwrap().modified().unwrap();
    let mut viewer = start(&home, &launch, &["-r", &id_b]);
    viewer.wait_for(Pipe::Err, &format!("Resumed session: {id_b}"));
    viewer.input("/quit\n");
    viewer.finish();
    assert_eq!(
        fs::metadata(&path_b).unwrap().modified().unwrap(),
        mtime_before,
        "viewing a session must not change its modification time"
    );

    // Bare -c still selects B (last actually persisted modification).
    let mut cli = start(&home, &launch, &["-c"]);
    cli.wait_for(Pipe::Err, &format!("Resumed session: {id_b}"));
    let (request, body) = receive(&server);
    let (users, _) = Api::Chat.turns(&body);
    assert_eq!(users, ["project beta"]);
    assert!(!body.to_string().contains("project alpha"));
    final_reply(request, "beta continued");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // A new message in A makes A the latest.
    let mut cli = start(&home, &launch, &["-r", &id_a]);
    cli.wait_for(Pipe::Err, &format!("Resumed session: {id_a}"));
    cli.input("update alpha\n");
    let (request, _) = receive(&server);
    final_reply(request, "alpha updated");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Bare -c now selects A and runs in A's saved working directory: the
    // marker file written by a real tool call lands in A, not the launch
    // directory and not B.
    let mut cli = start(&home, &launch, &["-c"]);
    cli.wait_for(Pipe::Err, &format!("Resumed session: {id_a}"));
    let (request, body) = receive(&server);
    let (users, assistants) = Api::Chat.turns(&body);
    assert_eq!(users, ["project alpha", "update alpha"]);
    assert_eq!(assistants, ["alpha noted", "alpha updated"]);
    assert!(!body.to_string().contains("project beta"));
    calls_reply(
        request,
        json!([call(
            "m",
            "write",
            json!({"path":"marker-after-continue","content":"landed"})
        )]),
    );
    let (request, body) = receive(&server);
    assert_eq!(Api::Chat.tool_result(&body, "m")["bytes_written"], 6);
    final_reply(request, "written");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(project_a.join("marker-after-continue")).unwrap(),
        "landed"
    );
    assert!(!launch.join("marker-after-continue").exists());
    assert!(!project_b.join("marker-after-continue").exists());

    // An explicit ID wins over recency.
    let mut cli = start(&home, &launch, &["-c", &id_b]);
    cli.wait_for(Pipe::Err, &format!("Resumed session: {id_b}"));
    let (request, body) = receive(&server);
    let (users, _) = Api::Chat.turns(&body);
    assert_eq!(users, ["project beta"]);
    final_reply(request, "beta again");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Session files live only under HOME/.pp/agent/sessions.
    assert_eq!(session_ids(&home).len(), 2);
    for dir in [&home, &project_a, &project_b, &launch] {
        if dir != &home {
            assert!(!dir.join(".pp").exists(), "no session data outside HOME");
        }
    }

    // A deleted original working directory fails the restore with zero HTTP.
    fs::remove_dir_all(&project_a).unwrap();
    expect_restore_failure(
        &server,
        &home,
        &launch,
        &["-r", &id_a, "-p", "hello"],
        &["restoring session working directory"],
        &["fixture-key"],
    );
}

#[test]
fn session_resume_prompt_conflicts_and_reset_boundaries() {
    let server = server();

    // clap rejects -p over -c, -r together with -c, and -r without a value;
    // none of them may silently drop the resume arguments.
    {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        configure(home.path(), fixture_settings(), &fixture_models(&server));
        let valid = "2026-10-17T01-02-03-00000000000000000000000000000000";
        for args in [
            vec!["-c", "-p", "text"],
            vec!["-r", valid, "-c"],
            vec!["-r"],
        ] {
            let cli = start(home.path(), cwd.path(), &args);
            let output = cli.finish();
            assert!(
                !output.status.success(),
                "clap must reject {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            no_more(&server);
        }
        assert!(session_ids(home.path()).is_empty());
    }

    // -r with -p appends the new prompt once, keeping saved context.
    for api in APIS {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();
        let (id, _path) = make_saved_session(
            api,
            &server,
            home.path(),
            project.path(),
            "remember old",
            "old noted",
        );
        let cli = start_configured(
            api,
            &server,
            home.path(),
            launch.path(),
            &["-r", &id, "-p", "brand new"],
        );
        let (request, body) = api.receive(&server);
        let (users, assistants) = api.turns(&body);
        assert_eq!(users, ["remember old", "brand new"]);
        assert_eq!(assistants, ["old noted"]);
        api.final_reply(request, "new done");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"new done\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&format!("Resumed session: {id}")));
        assert_eq!(only_session_id(home.path()), id);
    }

    // /new on a persisted session discards the active journal without
    // creating an empty session; the next prompt starts a new ID, and the
    // old session remains resumable.
    {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let (old_id, _path) = make_saved_session(
            Api::Chat,
            &server,
            home.path(),
            project.path(),
            "first task",
            "first answer",
        );
        let mut cli = start_configured(Api::Chat, &server, home.path(), project.path(), &[]);
        cli.input("/new\n");
        cli.wait_for(Pipe::Err, "Session reset.");
        assert_eq!(
            session_ids(home.path()),
            vec![old_id.clone()],
            "/new must not create an empty session file"
        );
        cli.input("fresh start\n");
        let (request, body) = receive(&server);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "fresh start");
        assert!(!body.to_string().contains("first task"));
        final_reply(request, "fresh answer");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let ids = session_ids(home.path());
        assert_eq!(ids.len(), 2, "next prompt creates a new session ID");
        let new_id = ids.iter().find(|id| **id != old_id).unwrap();
        // The old session is untouched and still resumable.
        let cli = start_configured(
            Api::Chat,
            &server,
            home.path(),
            project.path(),
            &["-r", &old_id, "-p", "recall"],
        );
        let (request, body) = receive(&server);
        let (users, assistants) = Api::Chat.turns(&body);
        assert_eq!(users, ["first task", "recall"]);
        assert_eq!(assistants, ["first answer"]);
        assert!(!body.to_string().contains("fresh start"));
        final_reply(request, "recalled");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"recalled\n");
        assert_ne!(*new_id, old_id);
        no_more(&server);
    }
}

#[test]
fn session_failures_keep_executed_tool_results_and_display_only_fragments() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();

        let mut cli = start_configured(api, &server, home.path(), project.path(), &[]);
        cli.input("use the tools\n");
        let (request, _) = api.receive(&server);
        api.calls_reply(
            request,
            json!([api.call("w", "write", json!({"path":"kept.txt","content":"kept"}))]),
        );
        let (request, body) = api.receive(&server);
        assert_eq!(api.tool_result(&body, "w")["bytes_written"], 4);
        // The next model answer fails after streaming a partial fragment.
        let mut stream = SseReply::new(request);
        match api {
            Api::Chat => {
                stream.event(chat_chunk(
                    json!({"role":"assistant","content":"partial fragment"}),
                    None,
                ));
                stream.event(json!({"error":{
                    "message":"private-provider-body fixture-key",
                    "type":"server_error","code":"private-provider-body"
                }}));
                stream.finish();
            }
            Api::Responses => {
                stream.event(json!({
                    "type":"response.output_text.delta","sequence_number":0,
                    "item_id":"msg-x","output_index":0,"content_index":0,
                    "delta":"partial fragment","logprobs":[]
                }));
                // End HTTP before response.completed: a complete transport with an interrupted protocol turn.
                stream.finish();
            }
        }
        let output = cli.finish();
        assert!(
            output.status.success(),
            "line mode must survive a task error: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("partial fragment"));
        assert!(!stderr.contains("private-provider-body"));
        assert!(!stderr.contains("fixture-key"));

        let id = only_session_id(home.path());
        let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
        let file_text = fs::read_to_string(&path).unwrap();
        // The partial fragment is display log only: present as an event
        // record, absent from the rebuilt native history.
        assert!(file_text.contains("partial fragment"));
        let saved = records(&path);
        assert_session_contract(&saved, &id, project.path());
        assert!(
            !serde_json::to_string(&canonical_messages(&saved))
                .unwrap()
                .contains("partial fragment")
        );
        assert!(
            records(&path)
                .iter()
                .any(|record| record["customType"] == "pp.event")
        );
        assert!(
            records(&path)
                .iter()
                .any(|record| record["customType"] == "pp.error")
        );
        assert!(!records(&path).iter().any(|record| {
            record["customType"] == "pp.error"
                && record["data"]["message"]
                    .to_string()
                    .contains("private-provider-body")
        }));
        assert!(
            !rebuild_request(&records(&path))
                .to_string()
                .contains("partial fragment")
        );
        assert!(
            fs::read_to_string(project.path().join("kept.txt"))
                .is_ok_and(|content| content == "kept")
        );

        // A fully written readable projection followed by a killed native
        // commit must still not become model context. Long nonempty entry
        // IDs are accepted on load even though pp generates eight hex digits.
        with_mutated_records(&path, |records| {
            push_entry(
                records,
                json!({
                    "type":"message",
                    "message":{
                        "role":"assistant", "api":if api == Api::Chat { "openai-completions" } else { "openai-responses" },
                        "provider":"fixture", "model":"fixture-model",
                        "content":[{"type":"text","text":"uncommitted projection"}],
                        "stopReason":"stop", "timestamp":1792224000000_i64
                    }
                }),
            );
        });
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":\"custom\",\"customType\":\"pp.native_append\",\"data\":")
            .unwrap();
        // After restart the executed tool result, but neither the streamed
        // fragment nor the uncommitted canonical answer, is native history.
        let cli = start_configured(
            api,
            &server,
            home.path(),
            launch.path(),
            &["-r", &id, "-p", "what happened"],
        );
        let (request, body) = api.receive(&server);
        assert_eq!(api.tool_result(&body, "w")["bytes_written"], 4);
        let (users, _) = api.turns(&body);
        assert_eq!(users, ["use the tools", "what happened"]);
        assert!(!body.to_string().contains("partial fragment"));
        assert!(!body.to_string().contains("uncommitted projection"));
        api.final_reply(request, "tools ran");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"tools ran\n");
        no_more(&server);
    }
}

#[test]
fn session_broken_stdout_still_persists_executed_tool_results() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    configure(home.path(), fixture_settings(), &fixture_models(&server));
    let mut cli = Cli(Some(
        command(
            home.path(),
            project.path(),
            &["-p", "record with closed display"],
        )
        .spawn()
        .unwrap(),
    ));
    drop(cli.0.as_mut().unwrap().stdout.take());
    let (request, _) = receive(&server);
    calls_reply(
        request,
        json!([call(
            "write",
            "write",
            json!({"path":"saved-record","content":"record me"})
        )]),
    );
    let (request, body) = receive(&server);
    assert_eq!(result(&body["messages"][3], "write")["bytes_written"], 9);
    final_reply(request, "finished history");
    let output = cli.finish();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("writing task output"));
    assert_eq!(
        fs::read_to_string(project.path().join("saved-record")).unwrap(),
        "record me"
    );
    no_more(&server);

    // The executed tool result survives the broken display and is native
    // history for the next run.
    let id = only_session_id(home.path());
    let cli = start_configured(
        Api::Chat,
        &server,
        home.path(),
        launch.path(),
        &["-r", &id, "-p", "after broken display"],
    );
    let (request, body) = receive(&server);
    assert_eq!(result(&body["messages"][3], "write")["bytes_written"], 9);
    final_reply(request, "recovered");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"recovered\n");
    no_more(&server);
}

#[test]
fn session_loader_rejects_corrupt_or_unavailable_logs() {
    let server = server();
    let cwd = tempfile::tempdir().unwrap();

    // A well-formed but absent ID fails to open the file.
    {
        let home = tempfile::tempdir().unwrap();
        configure(home.path(), fixture_settings(), &fixture_models(&server));
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &[
                "-r",
                "2026-10-17T01-02-03-00000000000000000000000000000000",
                "-p",
                "hi",
            ],
            &["opening saved session"],
            &["fixture-key"],
        );
        // Malformed and traversal IDs are rejected before any filesystem
        // access.
        for id in [
            "../escape",
            "id.jsonl",
            "back\\slash",
            "2026-10-17T01-02-03-0123456789ABCDEF0123456789ABCDEF",
            "2026-10-17T01-02-03-0123456789abcdef0123456789abcde",
            "2026-10-17T01-02-03",
            "2026-10-17T01-02-03-0123456789abcdef0123456789abcdeg",
            "2026-02-29T01-02-03-0123456789abcdef0123456789abcdef",
            "1792195200000-0123456789abcdef0123456789abcdef",
        ] {
            expect_restore_failure(
                &server,
                home.path(),
                cwd.path(),
                &["-r", id, "-p", "hi"],
                &["invalid session ID"],
                &["fixture-key"],
            );
        }
        // -c validates a supplied ID the same way.
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-c", "nope"],
            &["invalid session ID"],
            &["fixture-key"],
        );
        assert!(session_ids(home.path()).is_empty());
    }

    // Empty and missing session directories hold nothing to continue.
    {
        let home = tempfile::tempdir().unwrap();
        configure(home.path(), fixture_settings(), &fixture_models(&server));
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-c"],
            &["no saved sessions found"],
            &["fixture-key"],
        );
        let empty = tempfile::tempdir().unwrap();
        configure(empty.path(), fixture_settings(), &fixture_models(&server));
        fs::create_dir_all(sessions_dir(empty.path())).unwrap();
        expect_restore_failure(
            &server,
            empty.path(),
            cwd.path(),
            &["-c"],
            &["no saved sessions found"],
            &["fixture-key"],
        );
    }

    // Empty, non-header, and explicitly unsupported legacy files.
    {
        let home = tempfile::tempdir().unwrap();
        configure(home.path(), fixture_settings(), &fixture_models(&server));
        fs::create_dir_all(sessions_dir(home.path())).unwrap();
        let id = "2026-10-17T01-02-03-00000000000000000000000000000000";
        fs::write(sessions_dir(home.path()).join(format!("{id}.jsonl")), "").unwrap();
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", id, "-p", "hi"],
            &["missing session header"],
            &["fixture-key"],
        );
        fs::write(
            sessions_dir(home.path()).join(format!("{id}.jsonl")),
            "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":\"x\",\"timestamp\":0}}\n",
        )
        .unwrap();
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", id, "-p", "hi"],
            &["first record must be a session header"],
            &["fixture-key"],
        );
        fs::write(
            sessions_dir(home.path()).join(format!("{id}.jsonl")),
            "{\"type\":\"checkpoint\",\"version\":1,\"secret\":\"fixture-key\"}\n",
        )
        .unwrap();
        for args in [vec!["-r", id, "-p", "hi"], vec!["-c"]] {
            expect_restore_failure(
                &server,
                home.path(),
                cwd.path(),
                &args,
                &["unsupported legacy session format"],
                &["fixture-key"],
            );
        }
    }

    // Structural corruption of otherwise valid session logs: every case is a
    // fresh session, mutated, and expected to fail without touching files.
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            records[0]["version"] = json!(2)
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["unsupported session version"],
            &["fixture-key"],
        );
    }
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            records[0]["id"] = json!("2026-10-17T01-02-04-00000000000000000000000000000000");
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["session ID does not match filename"],
            &["fixture-key"],
        );
    }
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            session_state_mut(records)["toolLimits"]["read_max_bytes"] = json!(0);
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["read_max_bytes must be greater than zero"],
            &["fixture-key"],
        );
    }
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            push_entry(
                records,
                json!({"type":"custom","customType":"pp.native_append","data":{"protocol":"response","items":[]}}),
            );
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["append protocol mismatch"],
            &["fixture-key"],
        );
    }
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            session_state_mut(records)["session"]["request"]["messages"]
                .as_array_mut()
                .unwrap()
                .remove(0);
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["Completion first message must be system"],
            &["fixture-key"],
        );
    }
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            records[0]["cwd"] = json!("relative")
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["session working directory must be absolute"],
            &["fixture-key"],
        );
    }
    // Unknown record type: rejected without echoing the record body.
    {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            push_entry(
                records,
                json!({"type":"mystery","secret-record-body":"fixture-key"}),
            );
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["invalid session record"],
            &["secret-record-body", "fixture-key"],
        );
    }
    // Every semantic mutation starts with a valid generated file. Added rows
    // get real chain envelopes, and intentional reordering is relinked.
    type MutationCase = (&'static str, fn(&mut Vec<Value>));
    let cases: &[MutationCase] = &[
        ("unsupported pp session version", |records| {
            session_state_mut(records)["version"] = json!(2);
        }),
        ("missing pp session state", |records| {
            records.truncate(1);
        }),
        ("invalid session entry chain", |records| {
            records[1]["parentId"] = json!("not-the-parent");
        }),
        ("invalid session entry chain", |records| {
            records[2]["parentId"] = Value::Null;
        }),
        ("invalid session entry chain", |records| {
            records[2]["id"] = records[1]["id"].clone();
        }),
        ("invalid session entry chain", |records| {
            records[1]["id"] = json!("");
        }),
        ("invalid session record", |records| {
            records[1].as_object_mut().unwrap().remove("id");
        }),
        ("invalid session", |records| {
            records[1].as_object_mut().unwrap().remove("parentId");
        }),
        ("invalid session", |records| {
            records[2].as_object_mut().unwrap().remove("parentId");
        }),
        ("invalid session record", |records| {
            push_entry(
                records,
                json!({"type":"custom","customType":"pp.future","data":{"secret-record-body":"fixture-key"}}),
            );
        }),
        ("invalid session record", |records| {
            push_entry(
                records,
                json!({"type":"session","version":3,"cwd":"/","secret-record-body":"fixture-key"}),
            );
        }),
        ("invalid session record", |records| {
            records[1].as_object_mut().unwrap().remove("timestamp");
        }),
        ("invalid session record", |records| {
            records[1]["timestamp"] = json!("not-a-timestamp fixture-key");
        }),
        ("invalid session record", |records| {
            records[0]["timestamp"] = json!("not-a-timestamp fixture-key");
        }),
        ("invalid session record", |records| {
            records[0]["timestamp"] = json!("1969-12-31T23:59:59.999Z");
        }),
    ];
    for &(expected, mutate) in cases {
        let (home, id) = corrupted_base(&server, cwd.path(), mutate);
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &[expected],
            &["secret-record-body", "fixture-key"],
        );
    }
    // State-dependent records must not precede the first native snapshot.
    for entry in [
        json!({"type":"thinking_level_change","thinkingLevel":"high"}),
        json!({"type":"custom","customType":"pp.instructions","data":{"system":"x"}}),
        json!({"type":"custom","customType":"pp.native_append","data":{"protocol":"completion","messages":[]}}),
    ] {
        let (home, id) = corrupted_base(&server, cwd.path(), |records| {
            push_entry(records, entry);
            let entry = records.pop().unwrap();
            records.insert(1, entry);
            relink_entries(records);
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["before"],
            &["fixture-key"],
        );
    }
    // A complete (newline-terminated) invalid JSON line in the middle names
    // its line number and leaks nothing.
    {
        let home = tempfile::tempdir().unwrap();
        let (id, path) = make_saved_session(
            Api::Chat,
            &server,
            home.path(),
            cwd.path(),
            "remember base",
            "base noted",
        );
        insert_raw_line(&path, 2, "not-json secret-record-body fixture-key");
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["invalid session record", "line 3"],
            &["secret-record-body", "fixture-key"],
        );
    }

    // A Responses checkpoint whose input is not an item array is corrupt.
    {
        let home = tempfile::tempdir().unwrap();
        let (id, path) = make_saved_session(
            Api::Responses,
            &server,
            home.path(),
            cwd.path(),
            "remember base",
            "base noted",
        );
        with_mutated_records(&path, |records| {
            session_state_mut(records)["session"]["request"]["input"] = json!("plain text");
        });
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["Responses input must be items"],
            &["fixture-key"],
        );
    }

    // The saved model must resolve to the same protocol and model string.
    {
        let home = tempfile::tempdir().unwrap();
        let (id, _path) = make_saved_session(
            Api::Chat,
            &server,
            home.path(),
            cwd.path(),
            "remember base",
            "base noted",
        );
        let variants = [
            // Reference gone from the catalog.
            json!({"providers":[slash_provider(
                &server, "other", "completion",
                json!([{"id":"primary","model":"fixture-model"}])
            )]})
            .to_string(),
            // Same reference, different model string.
            json!({"providers":[slash_provider(
                &server, "fixture", "completion",
                json!([{"id":"primary","model":"changed-model"}])
            )]})
            .to_string(),
            // Same reference, different protocol.
            json!({"providers":[slash_provider(
                &server, "fixture", "response",
                json!([{"id":"primary","model":"fixture-model"}])
            )]})
            .to_string(),
        ];
        for models in variants {
            let other = tempfile::tempdir().unwrap();
            copy_session(home.path(), other.path(), &id);
            configure(other.path(), fixture_settings(), &models);
            expect_restore_failure(
                &server,
                other.path(),
                cwd.path(),
                &["-r", &id, "-p", "hi"],
                &["saved session model is unavailable or has changed"],
                &["fixture-key"],
            );
        }
    }

    // A session held by another process is explicitly in use.
    {
        let home = tempfile::tempdir().unwrap();
        let (id, _path) = make_saved_session(
            Api::Chat,
            &server,
            home.path(),
            cwd.path(),
            "remember base",
            "base noted",
        );
        let mut holder =
            start_configured(Api::Chat, &server, home.path(), cwd.path(), &["-r", &id]);
        holder.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["session is already in use"],
            &["fixture-key"],
        );
        holder.input("/quit\n");
        holder.finish();
    }

    // Session files are never followed through symlinks.
    {
        let home = tempfile::tempdir().unwrap();
        let (id, path) = make_saved_session(
            Api::Chat,
            &server,
            home.path(),
            cwd.path(),
            "remember base",
            "base noted",
        );
        let real = home.path().join("elsewhere.jsonl");
        fs::rename(&path, &real).unwrap();
        symlink(&real, &path).unwrap();
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id, "-p", "hi"],
            &["opening saved session"],
            &["fixture-key"],
        );
        assert!(fs::read_to_string(&real).is_ok_and(|content| content.contains("remember base")));
    }
}

#[test]
fn session_truncated_tail_is_ignored_and_repaired_on_first_append() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    let (id, path) = make_saved_session(
        Api::Chat,
        &server,
        home.path(),
        project.path(),
        "remember tail",
        "tail noted",
    );
    let complete = fs::read(&path).unwrap();
    let partial = b"{\"type\":\"message\",\"id\":\"unfin";
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(partial)
        .unwrap();

    // Loading tolerates the incomplete tail and does not modify the file.
    let mut cli = start_configured(Api::Chat, &server, home.path(), launch.path(), &["-r", &id]);
    cli.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
    let stderr = cli.text(Pipe::Err);
    assert!(
        stderr.contains("Ignoring incomplete final record"),
        "{stderr}"
    );
    assert_eq!(
        fs::read(&path).unwrap().len(),
        complete.len() + partial.len()
    );

    // The first real append truncates the partial tail in place.
    cli.input("after repair\n");
    let (request, body) = receive(&server);
    let (users, assistants) = Api::Chat.turns(&body);
    assert_eq!(users, ["remember tail", "after repair"]);
    assert_eq!(assistants, ["tail noted"]);
    final_reply(request, "repaired answer");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let after = fs::read(&path).unwrap();
    assert!(
        after.starts_with(&complete),
        "repair keeps committed records"
    );
    assert!(!String::from_utf8_lossy(&after).contains("unfin"));
    assert!(after.ends_with(b"\n"));
    assert_session_contract(&records(&path), &id, project.path());

    // The repaired file loads again without any incomplete-tail notice.
    let cli = start_configured(
        Api::Chat,
        &server,
        home.path(),
        launch.path(),
        &["-r", &id, "-p", "again"],
    );
    let (request, body) = receive(&server);
    let (users, assistants) = Api::Chat.turns(&body);
    assert_eq!(users, ["remember tail", "after repair", "again"]);
    assert_eq!(assistants, ["tail noted", "repaired answer"]);
    final_reply(request, "done again");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Ignoring incomplete"));
    assert_eq!(output.stdout, b"done again\n");
    assert_eq!(only_session_id(home.path()), id);
    no_more(&server);
}

#[test]
fn session_settings_persist_across_model_switch_and_restore() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let launch = tempfile::tempdir().unwrap();
    let models = json!({"providers":[
        slash_provider(&server, "a", "completion", json!([
            {"id":"primary","model":"a-one"},{"id":"secondary","model":"a-two"}
        ])),
        slash_provider(&server, "b", "response", json!([{"id":"primary","model":"b-one"}]))
    ]})
    .to_string();
    configure(home.path(), r#"{"default_model":"a/primary"}"#, &models);

    let mut cli = start(home.path(), project.path(), &[]);
    cli.input("first\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "a-one");
    Api::Chat.assert_level(&body, "none");
    final_reply(request, "first answer");
    cli.input("/effort max\n/model a/secondary\n");
    cli.wait_for(Pipe::Err, "Current model: a/secondary (a-two)");
    cli.input("second\n");
    let (request, body) = receive(&server);
    assert_eq!(body["model"], "a-two");
    Api::Chat.assert_level(&body, "max");
    let (users, assistants) = Api::Chat.turns(&body);
    assert_eq!(users, ["first", "second"]);
    assert_eq!(assistants, ["first answer"]);
    final_reply(request, "second answer");
    cli.input("/quit\n");
    let first_output = cli.finish();
    assert!(
        first_output.status.success(),
        "{}",
        String::from_utf8_lossy(&first_output.stderr)
    );
    let id = only_session_id(home.path());
    let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
    let same_protocol = records(&path);
    assert_session_contract(&same_protocol, &id, project.path());
    let saved_request = rebuild_request(&same_protocol);
    assert_eq!(saved_request["model"], "a-two");
    Api::Chat.assert_level(&saved_request, "max");
    let same_protocol_messages = canonical_messages(&same_protocol);
    assert_eq!(same_protocol_messages.len(), 4);
    let mut cli = start(home.path(), launch.path(), &["-r", &id]);
    cli.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
    no_more(&server);
    assert_eq!(records(&path), same_protocol);
    cli.input("/model b/primary\n");
    cli.wait_for(Pipe::Err, "Current model: b/primary (b-one)");
    cli.input("third\n");
    let (request, body) = receive_at(&server, "/v1/responses");
    assert_eq!(body["model"], "b-one");
    Api::Responses.assert_level(&body, "max");
    let (users, assistants) = Api::Responses.turns(&body);
    assert_eq!(users, ["first", "second", "third"]);
    assert_eq!(assistants, ["first answer", "second answer"]);
    assert!(
        body["instructions"]
            .as_str()
            .unwrap()
            .contains("coding assistant")
    );
    response_final(request, "third answer");
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        [first_output.stdout, output.stdout].concat(),
        b"first answer\nsecond answer\nthird answer\n"
    );

    let saved = records(&path);
    assert_session_contract(&saved, &id, project.path());
    assert_eq!(&saved[..same_protocol.len()], same_protocol.as_slice());
    let messages = canonical_messages(&saved);
    assert_eq!(messages.len(), 6);
    assert_eq!(
        &messages[..same_protocol_messages.len()],
        same_protocol_messages.as_slice()
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "user")
            .map(|message| message["content"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["first", "second", "third"]
    );
    assert_eq!(
        messages
            .iter()
            .filter(|message| message["role"] == "assistant")
            .map(|message| message["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a-one", "a-two", "b-one"]
    );
    assert_eq!(
        saved
            .iter()
            .filter(|record| record["type"] == "model_change")
            .map(|record| record["model"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["a/primary", "a/secondary", "b/primary"]
    );
    assert!(saved.iter().any(
        |record| record["type"] == "thinking_level_change" && record["thinkingLevel"] == "max"
    ));
    assert_eq!(
        saved
            .iter()
            .filter(|record| record["customType"] == "pp.session_state")
            .count(),
        3
    );
    let rebuilt = rebuild_request(&saved);

    // After restart the next request carries the saved model, think level
    // and converted history (replayed from the JSONL, including the
    // cross-protocol checkpoint).
    let cli = start(
        home.path(),
        launch.path(),
        &["-r", &id, "-p", "after restore"],
    );
    let (request, body) = receive_at(&server, "/v1/responses");
    let mut expected = rebuilt;
    let mut new_user = expected["input"][0].clone();
    match &mut new_user["content"] {
        Value::String(text) => *text = "after restore".to_owned(),
        Value::Array(parts) => parts[0]["text"] = json!("after restore"),
        content => panic!("unexpected user content: {content}"),
    }
    expected["input"].as_array_mut().unwrap().push(new_user);
    assert_eq!(
        body, expected,
        "restored request must replay the saved native history"
    );
    assert_eq!(body["model"], "b-one");
    assert_eq!(body["reasoning"]["effort"], "max");
    response_final(request, "restored answer");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, b"restored answer\n");
    let restored = records(&path);
    assert_session_contract(&restored, &id, project.path());
    assert_eq!(&restored[..saved.len()], saved.as_slice());
    let restored_messages = canonical_messages(&restored);
    assert_eq!(&restored_messages[..messages.len()], messages.as_slice());
    assert_eq!(restored_messages.len(), 8);
    assert_eq!(restored_messages[6]["role"], "user");
    assert_eq!(restored_messages[6]["content"], "after restore");

    // Switching settings without an active session writes nothing.
    let idle = tempfile::tempdir().unwrap();
    configure(idle.path(), r#"{"default_model":"a/primary"}"#, &models);
    let mut cli = start(idle.path(), project.path(), &[]);
    cli.input("/effort high\n/model b/primary\n/quit\n");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!sessions_dir(idle.path()).exists());
    no_more(&server);
}

#[test]
fn model_thinking_session_restore_preserves_saved_level_across_config_changes() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();
        let protocol = match api {
            Api::Chat => "completion",
            Api::Responses => "response",
        };
        let configured_models = |default_level: &str, efforts: Value| {
            json!({"providers":[
                slash_provider(&server, "p", protocol, json!([
                    {"id":"plain","model":"m-plain"},
                    {"id":"cfg","model":"m-cfg","thinking":{
                        "efforts":efforts,"defaultLevel":default_level
                    }}
                ]))
            ]})
            .to_string()
        };
        let settings = r#"{"default_model":"p/plain","think_level":"low"}"#;
        configure(
            home.path(),
            settings,
            &configured_models("high", json!(["low", "high"])),
        );

        // Startup on the unconfigured model keeps the global level.
        let mut cli = start(home.path(), project.path(), &[]);
        cli.input("first\n");
        let (request, body) = api.receive(&server);
        assert_eq!(body["model"], "m-plain");
        api.assert_level(&body, "low");
        api.final_reply(request, "first answer");

        // An actual switch applies the target model's explicit default even
        // though the current level is also supported there.
        cli.input("/model p/cfg\n");
        cli.wait_for(Pipe::Err, "Current model: p/cfg (m-cfg)");
        let id = only_session_id(home.path());
        let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
        let switched_request = rebuild_request(&records(&path));
        assert_eq!(switched_request["model"], "m-cfg");
        api.assert_level(&switched_request, "high");
        let before_rejected = fs::read(&path).unwrap();
        cli.input("/effort max\n");
        cli.wait_for(
            Pipe::Err,
            "error: thinking level is not supported by the current model; options: low, high",
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            before_rejected,
            "rejected effort command must not touch the journal"
        );
        cli.input("second\n");
        let (request, body) = api.receive(&server);
        assert_eq!(body["model"], "m-cfg");
        api.assert_level(&body, "high");
        let (users, assistants) = api.turns(&body);
        assert_eq!(users, ["first", "second"]);
        assert_eq!(assistants, ["first answer"]);
        api.final_reply(request, "second answer");
        cli.input("/quit\n");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );

        // The saved native state rebuilds to exactly the model and effort of
        // the real request.
        let saved = records(&path);
        assert_session_contract(&saved, &id, project.path());
        let saved_request = rebuild_request(&saved);
        assert_eq!(saved_request["model"], body["model"]);
        match api {
            Api::Chat => assert_eq!(saved_request["reasoning_effort"], body["reasoning_effort"]),
            Api::Responses => assert_eq!(saved_request["reasoning"], body["reasoning"]),
        }

        // A later defaultLevel change never rewrites the saved level or the
        // replayed native history.
        configure(
            home.path(),
            settings,
            &configured_models("low", json!(["low", "high"])),
        );
        let cli = start(
            home.path(),
            launch.path(),
            &["-r", &id, "-p", "after restore"],
        );
        let (request, body) = api.receive(&server);
        let field = match api {
            Api::Chat => "messages",
            Api::Responses => "input",
        };
        let mut expected = saved_request;
        let mut new_user = expected[field]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["role"] == "user")
            .expect("saved history must contain a user turn")
            .clone();
        match &mut new_user["content"] {
            Value::String(text) => *text = "after restore".to_owned(),
            Value::Array(parts) => parts[0]["text"] = json!("after restore"),
            content => panic!("unexpected user content: {content}"),
        }
        expected[field].as_array_mut().unwrap().push(new_user);
        assert_eq!(
            body, expected,
            "restored request must replay the saved native history and effort"
        );
        assert_eq!(body["model"], "m-cfg");
        api.assert_level(&body, "high");
        api.final_reply(request, "restored answer");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"restored answer\n");
        no_more(&server);

        // Narrowing efforts below the saved level blocks restore before any
        // request or journal write, byte for byte.
        let before_failure = fs::read(&path).unwrap();
        configure(
            home.path(),
            settings,
            &configured_models("medium", json!(["medium", "max"])),
        );
        expect_restore_failure(
            &server,
            home.path(),
            launch.path(),
            &["-r", &id, "-p", "blocked"],
            &["saved session thinking level is not supported by the configured model"],
            &["p-key"],
        );
        assert_eq!(fs::read(&path).unwrap(), before_failure);
    }
}

#[test]
fn session_restore_keeps_saved_edit_mode_and_execution_limits() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let launch = tempfile::tempdir().unwrap();
        fs::write(project.path().join("source.txt"), "alpha\nbeta\ngamma\n").unwrap();
        fs::write(project.path().join("oversized.txt"), "x".repeat(128)).unwrap();
        let models = api.models(&server);
        configure(
            home.path(),
            r#"{"default_model":"fixture/primary","skill_dirs":[],"edit":"hashline","read_max_bytes":64,"read_max_lines":1,"bash_timeout_seconds":1}"#,
            &models,
        );
        let cli = start(home.path(), project.path(), &["-p", "save tool policy"]);
        let (request, _) = api.receive(&server);
        api.final_reply(request, "policy saved");
        assert!(cli.finish().status.success());
        let id = only_session_id(home.path());
        let path = sessions_dir(home.path()).join(format!("{id}.jsonl"));
        let saved = rebuild_request(&records(&path));

        // Current settings must not silently replace the saved tool definitions or execution policy.
        configure(
            home.path(),
            r#"{"default_model":"fixture/primary","skill_dirs":[],"edit":"replace","read_max_bytes":1024,"read_max_lines":10,"bash_timeout_seconds":30}"#,
            &models,
        );
        let cli = start(home.path(), launch.path(), &["-c", &id]);
        let (request, body) = api.receive(&server);
        assert_eq!(body, saved);
        api.calls_reply(
            request,
            json!([
                api.call("read-policy", "read", json!({"path":"source.txt"})),
                api.call("byte-policy", "read", json!({"path":"oversized.txt"})),
                api.call(
                    "timeout-policy",
                    "bash",
                    json!({"command":"sleep 3; printf should-not-appear"})
                )
            ]),
        );
        let (request, body) = api.receive(&server);
        let read = api.tool_result(&body, "read-policy");
        assert_eq!(read["ok"], true);
        assert_eq!(read["lines_read"], 1);
        assert_eq!(read["next_offset"], 2);
        let content = read["content"].as_str().unwrap();
        assert!(content.starts_with("1:"));
        assert!(content.trim_end().ends_with("|alpha"));
        assert!(!content.contains("beta"));
        assert_eq!(api.tool_result(&body, "byte-policy")["ok"], false);
        let bash = api.tool_result(&body, "timeout-policy");
        assert_eq!(bash["timed_out"], true);
        assert_eq!(bash["ok"], false);
        assert!(
            !bash["stdout"]
                .as_str()
                .unwrap()
                .contains("should-not-appear")
        );
        api.final_reply(request, "saved policy enforced");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.stdout, b"saved policy enforced\n");
        let records = records(&path);
        assert_session_contract(&records, &id, project.path());
        let messages = canonical_messages(&records);
        let tools: Vec<_> = messages
            .iter()
            .filter(|message| message["role"] == "toolResult")
            .collect();
        assert_eq!(tools.len(), 3);
        for (index, (call_id, tool_name, is_error)) in [
            ("read-policy", "read", false),
            ("byte-policy", "read", true),
            ("timeout-policy", "bash", true),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(tools[index]["toolCallId"], call_id);
            assert_eq!(tools[index]["toolName"], tool_name);
            assert_eq!(tools[index]["isError"], is_error);
            let raw: Value =
                serde_json::from_str(tools[index]["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(tools[index]["details"], raw);
        }
        no_more(&server);
    }
}

#[test]
fn session_first_create_failure_is_deterministic_without_invented_session() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        configure(home.path(), fixture_settings(), &api.models(&server));
        // A regular file where the sessions directory belongs makes the
        // first create fail deterministically regardless of privileges.
        fs::write(sessions_dir(home.path()), "not a directory").unwrap();
        let cli = start(home.path(), cwd.path(), &["-p", "hello"]);
        let (request, _) = api.receive(&server);
        // Headers are enough to trigger the create-on-open hook; the task
        // aborts before any stream content is consumed.
        let stream = SseReply::new(request);
        let output = cli.finish();
        drop(stream);
        assert!(
            !output.status.success(),
            "task must fail when the journal cannot be created"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("persisting session"), "{stderr}");
        assert!(!stderr.contains("fixture-key"));
        assert!(output.stdout.is_empty());
        no_more(&server);
        assert_eq!(
            fs::read_to_string(sessions_dir(home.path())).unwrap(),
            "not a directory"
        );
        assert!(fs::metadata(sessions_dir(home.path())).unwrap().is_file());
    }
}

#[test]
fn session_file_size_limit_latches_after_executing_whole_tool_batch() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    configure(home.path(), fixture_settings(), &fixture_models(&server));
    // Ignore SIGXFSZ and cap file sizes: the session journal write of a huge
    // streamed delta fails with EFBIG after the checkpoint and user records
    // were already written, without touching the (small) tool files.
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg("trap '' XFSZ; ulimit -f 256; exec \"$@\"")
        .arg("pp")
        .arg(env!("CARGO_BIN_EXE_pp"))
        .args(["-p", "overflow the journal"])
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let cli = Live::new(Cli(Some(command.spawn().unwrap())));
    let (request, _) = receive(&server);
    let big = "A".repeat(400_000);
    reply(
        request,
        json!({
            "role":"assistant","content":big,
            "tool_calls":[
                call("one", "write", json!({"path":"first.txt","content":"one"})),
                call("two", "write", json!({"path":"second.txt","content":"two"}))
            ]
        }),
    );
    let output = cli.finish();
    assert!(
        !output.status.success(),
        "task must fail once persistence latched an error"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("persisting session"), "{stderr}");
    // The display kept streaming while persistence had already failed.
    assert!(output.stdout.len() > 300_000);
    // The accepted whole tool batch executed despite the latch.
    assert_eq!(
        fs::read_to_string(cwd.path().join("first.txt")).unwrap(),
        "one"
    );
    assert_eq!(
        fs::read_to_string(cwd.path().join("second.txt")).unwrap(),
        "two"
    );
    // No next HTTP request after the failed batch commit.
    no_more(&server);
    let id = only_session_id(home.path());
    let bytes = fs::read(sessions_dir(home.path()).join(format!("{id}.jsonl"))).unwrap();
    // The latched write left an unterminated partial record.
    assert!(!bytes.ends_with(b"\n"));
    assert!(String::from_utf8_lossy(&bytes).contains("\"type\":\"session\""));
    assert!(String::from_utf8_lossy(&bytes).contains("\"customType\":\"pp.session_state\""));
}
