use super::*;

fn todo_call(protocol: &str, id: &str, arguments: Value) -> Value {
    match protocol {
        "completion" => call(id, "todo", arguments),
        _ => response_call(id, "todo", arguments),
    }
}

fn todo_calls(request: Request, protocol: &str, calls: Value) {
    match protocol {
        "completion" => calls_reply(request, calls),
        _ => response_reply(request, calls),
    }
}

fn todo_receive(server: &Server, protocol: &str) -> (Request, Value) {
    receive_at(
        server,
        if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        },
    )
}

fn todo_result(body: &Value, protocol: &str, id: &str) -> Value {
    let history = body[if protocol == "completion" {
        "messages"
    } else {
        "input"
    }]
    .as_array()
    .unwrap();
    let item = history
        .iter()
        .find(|item| {
            if protocol == "completion" {
                item["role"] == "tool" && item["tool_call_id"] == id
            } else {
                item["type"] == "function_call_output" && item["call_id"] == id
            }
        })
        .expect("todo result in native history");
    if protocol == "completion" {
        result(item, id)
    } else {
        response_result(item, id)
    }
}

#[test]
fn todo_native_batches_commit_each_transition_and_resume_standalone_state() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let models = response_models(&fixture, Some(protocol));
        let cli = Cli::start_with(
            home.path(),
            Some("exercise standalone todos"),
            fixture_settings(),
            &models,
        );
        let (request, _) = todo_receive(&fixture, protocol);
        todo_calls(
            request,
            protocol,
            json!([
                todo_call(
                    protocol,
                    "init",
                    json!({"op":"init","list":[{"phase":"First","items":["A","B"]},{"phase":"Second","items":["C"]}]})
                ),
                todo_call(protocol, "done-b", json!({"op":"done","task":"B"})),
                todo_call(protocol, "view", json!({"op":"view"}))
            ]),
        );
        let (request, body) = todo_receive(&fixture, protocol);
        let viewed = todo_result(&body, protocol, "view");
        assert_eq!(viewed["phases"][0]["tasks"][0]["status"], "in_progress");
        assert_eq!(viewed["phases"][0]["tasks"][1]["status"], "completed");
        assert_eq!(viewed["phases"][1]["tasks"][0]["status"], "pending");
        todo_calls(
            request,
            protocol,
            json!([
                todo_call(protocol, "done-a", json!({"op":"done","task":"A"})),
                todo_call(
                    protocol,
                    "reject",
                    json!({"op":"append","phase":"Second","items":["D","C"]})
                ),
                todo_call(
                    protocol,
                    "occurrence",
                    json!({"op":"done","task":"C","occurrence":1})
                ),
                todo_call(protocol, "view-again", json!({"op":"view"}))
            ]),
        );
        let (request, body) = todo_receive(&fixture, protocol);
        let viewed = todo_result(&body, protocol, "view-again");
        assert_eq!(viewed["phases"][0]["tasks"][0]["status"], "completed");
        assert_eq!(viewed["phases"][0]["tasks"][1]["status"], "completed");
        assert_eq!(
            viewed["phases"][1]["tasks"],
            json!([{"content":"C","status":"in_progress"}])
        );
        assert_eq!(todo_result(&body, protocol, "reject")["ok"], false);
        assert_eq!(
            todo_result(&body, protocol, "occurrence")["error"],
            "occurrence is only available for plan tasks."
        );
        todo_calls(
            request,
            protocol,
            json!([todo_call(
                protocol,
                "done-c",
                json!({"op":"done","task":"C"})
            )]),
        );
        let (request, body) = todo_receive(&fixture, protocol);
        assert_eq!(
            todo_result(&body, protocol, "done-c")["phases"][1]["tasks"][0]["status"],
            "completed"
        );
        if protocol == "completion" {
            final_reply(request, "Standalone tasks finished.");
        } else {
            response_final(request, "Standalone tasks finished.");
        }
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            "Standalone tasks finished."
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("C"));
        let sessions = home.path().join(".pp/agent/sessions");
        let path = std::fs::read_dir(&sessions)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .unwrap();
        let journal = std::fs::read_to_string(&path).unwrap();
        let workflow = journal
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .rfind(|entry| entry["customType"] == "pp.workflow")
            .unwrap();
        assert!(workflow["data"]["workflow"]["todo_plan_id"].is_null());
        assert_eq!(
            workflow["data"]["workflow"]["todos"][1]["tasks"][0]["status"],
            "completed"
        );
        assert!(!home.path().join(".pp/plans").exists());
        let id = path.file_stem().unwrap().to_str().unwrap();
        let resumed = Cli::spawn(home.path(), &["-r", id, "-p", "view saved todos"], &[]);
        let (request, _) = todo_receive(&fixture, protocol);
        todo_calls(
            request,
            protocol,
            json!([todo_call(protocol, "restored-view", json!({"op":"view"}))]),
        );
        let (request, body) = todo_receive(&fixture, protocol);
        let restored = todo_result(&body, protocol, "restored-view");
        assert_eq!(restored["phases"], workflow["data"]["workflow"]["todos"]);
        if protocol == "completion" {
            final_reply(request, "Saved standalone todos restored.");
        } else {
            response_final(request, "Saved standalone todos restored.");
        }
        let output = resumed.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        no_more(&fixture);
    }
}

fn todo_final(request: Request, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

fn todo_developer_reminders(body: &Value, protocol: &str) -> Vec<String> {
    body[if protocol == "completion" {
        "messages"
    } else {
        "input"
    }]
    .as_array()
    .unwrap()
    .iter()
    .filter(|item| item["role"] == "developer")
    .filter_map(|item| item["content"].as_str().map(str::to_owned))
    .filter(|text| text.contains("<system-reminder>"))
    .collect()
}

#[test]
fn todo_completion_reminder_is_developer_and_only_lists_open_work() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let cli = Cli::start_with(
            home.path(),
            Some("complete every todo"),
            fixture_settings(),
            &response_models(&fixture, Some(protocol)),
        );
        let (request, _) = todo_receive(&fixture, protocol);
        todo_calls(
            request,
            protocol,
            json!([
                todo_call(
                    protocol,
                    "init",
                    json!({"op":"init","items":["Finished A","Open B"]})
                ),
                todo_call(protocol, "done-a", json!({"op":"done","task":"Finished A"}))
            ]),
        );
        let (request, _) = todo_receive(&fixture, protocol);
        todo_final(request, protocol, "Work stopped early.");
        let (request, body) = todo_receive(&fixture, protocol);
        let reminders = todo_developer_reminders(&body, protocol);
        assert_eq!(reminders.len(), 1);
        assert!(reminders[0].contains("Open B"));
        assert!(!reminders[0].contains("Finished A"));
        todo_calls(
            request,
            protocol,
            json!([todo_call(
                protocol,
                "done-b",
                json!({"op":"done","task":"Open B"})
            )]),
        );
        let (request, body) = todo_receive(&fixture, protocol);
        assert_eq!(
            todo_result(&body, protocol, "done-b")["phases"][0]["tasks"][1]["status"],
            "completed"
        );
        todo_final(request, protocol, "All tasks complete.");
        let output = cli.finish();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("1 incomplete todos - reminder 1/3"),
            "{stderr}"
        );
        assert!(
            !String::from_utf8(output.stdout)
                .unwrap()
                .contains("<system-reminder>")
        );
        no_more(&fixture);
    }
}

#[test]
fn todo_reminder_latch_and_three_attempt_budget_bound_continuation() {
    for protocol in ["completion", "response"] {
        for tools_between in [false, true] {
            let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let cli = Cli::start_with(
                home.path(),
                Some("exercise reminder bounds"),
                fixture_settings(),
                &response_models(&fixture, Some(protocol)),
            );
            let (request, _) = todo_receive(&fixture, protocol);
            todo_calls(
                request,
                protocol,
                json!([todo_call(
                    protocol,
                    "init",
                    json!({"op":"init","items":["Unfinished"]})
                )]),
            );
            let (mut request, _) = todo_receive(&fixture, protocol);
            let maximum = if tools_between { 3 } else { 1 };
            for attempt in 1..=maximum {
                todo_final(request, protocol, "Work stopped early.");
                let (next, body) = todo_receive(&fixture, protocol);
                assert_eq!(todo_developer_reminders(&body, protocol).len(), attempt);
                if tools_between {
                    todo_calls(
                        next,
                        protocol,
                        json!([todo_call(
                            protocol,
                            &format!("view-{attempt}"),
                            json!({"op":"view"})
                        )]),
                    );
                    (request, _) = todo_receive(&fixture, protocol);
                } else {
                    request = next;
                }
            }
            todo_final(request, protocol, "Work stopped early.");
            assert!(cli.finish().status.success());
            no_more(&fixture);
        }
    }
}

#[test]
fn todo_waiting_questions_and_all_blocked_lists_do_not_remind() {
    for protocol in ["completion", "response"] {
        for (text, block) in [("**请选择继续方式？**", false), ("Work is blocked.", true)] {
            let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let cli = Cli::start_with(
                home.path(),
                Some("exercise waiting"),
                fixture_settings(),
                &response_models(&fixture, Some(protocol)),
            );
            let (request, _) = todo_receive(&fixture, protocol);
            let mut calls = vec![todo_call(
                protocol,
                "init",
                json!({"op":"init","items":["Needs response"]}),
            )];
            if block {
                calls.push(todo_call(
                    protocol,
                    "block",
                    json!({"op":"block","task":"Needs response","reason":"waiting for user"}),
                ));
            }
            todo_calls(request, protocol, json!(calls));
            let (request, _) = todo_receive(&fixture, protocol);
            todo_final(request, protocol, text);
            assert!(cli.finish().status.success());
            no_more(&fixture);
        }
    }
}
