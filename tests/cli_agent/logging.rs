// File-logging integration coverage: lifecycle records survive restarts,
// concurrent instances with different working directories share one HOME and
// bounded numeric volumes, success diagnostics omit content, failure diagnostics
// preserve evidence while masking credentials, and an unavailable
// log directory degrades to one fixed warning without changing application
// results. Every run uses a temporary HOME, never the developer's ~/.pp/logs.

use super::*;
use rustix::fs::{FlockOperation, flock};
use std::{
    fs,
    path::{Path, PathBuf},
};

const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;

fn log_dir(home: &Path) -> PathBuf {
    home.join(".pp").join("logs")
}

fn today_volume(number: u64) -> String {
    format!("pp.{}.{}.log", chrono::Local::now().date_naive(), number)
}

// Sorted log volume names with their paths, excluding .lock and stray files.
fn log_files(home: &Path) -> Vec<(String, PathBuf)> {
    let mut files = Vec::new();
    for entry in fs::read_dir(log_dir(home)).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_str().unwrap().to_owned();
        if name.starts_with("pp.") && name.ends_with(".log") {
            files.push((name, entry.path()));
        }
    }
    files.sort();
    files
}

fn assert_volume_names(home: &Path, expected: &[String]) {
    let names: Vec<String> = log_files(home).into_iter().map(|(name, _)| name).collect();
    assert_eq!(names, expected, "unexpected log volume layout");
}

// Every volume is bounded and every line is standalone legal JSON.
fn log_text(home: &Path) -> String {
    let mut bytes = Vec::new();
    for (name, path) in log_files(home) {
        let volume = fs::read(&path).unwrap();
        assert!(
            volume.len() as u64 <= MAX_LOG_BYTES,
            "{name} exceeds the 10 MiB cap"
        );
        bytes.extend_from_slice(&volume);
    }
    String::from_utf8(bytes).expect("log volumes are UTF-8 JSON")
}

fn log_records(home: &Path) -> Vec<Value> {
    let mut records = Vec::new();
    for line in log_text(home).lines() {
        if line.is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("illegal log line {line:?}: {error}"));
        assert!(record["timestamp"].is_string());
        assert!(
            record["level"]
                .as_str()
                .is_some_and(|level| ["INFO", "WARN", "ERROR"].contains(&level)),
            "unexpected level in {line}"
        );
        assert!(record["fields"]["event"].is_string());
        assert!(record["fields"]["pid"].is_u64());
        assert!(
            record.get("span").is_none() && record.get("spans").is_none(),
            "span context must stay disabled: {line}"
        );
        records.push(record);
    }
    records
}

fn assert_no_secrets(home: &Path, markers: &[&str]) {
    let text = log_text(home);
    for marker in markers {
        assert!(!text.contains(marker), "file log leaked {marker}");
    }
}

fn event_name(record: &Value) -> &str {
    record["fields"]["event"].as_str().unwrap()
}

fn str_field<'a>(record: &'a Value, field: &str) -> &'a str {
    record["fields"][field].as_str().unwrap()
}

fn pid_records(records: &[Value], pid: u32) -> Vec<&Value> {
    records
        .iter()
        .filter(|record| record["fields"]["pid"].as_u64() == Some(pid as u64))
        .collect()
}

fn count_events(records: &[&Value], name: &str) -> usize {
    records
        .iter()
        .filter(|record| event_name(record) == name)
        .count()
}

fn pick<'a>(records: &'a [&'a Value], name: &str) -> Vec<&'a Value> {
    records
        .iter()
        .filter(|record| event_name(record) == name)
        .copied()
        .collect()
}

// The child resolves its working directory through the kernel, so compare
// against the canonical path rather than the tempdir symlink form.
fn canonical_cwd(path: &Path) -> String {
    fs::canonicalize(path).unwrap().display().to_string()
}

fn assert_lifecycle_pair(records: &[&Value], cwd: &Path, mode: &str, status: &str) {
    let started = pick(records, "app_started");
    assert_eq!(started.len(), 1, "one app_started per process");
    assert_eq!(str_field(started[0], "mode"), mode);
    assert_eq!(str_field(started[0], "cwd"), canonical_cwd(cwd));
    assert_eq!(started[0]["level"], "INFO");
    let stopped = pick(records, "app_stopped");
    assert_eq!(stopped.len(), 1, "one app_stopped per process");
    assert_eq!(str_field(stopped[0], "status"), status);
    assert_eq!(
        stopped[0]["level"],
        if status == "ok" { "INFO" } else { "ERROR" }
    );
    assert!(stopped[0]["fields"]["elapsed_ms"].is_u64());
    assert_eq!(
        records.first().copied().map(event_name),
        Some("app_started")
    );
    assert_eq!(records.last().copied().map(event_name), Some("app_stopped"));
}

#[test]
fn logging_restarts_append_and_shutdown_records_are_visible() {
    let home = tempfile::tempdir().unwrap();
    let envs = [("HOME", home.path().to_str().unwrap())];

    let first_cwd = tempfile::tempdir().unwrap();
    let first = Cli::spawn(first_cwd.path(), &[], &envs);
    let first_pid = first.0.as_ref().unwrap().id();
    let output = first.finish();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    assert_volume_names(home.path(), &[today_volume(1)]);
    let first_run = fs::read(log_dir(home.path()).join(today_volume(1))).unwrap();
    assert_lifecycle_pair(
        &pid_records(&log_records(home.path()), first_pid),
        first_cwd.path(),
        "line",
        "ok",
    );

    let second_cwd = tempfile::tempdir().unwrap();
    let second = Cli::spawn(second_cwd.path(), &[], &envs);
    let second_pid = second.0.as_ref().unwrap().id();
    let output = second.finish();
    assert!(output.status.success());
    assert_ne!(first_pid, second_pid);
    assert_volume_names(home.path(), &[today_volume(1)]);
    let second_run = fs::read(log_dir(home.path()).join(today_volume(1))).unwrap();
    assert!(
        second_run.starts_with(&first_run),
        "restart must append, not rewrite or rotate early"
    );
    let records = log_records(home.path());
    assert_lifecycle_pair(
        &pid_records(&records, first_pid),
        first_cwd.path(),
        "line",
        "ok",
    );
    assert_lifecycle_pair(
        &pid_records(&records, second_pid),
        second_cwd.path(),
        "line",
        "ok",
    );

    // A prompt task without models still records startup, the configuration
    // problem, and the error terminus, and never leaks logging onto stdout.
    let failure_cwd = tempfile::tempdir().unwrap();
    let failure = Cli::spawn(
        failure_cwd.path(),
        &["-p", "no configuration fixture"],
        &envs,
    );
    let failure_pid = failure.0.as_ref().unwrap().id();
    let output = failure.finish();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_volume_names(home.path(), &[today_volume(1)]);
    let third_run = fs::read(log_dir(home.path()).join(today_volume(1))).unwrap();
    assert!(third_run.starts_with(&second_run));
    let records = log_records(home.path());
    let mine = pid_records(&records, failure_pid);
    assert_eq!(mine.len(), 3);
    assert_eq!(event_name(mine[0]), "app_started");
    assert_eq!(str_field(mine[0], "mode"), "prompt");
    assert_eq!(str_field(mine[0], "cwd"), canonical_cwd(failure_cwd.path()));
    assert_eq!(event_name(mine[1]), "configuration_unavailable");
    assert_eq!(mine[1]["level"], "WARN");
    assert_eq!(event_name(mine[2]), "app_stopped");
    assert_eq!(str_field(mine[2], "status"), "error");
    assert_eq!(mine[2]["level"], "ERROR");
}

#[test]
fn logging_concurrent_cwds_rotate_without_loss() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir_all(log_dir(home.path())).unwrap();
    // A near-full legal JSON line leaves exactly one byte of headroom, so the
    // first append must rotate to volume 2 instead of overflowing volume 1.
    let seed_head = br#"{"timestamp":"2026-10-19T00:00:00.000Z","level":"INFO","fields":{"event":"seed","pid":0},"target":"pp"}"#;
    let mut seed = vec![b' '; MAX_LOG_BYTES as usize - 1];
    seed[..seed_head.len()].copy_from_slice(seed_head);
    seed[MAX_LOG_BYTES as usize - 2] = b'\n';
    let first_volume = log_dir(home.path()).join(today_volume(1));
    fs::write(&first_volume, &seed).unwrap();

    // Hold the shared .lock so every child blocks in logging init, then let
    // them contend in turn; no sleep is needed to guess write completion.
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(log_dir(home.path()).join(".lock"))
        .unwrap();
    flock(&lock, FlockOperation::LockExclusive).unwrap();

    let envs = [("HOME", home.path().to_str().unwrap())];
    let mut children = Vec::new();
    let mut cwds = Vec::new();
    for _ in 0..12 {
        let cwd = tempfile::tempdir().unwrap();
        children.push(Cli::spawn(cwd.path(), &[], &envs));
        cwds.push(cwd);
    }
    let pids: Vec<u32> = children
        .iter()
        .map(|child| child.0.as_ref().unwrap().id())
        .collect();
    flock(&lock, FlockOperation::Unlock).unwrap();
    drop(lock);
    for child in children {
        let output = child.finish();
        assert!(output.status.success());
    }

    assert_eq!(
        fs::read(&first_volume).unwrap(),
        seed,
        "seeded volume bytes must stay unchanged"
    );
    assert_volume_names(home.path(), &[today_volume(1), today_volume(2)]);
    let records = log_records(home.path());
    for record in &records {
        if event_name(record) == "seed" {
            continue;
        }
        let pid = record["fields"]["pid"].as_u64().unwrap();
        assert!(pids.contains(&(pid as u32)), "unexpected writer pid {pid}");
    }
    for (pid, cwd) in pids.iter().zip(&cwds) {
        assert_lifecycle_pair(&pid_records(&records, *pid), cwd.path(), "line", "ok");
    }
    let distinct: std::collections::BTreeSet<u32> = pids.iter().copied().collect();
    assert_eq!(distinct.len(), 12);
}

// Local protocol dispatch keeps this module self-contained, mirroring the
// session module instead of widening its helper visibility.
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
    fn protocol(self) -> &'static str {
        match self {
            Self::Chat => "completion",
            Self::Responses => "response",
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
    fn calls_reply(self, request: Request, calls: Value) {
        match self {
            Self::Chat => calls_reply(request, calls),
            Self::Responses => response_reply(request, calls),
        }
    }
    fn final_reply(self, request: Request, text: &str) {
        match self {
            Self::Chat => final_reply(request, text),
            Self::Responses => response_final(request, text),
        }
    }
    fn call(self, id: &str, name: &str, args: Value) -> Value {
        match self {
            Self::Chat => call(id, name, args),
            Self::Responses => response_call(id, name, args),
        }
    }
    fn tool_result(self, body: &Value, id: &str) -> Value {
        let items = match self {
            Self::Chat => body["messages"].as_array().unwrap(),
            Self::Responses => body["input"].as_array().unwrap(),
        };
        let item = items
            .iter()
            .find(|item| {
                (item["role"] == "tool" && item["tool_call_id"] == id)
                    || (item["type"] == "function_call_output" && item["call_id"] == id)
            })
            .expect("missing tool result");
        let content = match self {
            Self::Chat => &item["content"],
            Self::Responses => &item["output"],
        };
        serde_json::from_str(content.as_str().unwrap()).unwrap()
    }
}

#[test]
fn logging_operational_events_do_not_store_content_or_secrets() {
    let (prompt_marker, answer_marker, path_marker, content_marker, error_marker) = (
        "secret-prompt-7c4f",
        "secret-answer-91bd",
        "secret-path-5e2a",
        "secret-content-38d0",
        "secret-error-6f77",
    );
    let markers = [
        "fixture-key",
        prompt_marker,
        answer_marker,
        path_marker,
        content_marker,
        error_marker,
    ];
    for api in [Api::Chat, Api::Responses] {
        // One real prompt task: a write success, a write failure, and a final
        // answer, with a unique secret marker in every content channel.
        let server = server();
        let work = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            work.path(),
            Some(&format!("follow the instructions {prompt_marker}")),
            fixture_settings(),
            &api.models(&server),
        );
        let pid = cli.0.as_ref().unwrap().id();
        let (request, _) = api.receive(&server);
        api.calls_reply(
            request,
            json!([api.call(
                "w",
                "write",
                json!({
                    "path": format!("{path_marker}.txt"),
                    "content": format!("recorded {content_marker}")
                })
            )]),
        );
        let (request, body) = api.receive(&server);
        assert_eq!(api.tool_result(&body, "w")["ok"], true);
        api.calls_reply(
            request,
            json!([api.call("f", "write", json!({"path": "", "content": "denied"}))]),
        );
        let (request, body) = api.receive(&server);
        assert_eq!(api.tool_result(&body, "f")["ok"], false);
        api.final_reply(request, &format!("done {answer_marker}"));
        let output = cli.finish();
        assert!(output.status.success());
        assert_eq!(output.stdout, format!("done {answer_marker}\n").as_bytes());
        assert_eq!(
            fs::read_to_string(work.path().join(format!("{path_marker}.txt"))).unwrap(),
            format!("recorded {content_marker}")
        );
        no_more(&server);

        assert_no_secrets(work.path(), &markers);
        assert_volume_names(work.path(), &[today_volume(1)]);
        let records = log_records(work.path());
        let mine = pid_records(&records, pid);
        let names: Vec<&str> = mine.iter().map(|record| event_name(record)).collect();
        assert_eq!(names.first(), Some(&"app_started"));
        assert_eq!(names.last(), Some(&"app_stopped"));
        for record in &mine {
            assert_ne!(record["level"], "ERROR", "success run stays INFO/WARN");
        }
        assert_eq!(count_events(&mine, "app_started"), 1);
        assert_eq!(str_field(mine[0], "mode"), "prompt");
        assert_eq!(str_field(mine[0], "cwd"), canonical_cwd(work.path()));
        assert_eq!(count_events(&mine, "task_started"), 1);
        for record in pick(&mine, "task_started") {
            assert_eq!(str_field(record, "operation"), "run");
            assert_eq!(record["level"], "INFO");
        }
        assert_eq!(count_events(&mine, "task_finished"), 1);
        for record in pick(&mine, "task_finished") {
            assert_eq!(str_field(record, "operation"), "run");
            assert_eq!(str_field(record, "status"), "ok");
            assert_eq!(record["level"], "INFO");
            assert!(record["fields"]["elapsed_ms"].is_u64());
        }
        assert_eq!(count_events(&mine, "model_request_started"), 3);
        for record in pick(&mine, "model_request_started") {
            assert_eq!(str_field(record, "protocol"), api.protocol());
            assert_eq!(record["level"], "INFO");
        }
        assert_eq!(count_events(&mine, "model_request_finished"), 3);
        for record in pick(&mine, "model_request_finished") {
            assert_eq!(str_field(record, "protocol"), api.protocol());
            assert_eq!(str_field(record, "status"), "ok");
            assert_eq!(record["level"], "INFO");
            assert!(record["fields"]["elapsed_ms"].is_u64());
        }
        assert_eq!(count_events(&mine, "tool_started"), 2);
        for record in pick(&mine, "tool_started") {
            assert_eq!(str_field(record, "tool"), "write");
            assert_eq!(record["level"], "INFO");
        }
        let finished = pick(&mine, "tool_finished");
        assert_eq!(finished.len(), 2);
        let mut statuses: Vec<&str> = finished
            .iter()
            .map(|record| str_field(record, "status"))
            .collect();
        statuses.sort_unstable();
        assert_eq!(statuses, ["error", "ok"]);
        for record in &finished {
            assert_eq!(str_field(record, "tool"), "write");
            assert!(record["fields"]["elapsed_ms"].is_u64());
            assert_eq!(
                record["level"],
                if str_field(record, "status") == "error" {
                    "WARN"
                } else {
                    "INFO"
                }
            );
        }
        assert_eq!(count_events(&mine, "app_stopped"), 1);
        for record in pick(&mine, "app_stopped") {
            assert_eq!(str_field(record, "status"), "ok");
            assert_eq!(record["level"], "INFO");
        }

        // HTTP 错误正文保留现场，但请求提示词与实际凭据仍不落盘。
        let failure_server = super::server();
        let failure_home = tempfile::tempdir().unwrap();
        let failure_work = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(failure_home.path(), "settings.yml"),
            fixture_settings(),
        )
        .unwrap();
        std::fs::write(
            config_path(failure_home.path(), "models.yml"),
            api.models(&failure_server),
        )
        .unwrap();
        let failure_prompt = format!("explode {prompt_marker}");
        let failure = Cli::spawn(
            failure_work.path(),
            &["-p", failure_prompt.as_str()],
            &[("HOME", failure_home.path().to_str().unwrap())],
        );
        let failure_pid = failure.0.as_ref().unwrap().id();
        let (request, _) = api.receive(&failure_server);
        request
            .respond(
                Response::from_string(
                    json!({
                        "error": {"message": format!("upstream exposed fixture-key {error_marker}")}
                    })
                    .to_string(),
                )
                .with_status_code(500)
                .with_header(Header::from_bytes("x-request-id", "request-evidence-500").unwrap()),
            )
            .unwrap();
        let output = failure.finish();
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());

        assert_no_secrets(failure_home.path(), &markers[..5]);
        assert!(log_text(failure_home.path()).contains(error_marker));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(error_marker));
        let records = log_records(failure_home.path());
        let mine = pid_records(&records, failure_pid);
        let http = pick(&mine, "ai_http_error");
        assert_eq!(http.len(), 1);
        assert_eq!(http[0]["fields"]["status"], 500);
        assert_eq!(http[0]["fields"]["body_complete"], true);
        assert_eq!(str_field(http[0], "request_id"), "request-evidence-500");
        assert_eq!(count_events(&mine, "app_started"), 1);
        assert_eq!(count_events(&mine, "configuration_unavailable"), 0);
        assert_eq!(count_events(&mine, "task_started"), 1);
        assert_eq!(count_events(&mine, "tool_started"), 0);
        assert_eq!(count_events(&mine, "tool_finished"), 0);
        assert_eq!(count_events(&mine, "model_request_started"), 1);
        for record in pick(&mine, "model_request_started") {
            assert_eq!(str_field(record, "protocol"), api.protocol());
        }
        assert_eq!(count_events(&mine, "model_request_finished"), 1);
        for record in pick(&mine, "model_request_finished") {
            assert_eq!(str_field(record, "protocol"), api.protocol());
            assert_eq!(str_field(record, "status"), "error");
            assert_eq!(record["level"], "ERROR");
        }
        assert_eq!(count_events(&mine, "task_finished"), 1);
        for record in pick(&mine, "task_finished") {
            assert_eq!(str_field(record, "operation"), "run");
            assert_eq!(str_field(record, "status"), "error");
            assert_eq!(record["level"], "ERROR");
        }
        for record in pick(&mine, "app_stopped") {
            assert_eq!(str_field(record, "status"), "error");
            assert_eq!(record["level"], "ERROR");
        }
    }
}

#[test]
fn logging_ai_failures_preserve_parse_and_stream_evidence() {
    for api in [Api::Chat, Api::Responses] {
        for kind in ["syntax", "structure", "eof"] {
            let server = server();
            let work = tempfile::tempdir().unwrap();
            let cli = Cli::start_with(
                work.path(),
                Some("private-request-prompt"),
                fixture_settings(),
                &api.models(&server),
            );
            let (request, _) = api.receive(&server);
            let mut stream = SseReply::new(request);
            let partial = match api {
                Api::Chat => chat_chunk(
                    json!({"role":"assistant","content":"partial-evidence"}),
                    None,
                ),
                Api::Responses => {
                    json!({"type":"response.output_text.delta","sequence_number":1,"item_id":"msg","output_index":0,"content_index":0,"delta":"partial-evidence"})
                }
            };
            stream.event(partial);
            match kind {
                "syntax" => {
                    stream.frame(r#"{"evidence":"bad-json-evidence fixture-key","broken":]}"#)
                }
                "structure" => stream.event(match api {
                    Api::Chat => {
                        chat_chunk(json!({"role":"unknown-role-evidence fixture-key"}), None)
                    }
                    Api::Responses => {
                        json!({"type":"unknown-event-evidence","evidence":"fixture-key"})
                    }
                }),
                _ => {}
            }
            stream.finish();
            let output = cli.finish();
            assert!(!output.status.success(), "{kind}");
            let text = log_text(work.path());
            assert!(text.contains("partial-evidence"), "{kind}: {text}");
            let marker = match kind {
                "syntax" => "bad-json-evidence",
                "structure" => "unknown-",
                _ => "stream ended",
            };
            assert!(text.contains(marker), "{kind}: {text}");
            assert_no_secrets(work.path(), &["fixture-key", "private-request-prompt"]);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !stderr.contains("bad-json-evidence")
                    && !stderr.contains("unknown-role-evidence")
                    && !stderr.contains("unknown-event-evidence")
            );
            let records = log_records(work.path());
            let failure = records
                .iter()
                .find(|record| event_name(record) == "ai_request_failed")
                .unwrap();
            assert!(failure["fields"]["received_events"].as_u64().unwrap() >= 1);
            assert!(failure["fields"]["error"].as_str().unwrap().len() > 5);
            no_more(&server);
        }
    }
}

#[test]
fn logging_tool_argument_errors_keep_sanitized_evidence() {
    for api in [Api::Chat, Api::Responses] {
        let server = server();
        let work = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            work.path(),
            Some("test"),
            fixture_settings(),
            &api.models(&server),
        );
        let (request, _) = api.receive(&server);
        api.calls_reply(
            request,
            json!([api.call(
                "bad",
                "read",
                json!({"path":123,"evidence":"bad-tool-evidence","api_key":"unregistered-secret"})
            )]),
        );
        let (request, body) = api.receive(&server);
        assert_eq!(api.tool_result(&body, "bad")["ok"], false);
        api.final_reply(request, "recovered");
        assert!(cli.finish().status.success());
        let records = log_records(work.path());
        let failure = records
            .iter()
            .find(|record| {
                event_name(record) == "tool_finished" && record["fields"]["status"] == "error"
            })
            .unwrap();
        assert!(
            failure["fields"]["arguments"]
                .as_str()
                .unwrap()
                .contains("bad-tool-evidence")
        );
        assert!(!failure["fields"]["error"].as_str().unwrap().is_empty());
        assert_no_secrets(work.path(), &["fixture-key", "unregistered-secret"]);
        no_more(&server);
    }
}

fn debug_text(home: &Path) -> String {
    let mut files: Vec<_> = fs::read_dir(log_dir(home))
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("debug."))
        .collect();
    files.sort_by_key(|entry| entry.file_name());
    let mut text = String::new();
    for entry in files {
        text.push_str(&fs::read_to_string(entry.path()).unwrap());
    }
    text
}

fn prompt_logs(home: &Path) -> Vec<(String, String)> {
    let mut logs: Vec<_> = fs::read_dir(log_dir(home))
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix("prompt-debug.")?;
            Some((name, fs::read_to_string(entry.path()).unwrap()))
        })
        .collect();
    logs.sort();
    logs
}

#[test]
fn debug_logging_is_opt_in() {
    for settings in [None, Some(r#"{"debug":false}"#)] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        fs::write(
            config_path(home.path(), "models.yml"),
            fixture_models(&server),
        )
        .unwrap();
        if let Some(settings) = settings {
            fs::write(config_path(home.path(), "settings.yml"), settings).unwrap();
        }
        let cli = Cli::start_with_existing_files(home.path(), Some("private disabled prompt"));
        let (request, _) = receive(&server);
        final_reply(request, "done");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert!(debug_text(home.path()).is_empty());
        assert!(prompt_logs(home.path()).is_empty());
        assert_no_secrets(home.path(), &["private disabled prompt", "fixture-key"]);
    }
}

#[test]
fn debug_logging_separates_initial_prompts_per_session_across_tasks_and_models() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let mut models: Value =
        serde_json::from_str(&response_models(&server, Some("completion"))).unwrap();
    let mut response_provider: Value =
        serde_json::from_str(&response_models(&server, None)).unwrap();
    models["providers"]["responses"] = response_provider["providers"]["fixture"].take();
    fs::write(home.path().join("debug-source.txt"), "tool result marker").unwrap();
    let mut cli = Cli::start_with(
        home.path(),
        None,
        r#"{"debug":true,"think_level":"high","request_timeout_seconds":7}"#,
        &models.to_string(),
    );
    cli.input("首个合成 prompt\nsecond\n/model responses/primary\nthird\n/model fixture/primary\nfourth\n/model responses/primary\n/new\nfifth\n/quit\n");
    let (request, _) = receive(&server);
    calls_reply(
        request,
        json!([call(
            "debug-read",
            "read",
            json!({"path":"debug-source.txt"})
        )]),
    );
    let (request, _) = receive(&server);
    final_reply(request, "tool done");
    let (request, _) = receive(&server);
    final_reply(request, "second done");
    let (request, _) = receive_at(&server, "/v1/responses");
    response_final(request, "third done");
    let (request, _) = receive(&server);
    final_reply(request, "fourth done");
    let (request, _) = receive_at(&server, "/v1/responses");
    response_final(request, "fifth done");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    let logs = prompt_logs(home.path());
    assert_eq!(logs.len(), 2);
    let first = &logs
        .iter()
        .find(|(_, text)| text.contains("首个合成 prompt"))
        .unwrap()
        .1;
    let next = &logs
        .iter()
        .find(|(_, text)| text.contains("fifth"))
        .unwrap()
        .1;
    assert!(first.contains("protocol: completion\n"));
    assert!(next.contains("protocol: response\n"));
    for (name, prompt) in &logs {
        let id = name.strip_prefix("prompt-debug.").unwrap();
        assert!(
            home.path()
                .join(".pp/agent/sessions")
                .join(format!("{id}.jsonl"))
                .is_file()
        );
        assert!(prompt.contains(&format!("session_id: {id}\n")));
        assert_eq!(prompt.matches("=== llm_first_prompt |").count(), 1);
        assert!(!prompt.contains("harness_state"));
        assert!(prompt.contains("You are a coding assistant."));
        assert!(prompt.contains("function / read"));
        for marker in [
            "tool result marker",
            "second done",
            "third done",
            "fourth done",
            "fixture-key",
            "Authorization",
        ] {
            assert!(!prompt.contains(marker), "prompt log leaked {marker}");
        }
    }
    assert!(!first.contains("fifth"));
    assert!(!next.contains("首个合成 prompt"));
    let text = debug_text(home.path());
    assert_eq!(text.matches("=== harness_state |").count(), 6);
    for marker in [
        "llm_first_prompt",
        "首个合成 prompt",
        "fifth",
        "tool result marker",
        "fixture-key",
    ] {
        assert!(!text.contains(marker), "debug log leaked {marker}");
    }
    assert_no_secrets(
        home.path(),
        &["首个合成 prompt", "tool result marker", "fixture-key"],
    );
    no_more(&server);
}

#[test]
fn debug_logging_preserves_initial_prompt_on_resume_and_continue() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let models = response_models(&fixture, Some(protocol));
        let cli = Cli::start_with(
            home.path(),
            Some("initial marker"),
            r#"{"debug":true}"#,
            &models,
        );
        let path = if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        };
        let (request, _) = receive_at(&fixture, path);
        if protocol == "completion" {
            final_reply(request, "initial answer");
        } else {
            response_final(request, "initial answer");
        }
        assert!(cli.finish().status.success());
        let original = prompt_logs(home.path());
        assert_eq!(original.len(), 1);
        let id = original[0].0.strip_prefix("prompt-debug.").unwrap();
        for args in [vec!["-r", id, "-p", "resumed marker"], vec!["-c", id]] {
            let cli = Cli::spawn(home.path(), &args, &[]);
            let (request, _) = receive_at(&fixture, path);
            if protocol == "completion" {
                final_reply(request, "later answer");
            } else {
                response_final(request, "later answer");
            }
            assert!(cli.finish().status.success());
            assert_eq!(prompt_logs(home.path()), original);
        }
        assert_eq!(
            debug_text(home.path())
                .matches("=== harness_state |")
                .count(),
            3
        );
        no_more(&fixture);
    }
}

#[test]
fn debug_logging_keeps_first_prompt_when_provider_rejects_request() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let cli = Cli::start_with(
        home.path(),
        Some("debug failed request marker\nsecond line with \"quotes\" and \\path"),
        r#"{"debug":true,"rate_limit_retry_budget_seconds":0}"#,
        &fixture_models(&server),
    );
    let (request, _) = receive(&server);
    request
        .respond(Response::from_string("private provider error body").with_status_code(401))
        .unwrap();
    let output = cli.finish();
    assert!(!output.status.success());
    let logs = prompt_logs(home.path());
    assert_eq!(logs.len(), 1);
    let text = &logs[0].1;
    assert_eq!(text.matches("=== llm_first_prompt |").count(), 1);
    assert!(!debug_text(home.path()).contains("debug failed request marker"));
    let lines: Vec<_> = text.lines().map(str::trim).collect();
    assert!(lines.windows(2).any(|pair| pair
        == [
            "debug failed request marker",
            "second line with \"quotes\" and \\path",
        ]));
    assert!(!text.contains("private provider error body"));
    assert!(!text.contains("fixture-key"));
    assert_no_secrets(home.path(), &["debug failed request marker", "fixture-key"]);
    assert!(log_text(home.path()).contains("private provider error body"));
    no_more(&server);
}

#[test]
fn logging_unavailable_directory_preserves_application_result() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".pp")).unwrap();
    let logs_path = home.path().join(".pp").join("logs");
    std::fs::write(&logs_path, "sentinel file, not a directory\n").unwrap();
    let envs = [("HOME", home.path().to_str().unwrap())];

    // Interactive line mode still exits cleanly, warns exactly once, and the
    // offending regular file is neither overwritten nor truncated.
    let work = tempfile::tempdir().unwrap();
    let cli = Cli::spawn(work.path(), &[], &envs);
    let output = cli.finish();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("warning: file logging unavailable").count(),
        1,
        "exactly one fixed warning, actual stderr: {stderr}"
    );
    assert!(fs::symlink_metadata(&logs_path).unwrap().is_file());
    assert_eq!(
        fs::read(&logs_path).unwrap(),
        b"sentinel file, not a directory\n"
    );

    // A prompt task under the same broken HOME keeps its failure exit code.
    let failure_work = tempfile::tempdir().unwrap();
    let cli = Cli::spawn(failure_work.path(), &["-p", "still fails"], &envs);
    let output = cli.finish();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("warning: file logging unavailable").count(),
        1,
        "exactly one fixed warning, actual stderr: {stderr}"
    );
    assert!(fs::symlink_metadata(&logs_path).unwrap().is_file());
    assert_eq!(
        fs::read(&logs_path).unwrap(),
        b"sentinel file, not a directory\n"
    );
}
