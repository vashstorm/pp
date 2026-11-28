use super::*;

fn todo_call(protocol: &str, id: &str, arguments: Value) -> Value {
    match protocol {
        "completion" => call(id, "todo", arguments),
        _ => response_call(id, "todo", arguments),
    }
}

fn todo_calls(request: CapturedRequest, protocol: &str, calls: Value) {
    match protocol {
        "completion" => calls_reply(request, calls),
        _ => response_reply(request, calls),
    }
}

fn todo_receive(server: &TestServer, protocol: &str) -> (CapturedRequest, Value) {
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
    let protocol = "response";
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
                json!({"op":"init","list":[{"phase":"First","items":["A","B"]},{"phase":"Second","items":["C"]}],"occurrence":1})
            ),
            todo_call(protocol, "start-b", json!({"op":"start","task":"B"})),
            todo_call(protocol, "start-c", json!({"op":"start","task":"C"})),
            todo_call(protocol, "done-b", json!({"op":"done","task":"B"})),
            todo_call(protocol, "view", json!({"op":"view"}))
        ]),
    );
    let (request, body) = todo_receive(&fixture, protocol);
    let started = todo_result(&body, protocol, "start-c");
    assert!(started.get("phases").is_none());
    let started_content = started["content"].as_str().unwrap();
    assert!(started_content.contains("- [ ] A (in progress)"));
    assert!(started_content.contains("- [ ] B (in progress)"));
    assert!(started_content.contains("- [ ] C (in progress)"));
    let viewed = todo_result(&body, protocol, "view");
    assert!(viewed.get("phases").is_none());
    let viewed_content = viewed["content"].as_str().unwrap();
    assert!(viewed_content.contains("- [ ] A (in progress)"));
    assert!(viewed_content.contains("- [X] B"));
    assert!(viewed_content.contains("- [ ] C (in progress)"));
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
    assert!(viewed.get("phases").is_none());
    let viewed_content = viewed["content"].as_str().unwrap();
    assert!(viewed_content.contains("- [X] A"));
    assert!(viewed_content.contains("- [X] B"));
    assert!(viewed_content.contains("- [X] C"));
    assert!(!viewed_content.contains("- [ ] D"));
    assert_eq!(todo_result(&body, protocol, "reject")["ok"], false);
    assert_eq!(todo_result(&body, protocol, "occurrence")["ok"], true);
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
    assert!(
        todo_result(&body, protocol, "done-c")["content"]
            .as_str()
            .unwrap()
            .contains("- [X] C")
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
    let journal = records(&path);
    let workflow = journal
        .into_iter()
        .rfind(|entry| entry["customType"] == "pp.workflow")
        .unwrap();
    assert!(workflow["data"]["workflow"]["todo_plan_id"].is_null());
    assert_eq!(
        workflow["data"]["workflow"]["todos"][1]["tasks"][0]["status"],
        "completed"
    );
    assert!(!home.path().join(".pp/plans").exists());
    let id = path.file_stem().unwrap().to_str().unwrap();
    let resumed = Cli::spawn(
        home.path(),
        &[
            "--allowed-tools",
            "Todo",
            "-r",
            id,
            "-p",
            "view saved todos",
        ],
        &[],
    );
    let (request, _) = todo_receive(&fixture, protocol);
    todo_calls(
        request,
        protocol,
        json!([todo_call(protocol, "restored-view", json!({"op":"view"}))]),
    );
    let (request, body) = todo_receive(&fixture, protocol);
    let restored = todo_result(&body, protocol, "restored-view");
    assert!(restored.get("phases").is_none());
    let restored_content = restored["content"].as_str().unwrap();
    assert!(restored_content.contains("Overall: 3/3 done, 0 open."));
    assert!(restored_content.contains("- [X] C"));
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
