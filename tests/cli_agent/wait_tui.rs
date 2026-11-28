// Verified through a real PTY: wait refresh, Steering, cancellation and result write-back for both protocols.
fn wait_tui_start(
    protocol: &str,
    fixture: &TestServer,
    home: &std::path::Path,
    project: &std::path::Path,
) -> (Pty, String, CapturedRequest) {
    std::fs::write(config_path(home, "settings.yml"), CANCEL_SETTINGS).unwrap();
    std::fs::write(
        config_path(home, "models.yml"),
        response_models(fixture, Some(protocol)),
    )
    .unwrap();
    let mut pty = Pty::start_with_args(project, home, &[]);
    pty.ready();
    pty.input("wait lifecycle\r");
    let (request, _) = pty.request_at(fixture, api_path(protocol));
    background_reply_call(
        request,
        protocol,
        "wait-start",
        "bash",
        json!({"command":"sh -c 'printf \"%s\" \"$$\" > wait.pid; printf \"wait-start-log\\n\"; while [ ! -f wait-release ]; do sleep 0.01; done; printf \"wait-complete-log\\n\"'","mode":"background"}),
    );
    let (request, body) = pty.request_at(fixture, api_path(protocol));
    let result = cancel_tool_outputs(&body, protocol)
        .into_iter()
        .find(|(id, _)| id == "wait-start")
        .unwrap()
        .1;
    let id = result["task_id"].as_str().unwrap().to_owned();
    (pty, id, request)
}

fn wait_subagent_settings() -> String {
    let mut settings: Value = serde_json::from_str(CANCEL_SETTINGS).unwrap();
    settings["model_roles"] = json!({"fast":{"model":"fixture/primary","effort":"none"}});
    settings.to_string()
}

#[test]
fn wait_tui_esc_reaps_subagent_background_process_before_restoring_input() {
    let protocol = "response";
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), wait_subagent_settings()).unwrap();
        std::fs::write(config_path(home.path(), "models.yml"), response_models(&fixture, Some(protocol))).unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &["--yolo"]);
        pty.ready();
        pty.input("cancel child batch\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        background_reply_call(request, protocol, "spawn", "task", json!({"context":"cancel owned work", "tasks":[{"role":"fast","task":"child background service"}]}));
        let mut parent = None;
        let mut child = None;
        while parent.is_none() || child.is_none() {
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            if hub_is_child(&body) { child = Some(request); } else { parent = Some(request); }
        }
        background_reply_call(child.take().unwrap(), protocol, "child-bg", "bash",
            json!({"command":"sh -c 'printf %s $$ > child-wait.pid; while :; do sleep 0.01; done'","mode":"background"}));
        let (child_request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(hub_is_child(&body));
        background_reply_call(parent.take().unwrap(), protocol, "child-wait", "wait", json!({"run_ids":["r1"]}));
        pty.text("0/1 settled");
        cancel_wait_pid_files(project.path(), &["child-wait.pid"]);
        pty.input("\x1b");
        cancel_wait_pid_dead(&project.path().join("child-wait.pid"));
        cancel_wait_cancelled_ready(&mut pty);
        pty.wait_for("cancellation terminal state shown once the subagent really wraps up", |screen| {
            let text = screen.contents();
            text.contains("1/1 settled") && text.contains("[task] r1 · cancelled")
        });
        let registry = hub_registry(home.path());
        assert_eq!(registry["runs"]["r1"]["status"], "cancelled");
        drop(child_request);
        cancel_assert_no_request(&fixture);
        pty.input("/quit\r");
        pty.finish();
}
