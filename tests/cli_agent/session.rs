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
    crate::write_settings(home, settings).unwrap();
    std::fs::write(config_path(home, "models.yml"), models).unwrap();
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
            ("custom", Some("pp.tools")) => {
                assert_eq!(data["version"], 1);
                request["tools"] = data["tools"].clone();
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
                            | "pp.tools"
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

#[path = "session/lifecycle_cases.rs"]
mod lifecycle_cases;
#[path = "session/lock_cases.rs"]
mod lock_cases;
#[path = "session/restore_cases.rs"]
mod restore_cases;
#[path = "session/settings_cases.rs"]
mod settings_cases;
