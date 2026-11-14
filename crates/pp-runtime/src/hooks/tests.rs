//! 执行器与协议边界：真实短命进程、有界 I/O、进程组回收与决策解析。

use super::*;
use pp_config::config::{CommandHook, HookHandlerType, HookMatcherGroup};
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path
}

fn settings(event: HookEvent, matcher: Option<&str>, hooks: Vec<CommandHook>) -> HooksSettings {
    let mut settings = HooksSettings::default();
    let group = HookMatcherGroup {
        matcher: matcher.map(str::to_owned),
        hooks,
    };
    match event {
        HookEvent::UserPromptSubmit => settings.user_prompt_submit.push(group),
        HookEvent::PreToolUse => settings.pre_tool_use.push(group),
        HookEvent::PostToolUse => settings.post_tool_use.push(group),
        HookEvent::PostToolUseFailure => settings.post_tool_use_failure.push(group),
        HookEvent::Stop => settings.stop.push(group),
    }
    settings
}

fn command(path: &Path, timeout: u64) -> CommandHook {
    CommandHook {
        kind: HookHandlerType::Command,
        command: path.display().to_string(),
        args: None,
        timeout,
        on_failure: None,
    }
}

/// 输入信封带 cwd，执行器把它当作子进程工作目录。
fn envelope(dir: &Path) -> serde_json::Value {
    json!({
        "session_id": "s-1",
        "transcript_path": dir.join("session.jsonl").display().to_string(),
        "cwd": dir.display().to_string(),
        "permission_mode": "default",
        "hook_event_name": "PreToolUse",
        "tool_name": "bash",
        "tool_input": {"command": "true"},
        "tool_use_id": "call-1",
    })
}

#[test]
fn hooks_exec_large_stdin_and_interleaved_output_do_not_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    let path = script(
        dir.path(),
        "echo.sh",
        "payload=$(cat)\nprintf '%s' \"${#payload}\"\nprintf 'noise' >&2",
    );
    let input = vec![b'{'; 512 * 1024];
    let outcome = exec::run(
        exec::Request {
            command: path.to_str().unwrap(),
            args: None,
            timeout: Duration::from_secs(20),
            cwd: dir.path(),
            input: &input,
        },
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(outcome.code, Some(0));
    assert_eq!(String::from_utf8(outcome.stdout).unwrap(), "524288");
    assert_eq!(String::from_utf8(outcome.stderr).unwrap(), "noise");
}

#[test]
fn hooks_exec_direct_args_keep_spaces_unsplit() {
    let dir = tempfile::tempdir().unwrap();
    let path = script(dir.path(), "arg.sh", "printf '%s' \"$1\"");
    let args = vec!["a b  c".to_owned()];
    let outcome = exec::run(
        exec::Request {
            command: path.to_str().unwrap(),
            args: Some(&args),
            timeout: Duration::from_secs(20),
            cwd: dir.path(),
            input: b"{}",
        },
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(String::from_utf8(outcome.stdout).unwrap(), "a b  c");
}

#[test]
fn hooks_exec_bounds_stdout_and_keeps_stderr_tail() {
    let dir = tempfile::tempdir().unwrap();
    let huge = script(
        dir.path(),
        "huge.sh",
        &format!(
            "head -c {} /dev/zero | tr '\\0' 'x'",
            exec::STDOUT_LIMIT + 4096
        ),
    );
    let error = exec::run(
        exec::Request {
            command: huge.to_str().unwrap(),
            args: None,
            timeout: Duration::from_secs(20),
            cwd: dir.path(),
            input: b"{}",
        },
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("exceeded"), "{error:#}");

    let noisy = script(
        dir.path(),
        "noisy.sh",
        &format!(
            "head -c {} /dev/zero | tr '\\0' 'y' >&2",
            exec::STDERR_LIMIT * 2
        ),
    );
    let outcome = exec::run(
        exec::Request {
            command: noisy.to_str().unwrap(),
            args: None,
            timeout: Duration::from_secs(20),
            cwd: dir.path(),
            input: b"{}",
        },
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(outcome.stderr.len(), exec::STDERR_LIMIT);
    assert!(outcome.stderr.iter().all(|byte| *byte == b'y'));
}

fn alive(pid: u32) -> bool {
    rustix::process::Pid::from_raw(pid as i32)
        .map(|pid| rustix::process::test_kill_process(pid).is_ok())
        .unwrap_or(false)
}

/// 已回收的进程可能短暂处于僵尸状态：有界轮询而不是一次采样。
fn assert_reaped(pid: u32) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "process {pid} survived hook cleanup"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn hooks_exec_timeout_reaps_root_and_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("pids");
    let path = script(
        dir.path(),
        "hang.sh",
        &format!(
            "sleep 60 &\necho \"$$ $!\" > {}\nsleep 60",
            marker.display()
        ),
    );
    let started = std::time::Instant::now();
    let error = exec::run(
        exec::Request {
            command: path.to_str().unwrap(),
            args: None,
            // 宽限足够让 bash 在满载并行的机器上先落 marker，再被超时回收。
            timeout: Duration::from_secs(5),
            cwd: dir.path(),
            input: b"{}",
        },
        &CancellationToken::new(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("timed out"), "{error:#}");
    assert!(started.elapsed() < Duration::from_secs(20));
    let pids: Vec<u32> = std::fs::read_to_string(&marker)
        .unwrap()
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect();
    assert_eq!(pids.len(), 2);
    for pid in pids {
        assert_reaped(pid);
    }
}

#[test]
fn hooks_exec_reaps_descendants_after_root_exits() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("child");
    let path = script(
        dir.path(),
        "detach.sh",
        &format!("sleep 60 &\necho $! > {}\nexit 0", marker.display()),
    );
    let outcome = exec::run(
        exec::Request {
            command: path.to_str().unwrap(),
            args: None,
            timeout: Duration::from_secs(20),
            cwd: dir.path(),
            input: b"{}",
        },
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(outcome.code, Some(0));
    let pid: u32 = std::fs::read_to_string(&marker)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_reaped(pid);
}

#[test]
fn hooks_exec_cancellation_reaps_and_keeps_cancelled_type() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("pids");
    let path = script(
        dir.path(),
        "wait.sh",
        &format!("echo \"$$ $BASHPID\" > {}\nsleep 60", marker.display()),
    );
    let token = CancellationToken::new();
    let cancellation = token.clone();
    let command = path.display().to_string();
    let cwd = dir.path().to_path_buf();
    let handle = std::thread::spawn(move || {
        exec::run(
            exec::Request {
                command: &command,
                args: None,
                timeout: Duration::from_secs(60),
                cwd: &cwd,
                input: b"{}",
            },
            &cancellation,
        )
    });
    let mut waited = Duration::ZERO;
    while !marker.exists() && waited < Duration::from_secs(20) {
        std::thread::sleep(Duration::from_millis(20));
        waited += Duration::from_millis(20);
    }
    token.cancel();
    let error = handle.join().unwrap().unwrap_err();
    assert!(error.is::<Cancelled>(), "{error:#}");
    let pid: u32 = std::fs::read_to_string(&marker)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert_reaped(pid);
}

#[test]
fn hooks_matcher_names_and_regex_are_case_sensitive_and_unanchored() {
    assert!(
        Matcher::compile(HookEvent::PreToolUse, Some("bash, edit"), 0)
            .unwrap()
            .matches(Some("edit"))
    );
    assert!(
        !Matcher::compile(HookEvent::PreToolUse, Some("Bash"), 0)
            .unwrap()
            .matches(Some("bash"))
    );
    // `mcp__` 只含名称字符：按精确名称匹配，不做前缀匹配。
    assert!(
        !Matcher::compile(HookEvent::PreToolUse, Some("mcp__"), 0)
            .unwrap()
            .matches(Some("mcp__db__query"))
    );
    assert!(
        Matcher::compile(HookEvent::PreToolUse, Some("^mcp__"), 0)
            .unwrap()
            .matches(Some("mcp__db__query"))
    );
    assert!(
        Matcher::compile(HookEvent::PreToolUse, Some("*"), 0)
            .unwrap()
            .matches(Some("bash"))
    );
    assert!(
        Matcher::compile(HookEvent::PreToolUse, None, 0)
            .unwrap()
            .matches(Some("bash"))
    );
    let error = Matcher::compile(HookEvent::PreToolUse, Some("(?<=x)y"), 0).unwrap_err();
    let text = error.to_string();
    assert!(text.contains("PreToolUse[0]"), "{text}");
    // 错误不回显模式原文。
    assert!(!text.contains("(?<=x)"), "{text}");
}

#[test]
fn hooks_compile_rejects_matcher_on_input_events_and_keeps_empty_hooks_inert() {
    let link = settings(
        HookEvent::UserPromptSubmit,
        Some("bash"),
        vec![CommandHook {
            kind: HookHandlerType::Command,
            command: "/bin/true".into(),
            args: None,
            timeout: 30,
            on_failure: None,
        }],
    );
    assert!(Runner::compile(&link).is_err());
    let empty = settings(HookEvent::PreToolUse, None, Vec::new());
    let runner = Runner::compile(&empty).unwrap();
    assert!(!runner.has_handlers(HookEvent::PreToolUse));
}

#[test]
fn hooks_protocol_maps_exit_codes_and_decisions_per_event() {
    let output = protocol::parse(
        HookEvent::UserPromptSubmit,
        Some(0),
        b"extra context",
        b"",
        None,
    )
    .unwrap();
    assert_eq!(output.context, vec!["extra context".to_owned()]);

    let output = protocol::parse(HookEvent::PreToolUse, Some(0), b"just text", b"", None).unwrap();
    assert!(output.context.is_empty() && output.deny.is_none());

    let output = protocol::parse(
        HookEvent::PreToolUse,
        Some(0),
        br#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"no rm"}}"#,
        b"",
        None,
    )
    .unwrap();
    assert_eq!(output.deny.as_deref(), Some("no rm"));

    let output = protocol::parse(
        HookEvent::PreToolUse,
        Some(2),
        b"{ not json",
        b"policy violation",
        None,
    )
    .unwrap();
    assert_eq!(output.deny.as_deref(), Some("policy violation"));

    let output = protocol::parse(HookEvent::PostToolUse, Some(2), b"", b"feedback", None).unwrap();
    assert_eq!(output.context, vec!["feedback".to_owned()]);
    assert!(output.deny.is_none());

    let output = protocol::parse(HookEvent::Stop, Some(0), b"{}", b"", None).unwrap();
    assert!(output.deny.is_none());
    let error = protocol::parse(HookEvent::PreToolUse, Some(3), b"", b"boom", None).unwrap_err();
    assert!(error.to_string().contains("status 3"), "{error:#}");
}

#[test]
fn hooks_protocol_rejects_unknown_fields_and_bad_shapes() {
    for (event, stdout) in [
        (HookEvent::PreToolUse, &b"{\"decision\":\"allow\"}"[..]),
        (HookEvent::PreToolUse, b"{\"unknownControl\":true}"),
        (
            HookEvent::PreToolUse,
            b"{\"hookSpecificOutput\":{\"hookEventName\":\"Stop\",\"permissionDecision\":\"deny\"}}",
        ),
        (
            HookEvent::PostToolUse,
            b"{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"updatedInput\":{}}}",
        ),
        (
            HookEvent::PreToolUse,
            b"{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"updatedInput\":[]}}",
        ),
        (HookEvent::PreToolUse, b"{\"hookSpecificOutput\":{}}"),
        // 前置用 permissionDecision，不允许顶层 decision／reason。
        (
            HookEvent::PreToolUse,
            b"{\"decision\":\"block\",\"reason\":\"no\"}",
        ),
        // 后置失败只接受 additionalContext。
        (
            HookEvent::PostToolUseFailure,
            b"{\"decision\":\"block\",\"reason\":\"no\"}",
        ),
        (
            HookEvent::PostToolUseFailure,
            b"{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUseFailure\",\"updatedToolOutput\":{\"ok\":false}}}",
        ),
        (HookEvent::PreToolUse, b"[1,2]"),
        (HookEvent::PreToolUse, b"{\"reason\": 3}"),
        (
            HookEvent::PostToolUse,
            b"{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"updatedMCPToolOutput\":{}}}",
        ),
    ] {
        assert!(
            protocol::parse(event, Some(0), stdout, b"", None).is_err(),
            "{event:?} {stdout:?} must be rejected"
        );
    }
    // 尾部非空文本不是合法 JSON。
    assert!(
        protocol::parse(
            HookEvent::PreToolUse,
            Some(0),
            b"{\"decision\":\"block\"} trailing",
            b"",
            None
        )
        .is_err()
    );
}

#[test]
fn hooks_protocol_preserves_tool_status_and_stop_semantics() {
    let response = json!({"ok": false, "error": "boom"});
    let output = protocol::parse(
        HookEvent::PostToolUse,
        Some(0),
        br#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":{"note":"redacted"}}}"#,
        b"",
        Some(&response),
    )
    .unwrap();
    assert_eq!(output.updated_output.unwrap()["ok"], json!(false));

    let error = protocol::parse(
        HookEvent::PostToolUse,
        Some(0),
        br#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":{"ok":true}}}"#,
        b"",
        Some(&response),
    )
    .unwrap_err();
    assert!(error.to_string().contains("ok"), "{error:#}");

    let output = protocol::parse(
        HookEvent::Stop,
        Some(0),
        br#"{"decision":"block","reason":"tests missing"}"#,
        b"",
        None,
    )
    .unwrap();
    assert_eq!(output.deny.as_deref(), Some("tests missing"));

    let output = protocol::parse(
        HookEvent::Stop,
        Some(0),
        br#"{"hookSpecificOutput":{"hookEventName":"Stop","additionalContext":"keep going"}}"#,
        b"",
        None,
    )
    .unwrap();
    assert_eq!(output.deny.as_deref(), Some("keep going"));

    let output = protocol::parse(
        HookEvent::PreToolUse,
        Some(0),
        br#"{"continue":false,"stopReason":"stop now","hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny"}}"#,
        b"",
        None,
    )
    .unwrap();
    assert_eq!(output.stop.as_deref(), Some("stop now"));
    assert!(output.deny.is_none());

    // 原结果没有 `cancelled` 时，改写也不能凭空新增该状态。
    let success = json!({"ok": true, "content": "original"});
    let error = protocol::parse(
        HookEvent::PostToolUse,
        Some(0),
        br#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":{"ok":true,"cancelled":true}}}"#,
        b"",
        Some(&success),
    )
    .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error:#}");

    // 省略原有状态字段是允许的：缺失的原值由解析器补回。
    let output = protocol::parse(
        HookEvent::PostToolUse,
        Some(0),
        br#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":{"content":"redacted"}}}"#,
        b"",
        Some(&success),
    )
    .unwrap();
    let updated = output.updated_output.unwrap();
    assert_eq!(updated["ok"], json!(true));
    assert!(updated.get("cancelled").is_none());
}

#[test]
fn hooks_protocol_preserves_image_kind_while_rewriting_content() {
    let image = json!({
        "ok": true,
        "kind": "image",
        "content": "Image: fixture.png",
        "mime_type": "image/png"
    });
    // 省略 `kind`：由解析器补回，模型正文仍可被过滤。
    let output = protocol::parse(
        HookEvent::PostToolUse,
        Some(0),
        br#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":{"content":"filtered"}}}"#,
        b"",
        Some(&image),
    )
    .unwrap();
    let updated = output.updated_output.unwrap();
    assert_eq!(updated["content"], json!("filtered"));
    assert_eq!(updated["kind"], json!("image"));

    // 改成其它值会破坏附件关联，必须拒绝。
    let error = protocol::parse(
        HookEvent::PostToolUse,
        Some(0),
        br#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedToolOutput":{"ok":true,"kind":"text","content":"filtered"}}}"#,
        b"",
        Some(&image),
    )
    .unwrap_err();
    assert!(error.to_string().contains("kind"), "{error:#}");
}

#[test]
fn hooks_protocol_only_stops_on_continue_false() {
    // `stopReason` 是停止理由，不能独立触发停止。
    for stdout in [
        &br#"{"continue":true,"stopReason":"note"}"#[..],
        br#"{"continue":true,"stopReason":""}"#,
        br#"{"stopReason":""}"#,
        br#"{"stopReason":"just a note"}"#,
    ] {
        let output = protocol::parse(HookEvent::PreToolUse, Some(0), stdout, b"", None).unwrap();
        assert!(output.stop.is_none(), "{stdout:?}");
    }
    // 类型校验仍然生效。
    assert!(
        protocol::parse(
            HookEvent::PreToolUse,
            Some(0),
            br#"{"stopReason":3}"#,
            b"",
            None
        )
        .is_err()
    );
    // `continue:false` 的空白理由回落到固定默认理由。
    let output = protocol::parse(
        HookEvent::PreToolUse,
        Some(0),
        br#"{"continue":false,"stopReason":"   "}"#,
        b"",
        None,
    )
    .unwrap();
    let reason = output.stop.expect("continue:false stops the task");
    assert!(!reason.trim().is_empty(), "{reason}");
}

#[test]
fn hooks_protocol_truncates_injected_text_on_char_boundaries() {
    let long = "字".repeat(protocol::TEXT_LIMIT + 10);
    let payload = serde_json::to_vec(&json!({"reason": long})).unwrap();
    let output =
        protocol::parse(HookEvent::UserPromptSubmit, Some(0), &payload, b"", None).unwrap();
    let reason = output.deny.unwrap();
    assert_eq!(
        reason.chars().count(),
        protocol::TEXT_LIMIT + "…[truncated]".chars().count()
    );
    assert!(reason.ends_with("…[truncated]"));
    // 终端转义不进入注入文本。
    let output = protocol::parse(
        HookEvent::UserPromptSubmit,
        Some(0),
        b"{\"reason\":\"a\\u001b[2Jb\"}",
        b"",
        None,
    )
    .unwrap();
    assert!(!output.deny.unwrap().contains('\u{1b}'));
}

#[test]
fn hooks_runner_merges_handlers_in_order_and_short_circuits() {
    let dir = tempfile::tempdir().unwrap();
    let first = script(dir.path(), "first.sh", "cat > /dev/null\nprintf 'ctx-1'");
    let second = script(
        dir.path(),
        "second.sh",
        "cat > /dev/null\necho '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"allow\",\"additionalContext\":\"ctx-2\"}}'",
    );
    let deny = script(
        dir.path(),
        "deny.sh",
        "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"blocked\"}}'",
    );
    let mut settings = HooksSettings::default();
    settings.pre_tool_use.push(HookMatcherGroup {
        matcher: Some("bash".into()),
        hooks: vec![
            command(&first, 30),
            command(&second, 30),
            command(&deny, 30),
        ],
    });
    let runner = Runner::compile(&settings).unwrap();
    assert!(runner.has_handlers(HookEvent::PreToolUse));
    assert!(!runner.has_handlers(HookEvent::Stop));
    let effect = runner
        .run(
            HookEvent::PreToolUse,
            &envelope(dir.path()),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    // 普通文本不提供决定，两个 allow 的上下文按顺序保留；deny 短路。
    assert_eq!(effect.additional_context, vec!["ctx-2".to_owned()]);
    assert_eq!(effect.deny_reason.as_deref(), Some("blocked"));
}

#[test]
fn hooks_runner_reports_failure_policy_per_event() {
    let dir = tempfile::tempdir().unwrap();
    let failing = script(dir.path(), "fail.sh", "exit 7");
    let pre = settings(HookEvent::PreToolUse, None, vec![command(&failing, 30)]);
    let error = Runner::compile(&pre)
        .unwrap()
        .run(
            HookEvent::PreToolUse,
            &envelope(dir.path()),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(error.to_string().contains("status 7"), "{error:#}");
    let mut diagnosed = false;
    let mut observe = |event| {
        diagnosed |= matches!(event, AgentEvent::HookDiagnostic { .. });
    };
    Runner::compile(&pre)
        .unwrap()
        .run(
            HookEvent::PreToolUse,
            &envelope(dir.path()),
            &CancellationToken::new(),
            &mut observe,
        )
        .unwrap_err();
    assert!(diagnosed);

    let post = settings(HookEvent::PostToolUse, None, vec![command(&failing, 30)]);
    let effect = Runner::compile(&post)
        .unwrap()
        .run(
            HookEvent::PostToolUse,
            &envelope(dir.path()),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert!(effect.deny_reason.is_none());

    let mut blocked = command(&failing, 30);
    blocked.on_failure = Some(HookFailurePolicy::Block);
    let post_blocked = settings(HookEvent::PostToolUse, None, vec![blocked]);
    assert!(
        Runner::compile(&post_blocked)
            .unwrap()
            .run(
                HookEvent::PostToolUse,
                &envelope(dir.path()),
                &CancellationToken::new(),
                &mut |_| {},
            )
            .is_err()
    );
}

#[test]
fn hooks_runner_matches_tool_names_only_for_tool_events() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let path = script(
        dir.path(),
        "mark.sh",
        &format!("cat > /dev/null\necho ran >> {}", marker.display()),
    );
    let mut settings = HooksSettings::default();
    settings.pre_tool_use.push(HookMatcherGroup {
        matcher: Some("write,edit".into()),
        hooks: vec![command(&path, 30)],
    });
    let runner = Runner::compile(&settings).unwrap();
    runner
        .run(
            HookEvent::PreToolUse,
            &envelope(dir.path()),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert!(!marker.exists(), "bash must not match a write,edit matcher");
    let mut write = envelope(dir.path());
    write["tool_name"] = json!("write");
    runner
        .run(
            HookEvent::PreToolUse,
            &write,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(std::fs::read_to_string(&marker).unwrap().lines().count(), 1);
}

#[test]
fn hooks_runner_passes_real_envelope_on_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let capture = dir.path().join("stdin.json");
    let path = script(
        dir.path(),
        "capture.sh",
        &format!("cat > {}", capture.display()),
    );
    let mut settings = HooksSettings::default();
    settings.user_prompt_submit.push(HookMatcherGroup {
        matcher: None,
        hooks: vec![command(&path, 30)],
    });
    let runner = Runner::compile(&settings).unwrap();
    let input = json!({
        "session_id": "s-9",
        "transcript_path": dir.path().join("s-9.jsonl").display().to_string(),
        "cwd": dir.path().display().to_string(),
        "permission_mode": "acceptEdits",
        "hook_event_name": "UserPromptSubmit",
        "prompt": "hello",
    });
    runner
        .run(
            HookEvent::UserPromptSubmit,
            &input,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    let received: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&capture).unwrap()).unwrap();
    assert_eq!(received, input);
}

#[test]
fn hooks_protocol_block_without_reason_still_stops_the_event() {
    // 缺 reason 的 block 不能静默放行：输入、Stop 与后置反馈都拿到默认理由。
    for (event, stdout) in [
        (
            HookEvent::UserPromptSubmit,
            &b"{\"decision\":\"block\"}"[..],
        ),
        (HookEvent::Stop, b"{\"decision\":\"block\"}"),
    ] {
        let output = protocol::parse(event, Some(0), stdout, b"", None).unwrap();
        let reason = output.deny.expect("block without reason must deny");
        assert!(!reason.trim().is_empty(), "{event:?} {reason}");
    }
    let output = protocol::parse(
        HookEvent::PostToolUse,
        Some(0),
        b"{\"decision\":\"block\"}",
        b"",
        None,
    )
    .unwrap();
    assert!(output.deny.is_none());
    assert_eq!(output.context.len(), 1, "{:?}", output.context);
    assert!(!output.context[0].trim().is_empty());
    // 显式 reason 仍然是唯一生效的理由。
    let output = protocol::parse(
        HookEvent::UserPromptSubmit,
        Some(0),
        b"{\"decision\":\"block\",\"reason\":\"explicit\"}",
        b"",
        None,
    )
    .unwrap();
    assert_eq!(output.deny.as_deref(), Some("explicit"));
}

#[test]
fn hooks_protocol_redacts_credentials_before_diagnostics() {
    const SECRET: &str = "review-secret-123";
    let error = protocol::parse(
        HookEvent::PreToolUse,
        Some(3),
        b"",
        format!("policy failure: Authorization: Bearer {SECRET}").as_bytes(),
        None,
    )
    .unwrap_err();
    let text = format!("{error:#}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(text.contains("[REDACTED]"), "{text}");

    let output = protocol::parse(
        HookEvent::PreToolUse,
        Some(0),
        format!("{{\"systemMessage\":\"denied for Bearer {SECRET}\"}}").as_bytes(),
        b"",
        None,
    )
    .unwrap();
    let message = output.system_message.expect("systemMessage is preserved");
    assert!(!message.contains(SECRET), "{message}");
    assert!(message.contains("[REDACTED]"), "{message}");

    // exit 2 的 stderr 拒绝理由同样先脱敏再注入。
    let output = protocol::parse(
        HookEvent::UserPromptSubmit,
        Some(2),
        b"",
        format!("blocked: Bearer {SECRET}").as_bytes(),
        None,
    )
    .unwrap();
    let reason = output.deny.expect("exit 2 denies the input");
    assert!(!reason.contains(SECRET), "{reason}");
}

#[test]
fn hooks_runner_feeds_updated_output_to_later_handlers() {
    let dir = tempfile::tempdir().unwrap();
    let seen = dir.path().join("second-envelope.json");
    let rewrite = script(
        dir.path(),
        "rewrite.sh",
        "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"updatedToolOutput\":{\"ok\":true,\"content\":\"REDACTED\"}}}'",
    );
    let observe = script(
        dir.path(),
        "observe.sh",
        &format!("cat > {}", seen.display()),
    );
    let effect = Runner::compile(&settings(
        HookEvent::PostToolUse,
        None,
        vec![command(&rewrite, 30), command(&observe, 30)],
    ))
    .unwrap()
    .run(
        HookEvent::PostToolUse,
        &json!({
            "session_id": "s-1",
            "transcript_path": dir.path().join("s.jsonl").display().to_string(),
            "cwd": dir.path().display().to_string(),
            "permission_mode": "default",
            "hook_event_name": "PostToolUse",
            "tool_name": "read",
            "tool_input": {"path": "a.txt"},
            "tool_use_id": "call-1",
            "tool_response": {"ok": true, "content": "ORIGINAL"},
        }),
        &CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(effect.updated_output.unwrap()["content"], json!("REDACTED"));
    let received: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&seen).unwrap()).unwrap();
    assert_eq!(received["tool_response"]["content"], json!("REDACTED"));
    assert_eq!(received["tool_response"]["ok"], json!(true));
}

#[test]
fn hooks_runner_redacts_credentials_in_handler_failures() {
    const SECRET: &str = "review-secret-456";
    let dir = tempfile::tempdir().unwrap();
    let failing = script(
        dir.path(),
        "leak.sh",
        &format!("cat > /dev/null\nprintf 'Bearer {SECRET}' >&2\nexit 9"),
    );
    let runner = Runner::compile(&settings(
        HookEvent::PreToolUse,
        None,
        vec![command(&failing, 30)],
    ))
    .unwrap();
    let mut diagnostics = Vec::new();
    let error = runner
        .run(
            HookEvent::PreToolUse,
            &envelope(dir.path()),
            &CancellationToken::new(),
            &mut |event| {
                if let AgentEvent::HookDiagnostic { message, .. } = event {
                    diagnostics.push(message);
                }
            },
        )
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(!diagnostics.is_empty());
    for message in &diagnostics {
        assert!(!message.contains(SECRET), "{message}");
    }
}

#[test]
fn hooks_runner_does_not_report_the_command_when_spawn_fails() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing-hook.sh");
    // 直接 exec（不经 bash）才能让进程创建本身失败，覆盖启动错误路径。
    let mut hook = command(&missing, 30);
    hook.args = Some(Vec::new());
    let runner = Runner::compile(&settings(HookEvent::PreToolUse, None, vec![hook])).unwrap();
    let mut diagnostics = Vec::new();
    let error = runner
        .run(
            HookEvent::PreToolUse,
            &envelope(dir.path()),
            &CancellationToken::new(),
            &mut |event| {
                if let AgentEvent::HookDiagnostic { message, .. } = event {
                    diagnostics.push(message);
                }
            },
        )
        .unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("starting hook command"), "{text}");
    assert!(!text.contains("missing-hook.sh"), "{text}");
    assert!(!diagnostics.is_empty());
    for message in diagnostics {
        assert!(!message.contains("missing-hook.sh"), "{message}");
    }
}

#[test]
fn hooks_runner_stops_after_continue_false() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("second-ran");
    let stop = script(
        dir.path(),
        "stop.sh",
        "cat > /dev/null\nprintf '{\"continue\":false,\"stopReason\":\"halt\"}'",
    );
    let second = script(
        dir.path(),
        "second.sh",
        &format!("cat > /dev/null\ntouch {}", marker.display()),
    );
    let settings = settings(
        HookEvent::PostToolUse,
        None,
        vec![command(&stop, 30), command(&second, 30)],
    );
    let effect = Runner::compile(&settings)
        .unwrap()
        .run(
            HookEvent::PostToolUse,
            &envelope(dir.path()),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(effect.stop_reason.as_deref(), Some("halt"));
    assert!(!marker.exists());
}
