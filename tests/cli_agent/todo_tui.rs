const TODO_TUI_BODY: &str = "# Todo integration\n\n## Write\n- [ ] Write canary bytes.\n  - Acceptance: canary.txt contains smoke bytes.\n\n## Verify\n- [ ] Read back canary bytes.\n  - Acceptance: read returns smoke bytes.\n";

fn plan_todo_result(body: &Value, protocol: &str, id: &str) -> Value {
    cancel_tool_outputs(body, protocol)
        .into_iter()
        .find(|(call_id, _)| call_id == id)
        .expect("native todo result")
        .1
}

#[test]
fn plan_todo_approval_initializes_all_contexts_and_reminders_archive_same_bytes() {
    for protocol in ["completion", "response"] {
        for context in [0, 1, 2] {
            let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
            plan_enter(&mut pty, &fixture);
            if context == 2 {
                pty.input("Research previous context for compaction\r");
                let (request, _) = pty.request_at(&fixture, api_path(protocol));
                plan_final(request, protocol, "PRE_COMPACT_RESEARCH_CONTEXT");
                pty.ready();
            }
            plan_drive_ready(
                &mut pty,
                &fixture,
                protocol,
                "exercise todo plan",
                TODO_TUI_BODY,
            );
            let source_id = saved_session_id(home.path());
            let source = plan_journal(home.path(), &source_id);
            let preview = source
                .iter()
                .find(|record| {
                    record["customType"] == "pp.event"
                        && record["data"]["event"]["todo_snapshot"].is_object()
                })
                .expect("saved Preview snapshot");
            let workflow = source
                .iter()
                .rev()
                .find(|record| record["customType"] == "pp.workflow")
                .unwrap();
            assert_eq!(
                workflow["data"]["workflow"]["todos"][0]["tasks"][0]["status"],
                "pending"
            );
            assert_eq!(
                preview["data"]["event"]["todo_snapshot"]["phases"][1]["tasks"][0]["status"],
                "pending"
            );
            assert_eq!(
                std::fs::read_to_string(project.path().join(".pp/drafts/create-canary.md"))
                    .unwrap(),
                TODO_TUI_BODY
            );
            plan_choose(&mut pty, context);
            if context == 2 {
                let (request, summary) = pty.request_at(&fixture, api_path(protocol));
                assert!(
                    summary
                        .to_string()
                        .contains("Summarize the preceding conversation")
                );
                plan_final(request, protocol, "Compact approved planning context.");
            }
            let (request, _) = pty.request_at(&fixture, api_path(protocol));
            let execution_id = if context == 0 {
                resume_boundary_session_ids(home.path())
                    .into_iter()
                    .find(|id| id != &source_id)
                    .unwrap()
            } else {
                source_id.clone()
            };
            let executing = plan_journal(home.path(), &execution_id);
            let workflow = executing
                .iter()
                .rev()
                .find(|record| {
                    matches!(
                        record["customType"].as_str(),
                        Some("pp.workflow" | "pp.session_state")
                    )
                })
                .unwrap();
            assert_eq!(
                workflow["data"]["workflow"]["todos"][0]["tasks"][0]["status"],
                "in_progress"
            );
            plan_calls(
                request,
                protocol,
                json!([
                    plan_call(protocol, "initial-todo", "todo", json!({"op":"view"})),
                    plan_call(
                        protocol,
                        "canary-write",
                        "write",
                        json!({"path":"canary.txt","content":"smoke bytes"})
                    ),
                    plan_call(
                        protocol,
                        "canary-read",
                        "read",
                        json!({"path":"canary.txt"})
                    ),
                    plan_call(
                        protocol,
                        "first-done",
                        "todo",
                        json!({"op":"done","task":"Write canary bytes."})
                    ),
                    plan_call(protocol, "after-done", "todo", json!({"op":"view"}))
                ]),
            );
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            let initial = plan_todo_result(&body, protocol, "initial-todo");
            assert_eq!(initial["phases"][0]["tasks"][0]["status"], "in_progress");
            let after = plan_todo_result(&body, protocol, "after-done");
            assert_eq!(after["phases"][0]["tasks"][0]["status"], "completed");
            assert_eq!(after["phases"][1]["tasks"][0]["status"], "in_progress");
            assert_eq!(
                std::fs::read(project.path().join("canary.txt")).unwrap(),
                b"smoke bytes"
            );
            assert!(
                plan_todo_result(&body, protocol, "canary-read")["content"]
                    .as_str()
                    .unwrap()
                    .contains("smoke bytes")
            );
            assert_eq!(
                std::fs::read_to_string(project.path().join(".pp/plans/create-canary.md")).unwrap(),
                TODO_TUI_BODY.replacen("[ ]", "[x]", 1)
            );
            plan_final(request, protocol, "Work stopped early.");
            let (request, body) = pty.request_at(&fixture, api_path(protocol));
            let history = body[if protocol == "completion" {
                "messages"
            } else {
                "input"
            }]
            .as_array()
            .unwrap();
            let reminder = history
                .iter()
                .rev()
                .find(|item| item["role"] == "developer")
                .unwrap()["content"]
                .as_str()
                .unwrap();
            assert!(reminder.contains("Read back canary bytes."));
            assert!(!reminder.contains("Write canary bytes."));
            plan_calls(
                request,
                protocol,
                json!([plan_call(
                    protocol,
                    "second-done",
                    "todo",
                    json!({"op":"done","task":"Read back canary bytes."})
                )]),
            );
            let (request, _) = pty.request_at(&fixture, api_path(protocol));
            plan_final(request, protocol, "All tasks complete.");
            pty.completed();
            assert!(!project.path().join(".pp/plans/create-canary.md").exists());
            assert_eq!(
                std::fs::read_to_string(project.path().join(".pp/done/create-canary.md")).unwrap(),
                TODO_TUI_BODY.replace("[ ]", "[x]")
            );
            pty.input("/plan view\r");
            pty.wait_for("todo completed plan panel", |screen| {
                let text = screen.contents();
                text.contains("Completed")
                    && text.contains("2/2")
                    && text.contains(".pp/done/create-canary.md")
            });
            pty.input("\x1b");
            no_more(&fixture);
            pty.input("\x03");
            pty.finish();
        }
    }
}

#[test]
fn plan_todo_same_batch_external_checkbox_sync_preserves_runtime_and_duplicate_lookup() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let content =
            "# Duplicate plan\n\n## Work\n- [ ] duplicate\n- [ ] duplicate\n- [ ] last task\n";
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        plan_enter(&mut pty, &fixture);
        plan_drive_ready(&mut pty, &fixture, protocol, "duplicate tasks", content);
        plan_choose(&mut pty, 0);
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        plan_calls(
            request,
            protocol,
            json!([
                plan_call(
                    protocol,
                    "ambiguous",
                    "todo",
                    json!({"op":"done","task":"duplicate"})
                ),
                plan_call(
                    protocol,
                    "second-blocked",
                    "todo",
                    json!({"op":"block","task":"duplicate","occurrence":2,"reason":"external dependency"})
                ),
                plan_call(
                    protocol,
                    "direct-check",
                    "edit",
                    json!({"path":".pp/plans/create-canary.md","edits":[{"oldText":"## Work\n- [ ] duplicate","newText":"## Renamed\n- [x] duplicate"}]})
                ),
                plan_call(protocol, "synced-view", "todo", json!({"op":"view"}))
            ]),
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert_eq!(plan_todo_result(&body, protocol, "ambiguous")["ok"], false);
        let view = plan_todo_result(&body, protocol, "synced-view");
        assert_eq!(view["phases"][0]["name"], "Renamed");
        assert_eq!(view["phases"][0]["tasks"][0]["status"], "completed");
        assert_eq!(view["phases"][0]["tasks"][1]["status"], "blocked");
        assert_eq!(
            view["phases"][0]["tasks"][1]["blocker"],
            "external dependency"
        );
        assert_eq!(view["phases"][0]["tasks"][2]["status"], "in_progress");
        plan_calls(
            request,
            protocol,
            json!([
                plan_call(
                    protocol,
                    "reopen",
                    "todo",
                    json!({"op":"start","task":"duplicate","phase":"Renamed","occurrence":1})
                ),
                plan_call(protocol, "finish", "todo", json!({"op":"done"}))
            ]),
        );
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        plan_final(request, protocol, "Duplicate tasks verified.");
        pty.completed();
        assert_eq!(
            std::fs::read_to_string(project.path().join(".pp/done/create-canary.md")).unwrap(),
            content
                .replace("## Work", "## Renamed")
                .replace("[ ]", "[x]")
        );
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn plan_todo_draft_preview_is_readonly_and_noop_write_does_not_duplicate_card() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        plan_enter(&mut pty, &fixture);
        pty.input("readonly draft preview\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let write = json!({"title":"Todo preview","file_name":"create-canary.md","content":TODO_TUI_BODY,"expected_revision":0});
        let mut calls = vec![plan_call(
            protocol,
            "draft-write",
            "plan_write",
            write.clone(),
        )];
        for op in [
            "init", "start", "done", "block", "unblock", "append", "rm", "drop",
        ] {
            calls.push(plan_call(protocol, &format!("deny-{op}"), "todo", json!({"op":op,"task":"Write canary bytes.","phase":"Write","items":["unapproved"]})));
        }
        let mut no_op = write;
        no_op["expected_revision"] = json!(1);
        calls.push(plan_call(protocol, "noop-write", "plan_write", no_op));
        calls.push(plan_call(
            protocol,
            "draft-view",
            "todo",
            json!({"op":"view"}),
        ));
        calls.push(plan_call(
            protocol,
            "draft-submit",
            "plan_submit",
            json!({"expected_revision":1}),
        ));
        calls.push(plan_call(
            protocol,
            "sealed-view",
            "todo",
            json!({"op":"view"}),
        ));
        plan_calls(request, protocol, json!(calls));
        plan_wait_review(&mut pty, 1);
        no_more(&fixture);
        let records = plan_journal(home.path(), &saved_session_id(home.path()));
        let results: Vec<_> = records
            .iter()
            .filter_map(|record| record["data"]["event"]["tool_finished"].as_object())
            .collect();
        for op in [
            "init", "start", "done", "block", "unblock", "append", "rm", "drop",
        ] {
            let event = results
                .iter()
                .find(|result| result["id"] == format!("deny-{op}"))
                .unwrap();
            assert_eq!(event["result"]["ok"], false, "{event:?}");
            assert_eq!(
                event["result"]["phases"][0]["tasks"][0]["status"],
                "pending"
            );
        }
        let view = results
            .iter()
            .find(|result| result["id"] == "draft-view")
            .unwrap();
        assert_eq!(view["result"]["plan"]["readonly"], true);
        assert_eq!(view["result"]["phases"][1]["tasks"][0]["status"], "pending");
        let sealed = results
            .iter()
            .find(|result| result["id"] == "sealed-view")
            .unwrap();
        assert_eq!(sealed["result"]["ok"], false);
        assert_eq!(
            records
                .iter()
                .filter(|record| record["data"]["event"]["todo_snapshot"].is_object())
                .count(),
            1,
            "same Markdown write must not append a duplicate card"
        );
        assert!(
            !records
                .iter()
                .any(|record| record["data"]["event"]["todo_reminder"].is_object())
        );
        assert_eq!(
            std::fs::read_to_string(project.path().join(".pp/drafts/create-canary.md")).unwrap(),
            TODO_TUI_BODY
        );
        pty.input("\x03");
        pty.finish();
    }
}
