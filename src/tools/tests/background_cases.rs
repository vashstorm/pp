use super::*;

#[test]
fn background_natural_exit_cancellation_and_tools_drop_reap_descendants() {
    let workflow = WorkflowState::default();
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
    let execution = tools.execute("bash", &json!({"mode":"background","command":"trap '' TERM; sleep 60 & child=$!; printf '%s\\n%s' \"$$\" \"$child\" > cancelled.pending; mv cancelled.pending cancelled.pid; printf cancelled-output; wait \"$child\""}).to_string(), "cancel-start", ToolPolicy::Execute { workflow: &workflow }, &token, &mut |_| {});
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
    let workflow = WorkflowState::default();
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let mut stdout = String::new();
    let mut stderr = String::new();
    let token = CancellationToken::new();
    let execution = tools.execute(
            "bash",
            &json!({"command":"for ((i=0;i<10000;i++)); do printf 'out\\n'; printf 'err\\n' >&2; done; printf done > completed; exit 7"}).to_string(),
            "line-limit",
            ToolPolicy::Execute { workflow: &workflow },
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
    let workflow = WorkflowState::default();
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    tools.limits.bash_timeout_seconds = 1;
    let mut blocked = false;
    let start = Instant::now();
    let token = CancellationToken::new();
    let execution = tools.execute(
        "bash",
        &json!({"command":"trap '' TERM; printf ready; sleep 3; printf late > late"}).to_string(),
        "call",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
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

#[test]
fn bash_cancel_stops_term_resistant_group_and_reports_cancelled() {
    let workflow = WorkflowState::default();
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
            ToolPolicy::Execute { workflow: &workflow },
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
