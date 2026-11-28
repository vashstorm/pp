// —— Background task PTY scenario (plan TUI part): a real managed process, PID file fences,
// and visible-screen assertions; static header copy is not under test. ——

// Managed background command: the explicit outer sh ignores TERM, the parent sh and child sleep
// PIDs go into marker files, and the stop path must reap the whole process group with a KILL after grace.
fn background_hold_command(parent_marker: &str, child_marker: &str, log: Option<&str>) -> String {
    let mut script = String::from("trap \"\" TERM; ");
    if let Some(log) = log {
        script.push_str(&format!("printf \"{log}\\n\"; "));
    }
    script.push_str(&format!(
        "printf \"%s\" \"$$\" > {parent_marker}; sleep 30 & child=$!; printf \"%s\" \"$child\" > {child_marker}; wait \"$child\""
    ));
    format!("sh -c '{script}'")
}

// Reply to a single tool call per protocol.
fn background_reply_call(
    request: CapturedRequest,
    protocol: &str,
    id: &str,
    name: &str,
    args: Value,
) {
    if protocol == "completion" {
        calls_reply(request, json!([call(id, name, args)]));
    } else {
        response_reply(request, json!([response_call(id, name, args)]));
    }
}

// Assert each tool result in the request pairs the original call_id exactly once and return its snapshot.
fn background_paired_snapshot(body: &Value, protocol: &str, call_id: &str) -> Value {
    let outputs = cancel_tool_outputs(body, protocol);
    assert_eq!(
        outputs
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        [call_id],
        "a background call must pair its original call_id exactly once"
    );
    outputs[0].1.clone()
}

// Start a managed background task: the PID markers prove the bash parent and child really run; the next
// model request after the start carries the start result and is held (the model stops while awaiting the
// response); the screen must show the task_id and running, and start completion must not count as exit.
// Returns (task_id, start snapshot, held request).
fn background_start_and_wait(
    pty: &mut Pty,
    fixture: &TestServer,
    protocol: &str,
    project: &Path,
    start: &BackgroundStart<'_>,
) -> (String, Value, CapturedRequest) {
    let [parent, child] = start.markers else {
        panic!("a background start needs exactly two PID markers");
    };
    let command = background_hold_command(parent, child, start.log);
    pty.input(&format!("{}\r", start.prompt));
    let (request, _) = pty.request_at(fixture, api_path(protocol));
    background_reply_call(
        request,
        protocol,
        start.call_id,
        "bash",
        json!({"command": command, "mode": "background"}),
    );
    cancel_wait_pid_files(project, start.markers);
    let (request, body) = pty.request_at(fixture, api_path(protocol));
    let snapshot = background_paired_snapshot(&body, protocol, start.call_id);
    let task_id = snapshot["task_id"]
        .as_str()
        .unwrap_or_else(|| panic!("start result is missing task_id: {snapshot}"))
        .to_owned();
    assert_eq!(snapshot["ok"], true);
    assert_eq!(snapshot["status"], "running");
    pty.wait_for("screen shows the background task_id and running state", |screen| {
        cancel_normalize(screen).contains(&format!("Task{task_id}:running"))
    });
    (task_id, snapshot, request)
}

// Input of one background start: the marker list generates both the command and the process-level fence.
struct BackgroundStart<'a> {
    prompt: &'a str,
    call_id: &'a str,
    markers: &'a [&'a str],
    log: Option<&'a str>,
}

// Both protocols start a background task first and hold the next model request while it awaits the response.
// After Esc the parent and child PIDs plus scratch must be cleaned before Ready, leaving only the cancel
// status line; the next user task runs normally without replaying the old start call.
#[test]
fn background_esc_cancels_managed_tasks_and_cleans_before_ready_without_replay() {
    let protocol = "response";
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), CANCEL_SETTINGS).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let path = api_path(protocol);
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        let start = BackgroundStart {
            prompt: "background esc prompt",
            call_id: "bg-esc-start",
            markers: &["bg-esc-parent.pid", "bg-esc-child.pid"],
            log: None,
        };
        let (task_id, _snapshot, held) =
            background_start_and_wait(&mut pty, &fixture, protocol, project.path(), &start);
        let parent_before = std::fs::read(project.path().join("bg-esc-parent.pid")).unwrap();
        let child_before = std::fs::read(project.path().join("bg-esc-child.pid")).unwrap();
        pty.input("\x1b");
        // Verify the background parent and child PIDs plus scratch are cleaned before observing Ready and the cancel status line.
        cancel_wait_pid_dead(&project.path().join("bg-esc-parent.pid"));
        cancel_wait_pid_dead(&project.path().join("bg-esc-child.pid"));
        cancel_assert_no_scratch(project.path());
        cancel_wait_cancelled_ready(&mut pty);
        drop(held);
        // Completing the cancel triggers no model request.
        cancel_assert_no_request(&fixture);
        // The next user task runs normally and the old start call is not replayed.
        pty.input("background esc next\r");
        let (request, body) = pty.request_at(&fixture, path);
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["background esc prompt", "background esc next"]
        );
        assert_eq!(cancel_called_ids(&body, protocol), ["bg-esc-start"]);
        let replayed = background_paired_snapshot(&body, protocol, "bg-esc-start");
        assert_eq!(replayed["task_id"], task_id.as_str());
        let text = serde_json::to_string(&body).unwrap();
        assert!(!text.contains("pp.cancelled"), "pp.cancelled must not reach the request");
        if protocol == "completion" {
            final_reply(request, "background esc recovered");
        } else {
            response_final(request, "background esc recovered");
        }
        pty.text("background esc recovered");
        pty.ready();
        pty.input("/quit\r");
        pty.finish();
        cancel_assert_no_request(&fixture);
        // The old tool was never replayed: the PID markers keep their cancelled bytes, the process stays stopped, no new scratch.
        assert_eq!(
            std::fs::read(project.path().join("bg-esc-parent.pid")).unwrap(),
            parent_before
        );
        assert_eq!(
            std::fs::read(project.path().join("bg-esc-child.pid")).unwrap(),
            child_before
        );
        cancel_assert_no_scratch(project.path());
}

