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

    // 1. -C 切换工作目录，但 -o 相对路径仍按调用方 cwd 解析；显式 prompt + piped stdin 拼接。
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

    // 2. 纯 stdin 驱动（无位置 prompt）。
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
fn phase2_continue_latest_session_and_ephemeral_persistence() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    write_settings(dir.path(), task_fixture_settings()).unwrap();
    std::fs::write(
        config_path(dir.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();

    // 1. 新建持久化会话。
    let first = Cli::spawn(
        dir.path(),
        &["-p", "--mode", "json", "remember code 73"],
        &[],
    );
    let (request, _) = receive(&server);
    final_reply(request, "stored 73");
    let first_out = first.finish();
    assert!(first_out.status.success(), "{first_out:?}");
    let first_json: Value =
        serde_json::from_str(std::str::from_utf8(&first_out.stdout).unwrap().trim()).unwrap();
    let session_id = first_json["session_id"].as_str().unwrap().to_owned();
    let session_file = dir
        .path()
        .join(format!(".pp/agent/sessions/{session_id}.jsonl"));
    let original_bytes = std::fs::read(&session_file).unwrap();

    // 2. `-p -r <id> --ephemeral` 只读恢复：可读取原历史，但绝不修改原文件、不留锁、session_id 为 null。
    let readonly_resume = Cli::spawn(
        dir.path(),
        &[
            "-p",
            "-r",
            &session_id,
            "--ephemeral",
            "--mode",
            "json",
            "readonly ask",
        ],
        &[],
    );
    let (request, body) = receive(&server);
    assert!(
        body["messages"].to_string().contains("remember code 73"),
        "{body}"
    );
    final_reply(request, "ephemeral reply");
    let readonly_out = readonly_resume.finish();
    assert!(readonly_out.status.success(), "{readonly_out:?}");
    let readonly_json: Value =
        serde_json::from_str(std::str::from_utf8(&readonly_out.stdout).unwrap().trim()).unwrap();
    assert_eq!(readonly_json["session_id"], Value::Null);
    assert_eq!(std::fs::read(&session_file).unwrap(), original_bytes);

    // 3. `-p -c "follow up"` 续跑最近会话（无需传 ID）并落盘新轮次。
    let cont = Cli::spawn(
        dir.path(),
        &["-p", "-c", "--mode", "json", "follow up"],
        &[],
    );
    let (request, body) = receive(&server);
    assert!(
        body["messages"].to_string().contains("remember code 73"),
        "{body}"
    );
    assert!(body["messages"].to_string().contains("follow up"), "{body}");
    final_reply(request, "continued");
    let cont_out = cont.finish();
    assert!(cont_out.status.success(), "{cont_out:?}");
    let cont_json: Value =
        serde_json::from_str(std::str::from_utf8(&cont_out.stdout).unwrap().trim()).unwrap();
    assert_eq!(cont_json["session_id"], session_id);
    assert!(std::fs::read(&session_file).unwrap().len() > original_bytes.len());

    // 4. 全新 HOME 下 `--ephemeral` 不创建 sessions 目录（即使含 Task）。
    let fresh_home = tempfile::tempdir().unwrap();
    write_settings(fresh_home.path(), task_fixture_settings()).unwrap();
    std::fs::write(
        config_path(fresh_home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    let ephem = Cli::spawn(
        fresh_home.path(),
        &[
            "-p",
            "--ephemeral",
            "--allowed-tools",
            "Read,Task",
            "--mode",
            "json",
            "ephemeral fresh",
        ],
        &[],
    );
    let (request, _) = receive(&server);
    final_reply(request, "zero disk");
    let ephem_out = ephem.finish();
    assert!(ephem_out.status.success(), "{ephem_out:?}");
    let ephem_json: Value =
        serde_json::from_str(std::str::from_utf8(&ephem_out.stdout).unwrap().trim()).unwrap();
    assert_eq!(ephem_json["session_id"], Value::Null);
    assert!(!fresh_home.path().join(".pp/agent/sessions").exists());
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

    // 1. --max-turns 1：首轮返回 tool_call 需进第 2 轮时立即终止，退出码 3 且 JSON 写出终态 error。
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

    // 2. --timeout 250ms：请求挂起超过时限，退出码 124 且 JSON 写出终态 error。
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

    // 3. SIGTERM 与 SIGINT 优雅取消，stream-json 补写终态 result 且退出码分别为 143 / 130。
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

    // 4. 用法错误统一返回退出码 2。
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

    // 1. --mode text + --output-schema：第 1 轮违规不透出到 stdout，第 2 轮纠错通过后干净输出并写 -o。
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

    // 2. --mode json 下 1 期 result 字段保持零变化，通过校验的 JSON 落在 result.text。
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

    // 3. 重试耗尽仍不合 schema：失败退出（码 1）且不覆盖已有 -o 文件。
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

#[test]
fn phase2_bash_rules_disallowed_tools_effort_and_no_hooks() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("target.txt"), "x\n").unwrap();
    std::fs::create_dir_all(dir.path().join(".pp")).unwrap();
    std::fs::write(
        dir.path().join(".pp/hooks.yml"),
        json!({
            "UserPromptSubmit": [{
                "hooks": [{"command": "sh", "args": ["-c", "exit 2"]}]
            }]
        })
        .to_string(),
    )
    .unwrap();
    write_settings(dir.path(), fixture_settings()).unwrap();
    std::fs::write(
        config_path(dir.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();

    // 1. 默认启用的 hook 会阻断任务；--no-hooks 跳过加载与执行。
    let blocked = Cli::spawn(dir.path(), &["-p", "blocked by hook"], &[]).finish();
    assert!(!blocked.status.success());

    // 2. workspace-write 下 `Bash(echo:*)` 预批准放行，但 `&& pwd` 与 `eval` 被 AST 门禁拒绝；同时验证 `--effort high` 与 `--disallowed-tools`。
    let cli = Cli::spawn(
        dir.path(),
        &[
            "-p",
            "--no-hooks",
            "--permission-mode",
            "workspace-write",
            "--effort",
            "high",
            "--allowed-tools",
            "Read,Bash(echo:*),Bash(pwd:*),Write",
            "--disallowed-tools",
            "Write,Bash(pwd:*)",
            "run allowed bash",
        ],
        &[],
    );
    let (request, body) = receive(&server);
    assert_eq!(body["reasoning_effort"], "high");
    let exposed: BTreeSet<&str> = body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(exposed, BTreeSet::from(["read", "bash"]));
    calls_reply(
        request,
        json!([
            call("b-ok", "bash", json!({"command": "echo safe-ok"})),
            call("b-chain", "bash", json!({"command": "echo ok && pwd"})),
            call("w-deny", "write", json!({"path": "x.txt", "content": "no"}))
        ]),
    );
    let (request, body) = receive(&server);
    let extract = |id: &str| -> Value {
        let msg = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
            .unwrap();
        result(msg, id)
    };
    assert_eq!(extract("b-ok")["ok"], true);
    assert_eq!(extract("b-chain")["ok"], false);
    assert_eq!(extract("w-deny")["ok"], false);
    final_reply(request, "done");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");

    // 3. 即使在 `--permission-mode yolo` 下，`--allowed-tools Bash(echo:*)` 依然在能力门禁拒绝非匹配命令。
    let yolo_cli = Cli::spawn(
        dir.path(),
        &[
            "-p",
            "--no-hooks",
            "--permission-mode",
            "yolo",
            "--allowed-tools",
            "Bash(echo:*)",
            "check yolo capability gate",
        ],
        &[],
    );
    let (request, _) = receive(&server);
    calls_reply(
        request,
        json!([call("b-pwd", "bash", json!({"command": "pwd"}))]),
    );
    let (request, body) = receive(&server);
    let msg = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool" && m["tool_call_id"] == "b-pwd")
        .unwrap();
    assert_eq!(result(msg, "b-pwd")["ok"], false);
    final_reply(request, "blocked even in yolo");
    let yolo_out = yolo_cli.finish();
    assert!(yolo_out.status.success(), "{yolo_out:?}");
    no_more(&server);
}
