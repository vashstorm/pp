use super::*;
use std::collections::BTreeSet;

fn spawn_with_stdin(
    cwd: &std::path::Path,
    home: &std::path::Path,
    args: &[&str],
    stdin_text: &str,
) -> Cli {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pp"));
    command
        .current_dir(cwd)
        .env("HOME", home)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().unwrap();
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(stdin_text.as_bytes()).unwrap();
    }
    Cli(Some(child))
}

#[test]
fn phase2_cd_and_relative_output_file_and_piped_stdin() {
    let server = server();
    let caller = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let work = caller.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("inside.txt"), "inside-ok\n").unwrap();
    write_settings(home.path(), fixture_settings()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();

    // 1. -C switches the working directory, but a relative -o path is still resolved against the caller's cwd; an
    //    explicit prompt and piped stdin are concatenated.
    let cli = spawn_with_stdin(
        caller.path(),
        home.path(),
        &["-C", "work", "-o", "report.txt", "-p", "review diff"],
        "line-a\nline-b\n",
    );
    let (request, body) = receive(&server);
    let user_msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|m| m["role"] == "user")
        .unwrap();
    assert_eq!(user_msg["content"], "review diff\n\nline-a\nline-b");
    assert!(
        body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains(&work.canonicalize().unwrap().display().to_string())
    );
    calls_reply(
        request,
        json!([call("c-read", "read", json!({"path": "inside.txt"}))]),
    );
    let (request, body) = receive(&server);
    let tool_msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == "c-read")
        .unwrap();
    assert_eq!(result(tool_msg, "c-read")["ok"], true);
    final_reply(request, "reviewed ok");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read_to_string(caller.path().join("report.txt")).unwrap(),
        "reviewed ok\n"
    );
    assert!(!work.join("report.txt").exists());

    // 2. Pure stdin driving (no positional prompt).
    let cli = spawn_with_stdin(caller.path(), home.path(), &["-p"], "piped-only-prompt\n");
    let (request, body) = receive(&server);
    let user_msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|m| m["role"] == "user")
        .unwrap();
    assert_eq!(user_msg["content"], "piped-only-prompt");
    final_reply(request, "from pipe");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "from pipe");
    no_more(&server);
}

#[test]
fn phase2_guardrails_max_turns_timeout_signals_and_exit_codes() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    write_settings(dir.path(), fixture_settings()).unwrap();
    std::fs::write(
        config_path(dir.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();

    // 1. --max-turns 1: when the first turn returns a tool_call that needs a second turn, it terminates
    //    immediately with exit code 3 and writes the terminal error to JSON.
    let max_turns = Cli::spawn(
        dir.path(),
        &["-p", "--max-turns", "1", "--mode", "json", "need two turns"],
        &[],
    );
    let (request, _) = receive(&server);
    calls_reply(
        request,
        json!([call("c1", "read", json!({"path": "a.txt"}))]),
    );
    let max_out = max_turns.finish();
    assert_eq!(max_out.status.code(), Some(3), "{max_out:?}");
    let max_json: Value =
        serde_json::from_str(std::str::from_utf8(&max_out.stdout).unwrap().trim()).unwrap();
    assert_eq!(max_json["type"], "result");
    assert_eq!(max_json["status"], "error");
    assert!(max_json["error"].as_str().unwrap().contains("--max-turns"));

    // 2. --timeout 250ms: the request hangs past the limit, so exit code 124 and the terminal error in JSON.
    let timed = Cli::spawn(
        dir.path(),
        &["-p", "--timeout", "250ms", "--mode", "json", "slow request"],
        &[],
    );
    let (_held_request, _) = receive(&server);
    let timed_out = timed.finish();
    assert_eq!(timed_out.status.code(), Some(124), "{timed_out:?}");
    let timed_json: Value =
        serde_json::from_str(std::str::from_utf8(&timed_out.stdout).unwrap().trim()).unwrap();
    assert_eq!(timed_json["status"], "error");
    assert!(timed_json["error"].as_str().unwrap().contains("Timed out"));
    drop(_held_request);

    // 3. SIGTERM and SIGINT cancel gracefully, stream-json appends the terminal result and the exit codes are
    //    143 / 130 respectively.
    for (signal, expected_code) in [
        (rustix::process::Signal::TERM, 143),
        (rustix::process::Signal::INT, 130),
    ] {
        let signaled = Cli::spawn(
            dir.path(),
            &["-p", "--mode", "stream-json", "wait for signal"],
            &[],
        );
        let (_held, _) = receive(&server);
        let raw_pid = i32::try_from(signaled.0.as_ref().unwrap().id()).unwrap();
        let pid = rustix::process::Pid::from_raw(raw_pid).unwrap();
        rustix::process::kill_process(pid, signal).unwrap();
        let sig_out = signaled.finish();
        assert_eq!(sig_out.status.code(), Some(expected_code), "{sig_out:?}");
        let lines: Vec<Value> = std::str::from_utf8(&sig_out.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.first().unwrap()["type"], "init");
        let last = lines.last().unwrap();
        assert_eq!(last["type"], "result");
        assert_eq!(last["status"], "error");
        drop(_held);
    }

    // 4. Usage errors always return exit code 2.
    for args in [
        &["-p", "--timeout", "0s", "bad"][..],
        &["-p", "-C", "/nonexistent-pp-dir", "bad"][..],
        &["-p", "--effort", "invalid", "bad"][..],
        &["--mode", "json"][..],
    ] {
        let out = Cli::spawn(dir.path(), args, &[]).finish();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
    }
    no_more(&server);
}

#[test]
fn phase2_output_schema_retries_buffering_and_output_file() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    write_settings(dir.path(), fixture_settings()).unwrap();
    std::fs::write(
        config_path(dir.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    let schema_path = dir.path().join("schema.json");
    std::fs::write(
        &schema_path,
        r#"{"type":"object","properties":{"status":{"type":"string"},"count":{"type":"integer"}},"required":["status","count"],"additionalProperties":false}"#,
    )
    .unwrap();

    // 1. --mode text with --output-schema: a first-round violation never reaches stdout, and after the second round
    //    corrects it the clean output is written and -o is populated.
    let out_file = dir.path().join("result.json");
    let cli = Cli::spawn(
        dir.path(),
        &[
            "-p",
            "--output-schema",
            schema_path.to_str().unwrap(),
            "-o",
            out_file.to_str().unwrap(),
            "return structured status",
        ],
        &[],
    );
    let (request, body) = receive(&server);
    assert!(body["messages"].to_string().contains("Output requirement:"));
    final_reply(request, "sorry here is markdown ```json```");
    let (request, body) = receive(&server);
    assert!(body["messages"].to_string().contains("--output-schema"));
    final_reply(request, r#"{"status":"ok","count":2}"#);
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "{\"status\":\"ok\",\"count\":2}\n"
    );
    assert_eq!(
        std::fs::read_to_string(&out_file).unwrap(),
        "{\"status\":\"ok\",\"count\":2}\n"
    );

    // 2. Under --mode json the result fields of phase 1 stay unchanged, and the validated JSON lands in result.text.
    let json_cli = Cli::spawn(
        dir.path(),
        &[
            "-p",
            "--mode",
            "json",
            "--output-schema",
            schema_path.to_str().unwrap(),
            "structured json mode",
        ],
        &[],
    );
    let (request, _) = receive(&server);
    final_reply(request, r#"{"status":"done","count":5}"#);
    let json_out = json_cli.finish();
    assert!(json_out.status.success(), "{json_out:?}");
    let summary: Value =
        serde_json::from_str(std::str::from_utf8(&json_out.stdout).unwrap().trim()).unwrap();
    let keys: BTreeSet<&str> = summary
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        BTreeSet::from([
            "effort",
            "elapsed_ms",
            "model",
            "session_id",
            "status",
            "text",
            "turns",
            "type",
            "usage",
        ])
    );
    assert_eq!(summary["text"], r#"{"status":"done","count":5}"#);

    // 3. Retries exhausted and still not matching the schema: fail (code 1) without overwriting the existing -o file.
    std::fs::write(&out_file, "keep-previous\n").unwrap();
    let fail_cli = Cli::spawn(
        dir.path(),
        &[
            "-p",
            "--output-schema",
            schema_path.to_str().unwrap(),
            "-o",
            out_file.to_str().unwrap(),
            "always invalid",
        ],
        &[],
    );
    for _ in 0..3 {
        let (request, _) = receive(&server);
        final_reply(request, "still invalid");
    }
    let fail_out = fail_cli.finish();
    assert_eq!(fail_out.status.code(), Some(1), "{fail_out:?}");
    assert_eq!(
        std::fs::read_to_string(&out_file).unwrap(),
        "keep-previous\n"
    );
    no_more(&server);
}
