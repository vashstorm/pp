// Debug 构建的内嵌 Mock AI Server 与全部 `/test-*` 命令的端到端回归。
//
// 每个用例都在独立临时 HOME/工作目录中启动真实 pp 二进制，只通过子进程环境
// 注入代理变量；不会读取开发者 HOME，也不会访问真实供应商。

use super::streaming::LiveCli;
use super::*;
use rusqlite::Connection;
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream},
    path::{Path, PathBuf},
};

const COMMANDS: [&str; 13] = [
    "/test-task",
    "/test-ui",
    "/test-tools",
    "/test-read",
    "/test-write",
    "/test-edit",
    "/test-grep",
    "/test-glob",
    "/test-bash",
    "/test-bash-task",
    "/test-wait",
    "/test-todo",
    "/test-ask",
];

const TASK_MARKERS: [&str; 3] = [
    "fast-rust-count: completed (fast-result-11)",
    "slow-symbol-lookup: completed (slow-result-22)",
    "review-events-risk: completed (review-result-33)",
];

const UI_MARKERS: [&str; 3] = [
    "UI-TEST-ASSISTANT-START",
    "UI-TEST-CODE-MARKER",
    "UI-TEST-ASSISTANT-END",
];

const TOOL_MARKERS: [&str; 12] = [
    "Read fixture complete.",
    "Write fixture complete.",
    "Replace edit fixture complete.",
    "Hashline edit fixture complete.",
    "Grep fixture complete.",
    "Glob fixture complete.",
    "Bash fixture complete.",
    "Background Bash fixture complete.",
    "Wait fixture complete.",
    "Todo fixture complete.",
    "Todo scenario complete; remaining item retained for inspection.",
    "Ask fixture complete.",
];

fn own_markers(command: &str) -> Vec<&'static str> {
    match command {
        "/test-task" => TASK_MARKERS.to_vec(),
        "/test-ui" => UI_MARKERS.to_vec(),
        "/test-tools" => TOOL_MARKERS.to_vec(),
        "/test-read" => vec!["Read fixture complete."],
        "/test-write" => vec!["Write fixture complete."],
        "/test-edit" => vec![
            "Replace edit fixture complete.",
            "Hashline edit fixture complete.",
        ],
        "/test-grep" => vec!["Grep fixture complete."],
        "/test-glob" => vec!["Glob fixture complete."],
        "/test-bash" => vec!["Bash fixture complete."],
        "/test-bash-task" => vec!["Background Bash fixture complete."],
        "/test-wait" => vec!["Wait fixture complete."],
        "/test-todo" => vec![
            "Todo fixture complete.",
            "Todo scenario complete; remaining item retained for inspection.",
        ],
        "/test-ask" => vec!["Ask fixture complete."],
        other => panic!("unknown mock test command {other}"),
    }
}

fn every_marker() -> Vec<&'static str> {
    let mut markers = Vec::new();
    for command in COMMANDS {
        for marker in own_markers(command) {
            if !markers.contains(&marker) {
                markers.push(marker);
            }
        }
    }
    markers
}

fn text(output: &Output) -> (String, String) {
    (
        String::from_utf8(output.stdout.clone()).unwrap(),
        String::from_utf8(output.stderr.clone()).unwrap(),
    )
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

fn session_files(home: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(home.join(".pp/agent/sessions")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect()
}

fn wait_for_session(home: &Path) -> PathBuf {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let files = session_files(home);
        if let [file] = files.as_slice() {
            return file.clone();
        }
        assert!(
            Instant::now() < deadline,
            "expected one saved session, found {files:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn request_for(server: &Server, timeout: Duration) -> (Request, Value) {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match server.recv_timeout(remaining) {
            Ok(Some(mut request)) => {
                let body = serde_json::from_reader(request.as_reader()).unwrap();
                return (request, body);
            }
            Ok(None) => {
                assert!(Instant::now() < deadline, "missing expected model request");
            }
            Err(error) => panic!("receiving model request: {error}"),
        }
    }
}

fn usage_rows(home: &Path) -> Vec<(String, u64)> {
    let database = Connection::open(home.join(".pp/agent/agent.db")).unwrap();
    database
        .prepare("SELECT model_reference, request_count FROM model_usage ORDER BY model_reference")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn role_rows(home: &Path) -> Vec<(String, String)> {
    let database = Connection::open(home.join(".pp/agent/agent.db")).unwrap();
    database
        .prepare(
            "SELECT role, model_reference FROM model_roles
             WHERE role IN ('fast','slow','review') ORDER BY role",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn stats_models(home: &Path) -> Vec<(String, String)> {
    let database = Connection::open(home.join(".pp/stats/stats.db")).unwrap();
    database
        .prepare("SELECT model, model_reference FROM requests ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn mock_test_all_commands_run_in_empty_home_without_models() {
    let home = tempfile::tempdir().unwrap();
    let mut cli = LiveCli::new(Cli::start_env(home.path(), home.path(), &[], &[]));
    for command in COMMANDS {
        cli.input(&format!("{command}\n"));
    }
    cli.input("/quit\n");
    let output = cli.finish_within(Duration::from_secs(120));
    assert!(output.status.success(), "{output:?}");
    let (stdout, stderr) = text(&output);
    assert!(
        stderr.contains("Configuration unavailable"),
        "empty HOME must really be unavailable: {stderr}"
    );
    for marker in every_marker() {
        assert!(
            stdout.contains(marker),
            "missing {marker} after all commands: {stdout}\n{stderr}"
        );
    }
    for tool in [
        "read",
        "write",
        "edit",
        "grep",
        "glob",
        "bash",
        "bash_task",
        "wait",
        "todo",
        "ask",
    ] {
        assert!(
            stderr.contains(&format!("tool {tool}: ")),
            "missing real tool event for {tool}: {stderr}"
        );
    }
    assert!(
        stderr.contains("Model: mock-root/primary"),
        "mock model status missing: {stderr}"
    );
    assert!(
        stderr.contains("incomplete todos - reminder"),
        "native incomplete-todo reminder missing: {stderr}"
    );
    // 预期工具失败只作为结果展示，后续脚本仍继续到各自完成正文。
    for tool in ["write", "edit", "grep", "bash", "ask"] {
        assert!(
            stderr.contains(&format!("tool {tool}: failed")),
            "missing expected failure for {tool}: {stderr}"
        );
    }
    assert!(!stderr.contains("usage: /test-"), "{stderr}");
    assert!(session_files(home.path()).is_empty());
}

#[test]
fn mock_test_each_command_runs_only_its_own_scenarios() {
    let expected: &[(&str, &[(&str, usize)])] = &[
        ("/test-task", &[("task", 1), ("wait", 1)]),
        ("/test-ui", &[]),
        (
            "/test-tools",
            &[
                ("read", 4),
                ("write", 3),
                ("edit", 4),
                ("grep", 4),
                ("glob", 4),
                ("bash", 7),
                ("bash_task", 3),
                ("wait", 2),
                ("todo", 9),
                ("ask", 1),
            ],
        ),
        ("/test-read", &[("read", 3)]),
        ("/test-write", &[("write", 3)]),
        ("/test-edit", &[("read", 1), ("edit", 4)]),
        ("/test-grep", &[("grep", 4)]),
        ("/test-glob", &[("glob", 4)]),
        ("/test-bash", &[("bash", 4)]),
        ("/test-bash-task", &[("bash", 1), ("bash_task", 3)]),
        ("/test-wait", &[("bash", 2), ("wait", 2)]),
        ("/test-todo", &[("todo", 9)]),
        ("/test-ask", &[("ask", 1)]),
    ];
    for (command, expected) in expected {
        let home = tempfile::tempdir().unwrap();
        let mut cli = LiveCli::new(Cli::start_env(home.path(), home.path(), &[], &[]));
        cli.input(&format!("{command}\n/quit\n"));
        let output = cli.finish_within(Duration::from_secs(60));
        assert!(output.status.success(), "{command}: {output:?}");
        let (_, stderr) = text(&output);
        let mut observed = BTreeMap::new();
        for line in stderr.lines() {
            if let Some(tool) = line
                .trim_start_matches("> ")
                .strip_prefix("tool ")
                .and_then(|line| line.strip_suffix(": running"))
            {
                *observed.entry(tool).or_insert(0) += 1;
            }
        }
        assert_eq!(
            observed,
            expected.iter().copied().collect(),
            "{command}: {stderr}"
        );
    }
}

#[test]
fn mock_test_commands_work_with_broken_yaml_configuration() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        "{{{ broken yaml\n",
    )
    .unwrap();
    std::fs::write(config_path(home.path(), "models.yml"), "- not: [a model\n").unwrap();
    let mut cli = LiveCli::new(Cli::start_env(home.path(), home.path(), &[], &[]));
    cli.input("/test-ui\n/test-read\n/test-task\n/quit\n");
    let output = cli.finish_within(Duration::from_secs(60));
    assert!(output.status.success(), "{output:?}");
    let (stdout, stderr) = text(&output);
    for marker in UI_MARKERS.into_iter().chain([
        "Read fixture complete.",
        "fast-rust-count: completed (fast-result-11)",
        "slow-symbol-lookup: completed (slow-result-22)",
        "review-events-risk: completed (review-result-33)",
    ]) {
        assert!(
            stdout.contains(marker),
            "broken config blocked {marker}: {stdout}\n{stderr}"
        );
    }
    assert!(
        stderr.contains("Configuration unavailable"),
        "the broken config must stay visible for ordinary tasks: {stderr}"
    );
    assert!(session_files(home.path()).is_empty());
}

#[test]
fn mock_test_commands_bypass_broken_proxies_and_never_touch_a_real_provider() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    write_settings(home.path(), r#"{"rate_limit_retry_budget_seconds":0}"#).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    let bad = "http://127.0.0.1:1";
    let mut cli = LiveCli::new(Cli::start_env(
        home.path(),
        home.path(),
        &[],
        &[
            ("HTTP_PROXY", bad),
            ("HTTPS_PROXY", bad),
            ("ALL_PROXY", bad),
            ("NO_PROXY", ""),
            ("http_proxy", bad),
            ("https_proxy", bad),
            ("all_proxy", bad),
            ("no_proxy", ""),
        ],
    ));
    cli.input("/test-ui\n/test-read\n/test-task\n/quit\n");
    let output = cli.finish_within(Duration::from_secs(60));
    assert!(output.status.success(), "{output:?}");
    let (stdout, _) = text(&output);
    for marker in UI_MARKERS.into_iter().chain([
        "Read fixture complete.",
        "fast-rust-count: completed (fast-result-11)",
        "review-events-risk: completed (review-result-33)",
    ]) {
        assert!(stdout.contains(marker), "proxy blocked {marker}: {stdout}");
    }
    no_more(&server);
}

#[test]
fn mock_test_task_ignores_sentinel_provider_and_missing_roles() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    write_settings(home.path(), r#"{"rate_limit_retry_budget_seconds":0}"#).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    let mut cli = LiveCli::new(Cli::start_env(home.path(), home.path(), &[], &[]));
    cli.input("/test-task\n/quit\n");
    let output = cli.finish_within(Duration::from_secs(60));
    assert!(output.status.success(), "{output:?}");
    let (stdout, stderr) = text(&output);
    for marker in TASK_MARKERS {
        assert!(stdout.contains(marker), "missing {marker}: {stdout}");
    }
    assert!(stderr.contains("tool task: ok"), "{stderr}");
    assert!(stderr.contains("tool wait: ok"), "{stderr}");
    assert!(role_rows(home.path()).is_empty());
    no_more(&server);
}

#[test]
fn mock_test_edit_and_tools_repeat_with_fresh_workspaces_and_snapshots() {
    let home = tempfile::tempdir().unwrap();
    let mut cli = LiveCli::new(Cli::start_env(home.path(), home.path(), &[], &[]));
    cli.input("/test-edit\n/test-edit\n/test-tools\n/quit\n");
    let output = cli.finish_within(Duration::from_secs(180));
    assert!(output.status.success(), "{output:?}");
    let (stdout, stderr) = text(&output);
    assert_eq!(
        count(&stdout, "Replace edit fixture complete."),
        3,
        "{stdout}"
    );
    assert_eq!(
        count(&stdout, "Hashline edit fixture complete."),
        3,
        "{stdout}"
    );
    assert_eq!(count(&stdout, "Read fixture complete."), 1, "{stdout}");
    // 每场 replace、hashline 各一次成功 edit；第二次运行若复用工作区/快照就会失败。
    assert_eq!(count(&stderr, "tool edit: ok"), 6, "{stderr}");
    assert_eq!(count(&stderr, "tool edit: failed"), 6, "{stderr}");
    assert_eq!(count(&stderr, "tool read: ok"), 6, "{stderr}");
    assert_eq!(count(&stderr, "tool read: failed"), 0, "{stderr}");
    assert_eq!(count(&stderr, "tool write: ok"), 2, "{stderr}");
    assert_eq!(count(&stderr, "tool write: failed"), 1, "{stderr}");
    assert_eq!(count(&stderr, "tool bash: failed"), 1, "{stderr}");
    // /test-tools 的 wait 场必须使用自己 Tools 的首个后台 ID，而不是前场的后续 ID。
    assert!(stderr.contains("task bg-2:"), "{stderr}");
    assert!(!stderr.contains("task bg-3:"), "{stderr}");
    assert!(session_files(home.path()).is_empty());
}

#[test]
fn mock_test_bash_task_repeats_from_fresh_task_ids() {
    let home = tempfile::tempdir().unwrap();
    let mut cli = LiveCli::new(Cli::start_env(home.path(), home.path(), &[], &[]));
    cli.input("/test-bash-task\n/test-bash-task\n/quit\n");
    let output = cli.finish_within(Duration::from_secs(60));
    assert!(output.status.success(), "{output:?}");
    let (stdout, stderr) = text(&output);
    assert_eq!(count(&stdout, "Background Bash fixture complete."), 2);
    let ids: Vec<&str> = stderr
        .lines()
        .filter_map(|line| line.strip_prefix("task bg-"))
        .filter_map(|rest| rest.split_once(':'))
        .map(|(id, _)| id)
        .collect();
    assert!(!ids.is_empty(), "{stderr}");
    assert!(
        ids.iter().all(|id| *id == "1"),
        "each fresh Tools must start at bg-1: {ids:?}\n{stderr}"
    );
}

#[test]
fn mock_test_normal_agent_session_roles_and_stats_survive_each_scenario() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("keep.txt"), "project content\n").unwrap();
    write_settings(
        home.path(),
        json!({
            "rate_limit_retry_budget_seconds": 0,
            "notification_timeout_seconds": 1,
            "default_model": "fixture/primary",
            "read": {"toolResultPreview": true},
            "model_roles": {
                "fast": {"model": "fixture/primary", "effort": "none"},
                "slow": {"model": "fixture/primary", "effort": "none"},
                "review": {"model": "fixture/primary", "effort": "none"}
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();

    let mut cli = LiveCli::new(Cli::start_env(project.path(), home.path(), &[], &[]));
    cli.input("normal-before\n");
    let (request, body) = request_for(&server, Duration::from_secs(20));
    assert_eq!(body["model"], "fixture-model");
    final_reply(request, "normal-before-reply");
    let session = wait_for_session(home.path());

    cli.input("/test-ui\n/test-read\n/test-edit\n/test-tools\n/test-task\n");
    cli.input("normal-after\n");
    let (request, body) = request_for(&server, Duration::from_secs(120));
    assert_eq!(
        body["model"], "fixture-model",
        "ordinary task must keep the configured model"
    );
    final_reply(request, "normal-after-reply");
    cli.input("/quit\n");

    let output = cli.finish_within(Duration::from_secs(180));
    assert!(output.status.success(), "{output:?}");
    let (stdout, _) = text(&output);
    assert!(stdout.contains("normal-before-reply"), "{stdout}");
    assert!(stdout.contains("normal-after-reply"), "{stdout}");

    assert_eq!(session_files(home.path()), vec![session.clone()]);
    let transcript = std::fs::read_to_string(&session).unwrap();
    for marker in [
        "normal-before",
        "normal-after",
        "normal-before-reply",
        "normal-after-reply",
    ] {
        assert!(
            transcript.contains(marker),
            "missing {marker}: {transcript}"
        );
    }
    for marker in [
        "UI-TEST-ASSISTANT-START",
        "Read fixture complete.",
        "Replace edit fixture complete.",
        "fast-result-11",
    ] {
        assert!(
            !transcript.contains(marker),
            "mock run leaked {marker} into the real session"
        );
    }

    assert_eq!(
        usage_rows(home.path()),
        vec![("fixture/primary".to_owned(), 2)],
        "mock requests must not count as real model usage"
    );
    assert_eq!(
        role_rows(home.path()),
        vec![
            ("fast".to_owned(), "fixture/primary".to_owned()),
            ("review".to_owned(), "fixture/primary".to_owned()),
            ("slow".to_owned(), "fixture/primary".to_owned()),
        ],
        "roles must not be rewritten by /test-task"
    );
    let stats = stats_models(home.path());
    assert_eq!(stats.len(), 2, "{stats:?}");
    assert!(
        stats
            .iter()
            .all(|(model, reference)| model == "fixture-model" && reference == "fixture/primary"),
        "mock requests leaked into statistics: {stats:?}"
    );

    assert_eq!(
        std::fs::read_to_string(project.path().join("keep.txt")).unwrap(),
        "project content\n"
    );
    let mut entries: Vec<String> = std::fs::read_dir(project.path())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    entries.sort();
    assert_eq!(
        entries,
        ["keep.txt"],
        "mock runs must not add project files"
    );
    no_more(&server);
}

#[test]
fn mock_test_line_mode_rejects_arguments_without_running_scenarios() {
    let home = tempfile::tempdir().unwrap();
    let mut cli = LiveCli::new(Cli::start_env(home.path(), home.path(), &[], &[]));
    cli.input("/test-edit extra\n/test-ui extra\n/test-tools extra\n/quit\n");
    let output = cli.finish_within(Duration::from_secs(30));
    assert!(output.status.success(), "{output:?}");
    let (stdout, stderr) = text(&output);
    for command in ["/test-edit", "/test-ui", "/test-tools"] {
        assert!(
            stderr.contains(&format!("usage: {command}")),
            "missing usage for {command}: {stderr}"
        );
    }
    for marker in every_marker() {
        assert!(
            !stdout.contains(marker),
            "argument error must not run a scenario: {marker}"
        );
    }
}

// 手工 Responses 客户端：直接读取 chunked SSE，直到终止块，不依赖连接关闭。
struct HttpResponse {
    status: u16,
    body: String,
}

fn http_request(port: u16, method: &str, path: &str, body: &[u8]) -> HttpResponse {
    let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(5))
        .unwrap_or_else(|error| panic!("connecting to mock server on {port}: {error}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
    let _ = stream.shutdown(Shutdown::Write);

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("bad HTTP status line {status_line:?}"));
    let mut content_length = None;
    let mut chunked = false;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            content_length = Some(value.trim().parse::<usize>().unwrap());
        }
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }
    if matches!(status, 404 | 405) {
        return HttpResponse {
            status,
            body: String::new(),
        };
    }
    let body = if chunked {
        let mut body = Vec::new();
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).unwrap();
            let size = usize::from_str_radix(size.trim(), 16).unwrap();
            if size == 0 {
                let mut trailer = String::new();
                reader.read_line(&mut trailer).unwrap();
                break;
            }
            let mut chunk = vec![0; size];
            reader.read_exact(&mut chunk).unwrap();
            let mut terminator = [0; 2];
            reader.read_exact(&mut terminator).unwrap();
            assert_eq!(&terminator, b"\r\n");
            body.extend_from_slice(&chunk);
        }
        body
    } else if let Some(length) = content_length {
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        body
    } else {
        let mut body = Vec::new();
        reader.read_to_end(&mut body).unwrap();
        body
    };
    HttpResponse {
        status,
        body: String::from_utf8(body).unwrap(),
    }
}

fn mock_log_port(home: &Path) -> Option<u16> {
    let entries = std::fs::read_dir(home.join(".pp/logs")).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("pp.") && name.ends_with(".log")) {
            continue;
        }
        let text = std::fs::read_to_string(entry.path()).ok()?;
        for line in text.lines() {
            let Ok(record) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if record["fields"]["event"] == "mock_ai_started"
                && let Some(port) = record["fields"]["port"].as_u64()
            {
                return u16::try_from(port).ok();
            }
        }
    }
    None
}

fn wait_for_log_port(home: &Path) -> u16 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(port) = mock_log_port(home) {
            assert!(port > 32_000, "mock server must bind a high loopback port");
            return port;
        }
        assert!(Instant::now() < deadline, "mock_ai_started was not logged");
        thread::sleep(Duration::from_millis(10));
    }
}

// lsof 存在时核对实际监听地址；缺失 lsof 的平台退回启动日志与实际连接。
fn lsof_listeners(pid: u32) -> Option<Vec<(String, u16)>> {
    let output = Command::new("lsof")
        .args([
            "-a",
            "-p",
            &pid.to_string(),
            "-iTCP",
            "-sTCP:LISTEN",
            "-nP",
            "-F",
            "n",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return Some(Vec::new());
    }
    let mut listeners = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some(address) = line.strip_prefix('n') else {
            continue;
        };
        let Some((host, port)) = address.rsplit_once(':') else {
            continue;
        };
        if let Ok(port) = port.parse() {
            listeners.push((host.to_owned(), port));
        }
    }
    Some(listeners)
}

fn wait_for_listener(pid: u32, port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match lsof_listeners(pid) {
            None => {
                TcpStream::connect_timeout(
                    &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
                    Duration::from_millis(500),
                )
                .unwrap_or_else(|error| panic!("connecting to logged mock port {port}: {error}"));
                return;
            }
            Some(listeners) if listeners.iter().any(|(_, found)| *found == port) => {
                for (host, found) in &listeners {
                    assert_eq!(host, "127.0.0.1", "mock listener must stay on loopback");
                    assert_eq!(*found, port, "unexpected mock listener set: {listeners:?}");
                }
                return;
            }
            Some(_) if Instant::now() >= deadline => {
                panic!("process {pid} never listened on logged mock port {port}")
            }
            Some(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn assert_listener_closed(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match TcpStream::connect_timeout(
            &SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
            Duration::from_millis(100),
        ) {
            Err(_) => return,
            Ok(_) if Instant::now() >= deadline => {
                panic!("mock listener on {port} survived process exit")
            }
            Ok(_) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[test]
fn mock_test_manual_server_posts_until_reload_and_closes_with_the_owner() {
    let first_home = tempfile::tempdir().unwrap();
    let second_home = tempfile::tempdir().unwrap();
    let mut first = Cli::start_env(first_home.path(), first_home.path(), &[], &[]);
    let mut second = Cli::start_env(second_home.path(), second_home.path(), &[], &[]);
    let first_port = wait_for_log_port(first_home.path());
    let second_port = wait_for_log_port(second_home.path());
    assert_ne!(
        first_port, second_port,
        "parallel processes need distinct ports"
    );
    wait_for_listener(first.pid(), first_port);
    wait_for_listener(second.pid(), second_port);

    // 非 POST 与未命中路由不推进任何队列。
    assert_eq!(
        http_request(first_port, "GET", "/v1/responses", b"").status,
        405
    );
    assert_eq!(
        http_request(first_port, "POST", "/missing", b"[]").status,
        404
    );

    let response = http_request(
        first_port,
        "POST",
        "/v1/responses",
        br#"[{"response":"first"},{"response":"second"}]"#,
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(
        response.body.contains("\"delta\":\"first\""),
        "{}",
        response.body
    );
    assert!(
        !response.body.contains("\"delta\":\"second\""),
        "{}",
        response.body
    );
    assert!(response.body.contains("\"model\":\"pp-mock-root\""));

    // 队列未耗尽时完全不解析请求体：任意非 JSON 字节也只推进到下一条。
    let response = http_request(first_port, "POST", "/v1/responses", b"not json at all");
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(
        response.body.contains("\"delta\":\"second\""),
        "{}",
        response.body
    );
    assert!(
        !response.body.contains("\"delta\":\"first\""),
        "{}",
        response.body
    );

    // 耗尽后缺少新数组：400 且不伪造完成文本；随后重新上传数组即可恢复。
    let response = http_request(
        first_port,
        "POST",
        "/v1/responses",
        br#"{"input":"ignored"}"#,
    );
    assert_eq!(response.status, 400, "{}", response.body);
    assert!(
        response.body.contains("mock_script_required"),
        "{}",
        response.body
    );
    let response = http_request(
        first_port,
        "POST",
        "/v1/responses",
        br#"[{"response":"new-batch"}]"#,
    );
    assert_eq!(response.status, 200, "{}", response.body);
    assert!(
        response.body.contains("\"delta\":\"new-batch\""),
        "{}",
        response.body
    );

    second.input("/quit\n");
    let output = second.finish_within(Duration::from_secs(10));
    assert!(output.status.success(), "{output:?}");
    assert_listener_closed(second_port);

    first.input("/quit\n");
    let output = first.finish_within(Duration::from_secs(10));
    assert!(output.status.success(), "{output:?}");
    assert_listener_closed(first_port);
}
