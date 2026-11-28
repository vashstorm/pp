fn runtime_role_save(pty: &mut Pty, role: usize, model: &str) {
    pty.input("/roles\r");
    pty.text("Model roles");
    pty.input("\t");
    for _ in 0..role {
        pty.input("\x1b[B");
    }
    pty.input("\r");
    pty.text("Select a model for");
    pty.input(&format!("{model}\r"));
    pty.text("Choose an effort for this role");
    pty.input("\r");
    pty.wait_for("role saved and back on the role list", |screen| {
        let text = screen.contents();
        text.contains("Model roles")
            && !text.contains("Choose an effort for this role")
            && !text.contains("Saving…")
    });
    pty.input("\x1b");
}

#[test]
fn tui_runtime_role_changes_switch_children_at_their_own_committed_boundary() {
    let protocol = "response";
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("marker.txt"), "child tool marker").unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), "default_model: fixture/one\ntask: {idle_ttl_ms: 1}\nmodel_roles:\n  fast: {model: fixture/one, effort: none}\n").unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            json!({"providers":{"fixture":slash_provider(&fixture,"fixture",protocol,json!({
                "one":{"model":"one"},"two":{"model":"two"},"three":{"model":"three"}
            }))}})
            .to_string(),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &["--yolo"]);
        pty.ready_model("fixture/one");
        pty.input("parent task\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let task =
            json!({"context":"shared", "tasks":[{"role":"fast","task":"child role context"}]});
        let spawn = if protocol == "completion" {
            call("spawn", "task", task)
        } else {
            response_call("spawn", "task", task)
        };
        ask_reply_calls(request, protocol, json!([spawn]));
        let mut child = None;
        let mut parent = None;
        while child.is_none() || parent.is_none() {
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            assert_eq!(body["model"], "one");
            if test_task_tui_child(protocol, &body) {
                child = Some(request);
            } else {
                parent = Some(request);
            }
        }
        runtime_role_save(&mut pty, 0, "three");
        // Saving DEFAULT only changes the configuration default `b`: nothing is cancelled or restarted, and the running
        // session and subagents are not switched.
        no_more(&fixture);
        runtime_role_save(&mut pty, 1, "two");
        no_more(&fixture);
        let read = json!({"path":"marker.txt"});
        let read = if protocol == "completion" {
            call("read-child", "read", read)
        } else {
            response_call("read-child", "read", read)
        };
        ask_reply_calls(child.take().unwrap(), protocol, json!([read]));
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(body["model"], "two");
        let native = body.to_string();
        assert!(native.contains("child role context"));
        assert!(native.contains("read-child"));
        assert!(native.contains("child tool marker"));
        ask_final_reply(request, protocol, "child changed safely");
        ask_final_reply(parent.take().unwrap(), protocol, "Waiting for child.");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(body["model"], "one");
        assert!(body.to_string().contains("child changed safely"));
        ask_final_reply(request, protocol, "parent complete safely");
        pty.text("parent complete safely");
        pty.completed();
        pty.ready_model("fixture/one");
        pty.input("/agents\r");
        pty.text("parked");
        pty.input("e");
        // Type only after the detail view finished loading asynchronously, so the body keys do not go to the roster.
        pty.text("fast / a1");
        pty.input("resume saved child\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(body["model"], "two");
        assert!(body.to_string().contains("child changed safely"));
        assert!(body.to_string().contains("resume saved child"));
        ask_final_reply(request, protocol, "child resumed safely");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(body["model"], "one");
        assert!(body.to_string().contains("child resumed safely"));
        ask_final_reply(request, protocol, "parent resumed safely");
        // The detail view is now its own layer: Esc returns to the roster first, then closes the Hub.
        pty.input("\x1b");
        pty.text("Roster ·");
        pty.input("\x1b");
        pty.text("parent resumed safely");
        pty.completed();
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
}
