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
    assert_eq!(terminal["output"], "retained");
    wait_until_gone(
        background_marker(&dir.path().join("natural.pid"))
            .parse()
            .unwrap(),
        "natural-exit descendant",
    );
    let token = CancellationToken::new();
    let execution = tools.execute("bash", &json!({"mode":"background","command":"sh -c 'trap \"\" TERM; sleep 60 & child=$!; printf \"%s\\n%s\" \"$$\" \"$child\" > cancelled.pending; mv cancelled.pending cancelled.pid; printf cancelled-output; wait \"$child\"'"}).to_string(), "cancel-start", ToolPolicy::Execute { workflow: &workflow }, &token, &mut |_| {});
    let started: Value = serde_json::from_str(&execution.output).unwrap();
    let pids = background_marker(&dir.path().join("cancelled.pid"));
    let other = execute(
        &tools,
        "bash",
        json!({"mode":"background","command":"sh -c 'printf \"%s\\n%s\" \"$$\" \"$TMPDIR\" > drop.pending; mv drop.pending drop.marker; exec sleep 60'"}),
    );
    let drop_marker = background_marker(&dir.path().join("drop.marker"));
    token.cancel();
    let cancelled = background_wait(&tools, started["task_id"].as_str().unwrap(), |value| {
        value["status"] != "running"
    });
    assert_eq!(cancelled["status"], "cancelled");
    assert_eq!(cancelled["ok"], false);
    assert_eq!(cancelled["output"], "cancelled-output");
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
        json!({"mode":"background","command":"sh -c 'trap \"\" TERM; printf \"%s\" \"$$\" > parent.pid; sleep 60 & child=$!; printf \"%s\" \"$child\" > child.pid; printf \"%s\" \"$TMPDIR\" > scratch.path; printf ready; wait \"$child\"'"}),
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
    assert_eq!(alive["output"], "alive");
    assert_eq!(alive["exit_code"], 0);
    let running = background_wait(&tools, id, |value| value["output"] == "ready");
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
    assert_eq!(next["output"], "next");
}

#[test]
fn background_output_is_bounded_without_blocking_progress() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    tools.artifacts.bind(dir.path(), "background").unwrap();
    for command in [
        "printf '%70000s' ''; printf '%70000s' '' >&2; printf done > completed; exit 7",
        "for ((i=0;i<10000;i++)); do printf 'out\\n'; printf 'err\\n' >&2; done; printf done > completed; exit 7",
    ] {
        let started = execute(
            &tools,
            "bash",
            json!({"mode":"background","command":command}),
        );
        let terminal = background_wait(&tools, started["task_id"].as_str().unwrap(), |v| {
            v["status"] != "running"
        });
        assert_eq!(terminal["exit_code"], 7);
        assert_eq!(terminal["truncated"], true);
        assert!(terminal["output"].as_str().unwrap().len() <= 50 * 1024 + 128);
        assert_eq!(terminal["artifact"]["complete"], true);
        assert_eq!(
            fs::read_to_string(dir.path().join("completed")).unwrap(),
            "done"
        );
    }
    let started = execute(
        &tools,
        "bash",
        json!({"mode":"background","command":"printf '\\344'; printf first >&2; while [ ! -f release ]; do sleep 0.01; done; printf '\\270\\255-tail\\360\\237'; printf second >&2; sleep 60"}),
    );
    let id = started["task_id"].as_str().unwrap();
    let partial = background_wait(&tools, id, |v| v["output"] == "first");
    assert_eq!(partial["truncated"], false);
    fs::write(dir.path().join("release"), "go").unwrap();
    background_wait(&tools, id, |v| {
        let text = v["output"].as_str().unwrap();
        text.contains("中-tail") && text.contains("second")
    });
    let stopped = execute(&tools, "bash_task", json!({"task_id":id,"action":"stop"}));
    let text = stopped["output"].as_str().unwrap();
    assert!(
        text.contains("first")
            && text.contains("中-tail")
            && text.contains("second")
            && text.ends_with('�')
    );
    assert_eq!(stopped["truncated"], false);
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
        assert!(terminal["output"].as_str().unwrap().contains("out"));
        assert!(terminal["output"].as_str().unwrap().contains("err"));
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
    tools.artifacts.bind(dir.path(), "large").unwrap();
    let result = execute(
        &tools,
        "bash",
        json!({"command":"for ((i=0;i<10000;i++)); do printf '0123456789'; printf 'abcdefghij' >&2; done"}),
    );
    assert_eq!(result["ok"], true);
    assert_eq!(result["exit_code"], 0);
    assert_eq!(result["total_bytes"], 200000);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["artifact"]["complete"], true);
    assert!(result["output"].as_str().unwrap().len() < 1024);
}

#[test]
fn bash_output_budget_is_shared_by_both_pipes() {
    let workflow = WorkflowState::default();
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    tools.artifacts.bind(dir.path(), "budget").unwrap();
    let mut snapshots = Vec::new();
    let execution = tools.execute("bash",&json!({"command":"for ((i=0;i<10000;i++)); do printf 'out\\n'; printf 'err\\n' >&2; done; printf done > completed; exit 7"}).to_string(),"budget",ToolPolicy::Execute { workflow: &workflow },&CancellationToken::new(),&mut |event| {
        if let AgentEvent::ToolOutputSnapshot { output, .. } = event { snapshots.push(output); }
    });
    assert!(execution.stop_error.is_none());
    let result: Value = serde_json::from_str(&execution.output).unwrap();
    assert!(!snapshots.is_empty());
    assert!(snapshots.iter().all(|s| s.len() <= 50 * 1024 + 128));
    assert_eq!(result["total_bytes"], 80000);
    assert_eq!(result["truncated"], true);
    assert_eq!(result["exit_code"], 7);
    assert_eq!(result["timed_out"], false);
    assert_eq!(result["cancelled"], false);
    assert_eq!(result["artifact"]["complete"], true);
    assert_eq!(
        fs::read_to_string(dir.path().join("completed")).unwrap(),
        "done"
    );
    assert_eq!(
        execute(&tools, "bash", json!({"command":"printf 'next\\n'"}))["output"],
        "next\n"
    );
}

#[test]
fn bash_line_count_no_longer_truncates_at_500() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    for count in [500, 501, 1000] {
        for redirect in ["", " >&2"] {
            let result = execute(
                &tools,
                "bash",
                json!({"command":format!("for ((i=0;i<{count};i++)); do printf 'line\\n'{redirect}; done")}),
            );
            assert_eq!(result["output"], "line\n".repeat(count));
            assert_eq!(result["truncated"], false);
            assert_eq!(result["ok"], true);
        }
    }
}

#[test]
fn bash_deadline_stops_silent_and_term_resistant_process_groups() {
    use std::time::{Duration, Instant};
    // `immediate`: 解释器自己立即以 0 退出（后台子 shell 仍持有输出写端）。
    // 工具报告 shell 的真实终态，并在收尾时回收遗留组，不因此判为超时。
    for (command, immediate) in [
        ("sleep 10; printf late > late", false),
        (
            "trap '' TERM; printf before; printf error >&2; (trap '' TERM; sleep 10; printf late > late) & wait",
            false,
        ),
        ("(sleep 10; printf late > late) & exit 0", true),
        (
            "trap '' TERM; while :; do printf 0123456789; printf abcdefghij >&2; done",
            false,
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        tools.limits.bash_timeout_seconds = 1;
        let start = Instant::now();
        let result = execute(&tools, "bash", json!({"command":command}));
        if immediate {
            assert_eq!(result["ok"], true, "{result}");
            assert_eq!(result["exit_code"], 0, "{result}");
            assert_eq!(result["timed_out"], false, "{result}");
        } else {
            assert_eq!(result["ok"], false, "{result}");
            assert_eq!(result["timed_out"], true, "{result}");
        }
        assert_eq!(result["cancelled"], false, "{result}");
        assert!(start.elapsed() < Duration::from_secs(4));
        assert!(!dir.path().join("late").exists());
        if command.contains("printf before") {
            assert!(result["output"].as_str().unwrap().contains("before"));
            assert!(result["output"].as_str().unwrap().contains("error"));
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
            if matches!(event, AgentEvent::ToolOutputSnapshot { .. }) && !blocked {
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
    for path in result["output"].as_str().unwrap().lines() {
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
            &json!({"command":"sh -c 'trap \"\" TERM; printf \"cancel-ready\\n\"; printf \"%s\" \"$$\" > bash.pid; sleep 30 & child=$!; printf \"%s\" \"$child\" > bash-child.pid; wait \"$child\"'"}).to_string(),
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
    assert!(result["output"].as_str().unwrap().contains("cancel-ready"));
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
