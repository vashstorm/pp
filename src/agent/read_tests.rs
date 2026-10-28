use super::*;
use crate::agent::tests::{
    completion_stream, final_for, response_stream, runtime_input_agent, seed_session,
};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

#[test]
fn read_image_tree_navigation_keeps_complete_native_attachment_without_new_user_node() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        image::DynamicImage::new_rgb8(1, 1)
            .save(dir.path().join("fixture.png"))
            .unwrap();
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").unwrap());
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        let peer = Arc::clone(&server);
        let worker = std::thread::spawn(move || {
            let request = peer.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            let arguments = json!({"path":"fixture.png"}).to_string();
            if protocol == "completion" {
                completion_stream(
                    request,
                    json!({"role":"assistant","content":null,"tool_calls":[{"id":"image","index":0,"type":"function","function":{"name":"read","arguments":arguments}}]}),
                    "tool_calls",
                );
            } else {
                response_stream(
                    request,
                    &[
                        json!({"type":"function_call","id":"fc-image","call_id":"image","name":"read","arguments":arguments,"status":"completed"}),
                    ],
                    true,
                );
            }
            let request = peer.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            final_for(protocol, request, "image read", true);
            let request = peer.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
            final_for(protocol, request, "second task", true);
        });
        agent
            .run(
                "look at image",
                &CancellationToken::new(),
                None,
                &mut |_| {},
            )
            .unwrap();
        agent
            .run("another task", &CancellationToken::new(), None, &mut |_| {})
            .unwrap();
        worker.join().unwrap();
        let view = agent.session_tree().unwrap();
        assert_eq!(
            view.tree
                .nodes
                .iter()
                .filter(|node| node.kind == persistence::tree::Kind::User)
                .count(),
            2
        );
        let target = view
            .tree
            .nodes
            .iter()
            .find(|node| node.kind == persistence::tree::Kind::Tools)
            .unwrap()
            .id
            .clone();
        std::fs::remove_file(dir.path().join("fixture.png")).unwrap();
        agent.navigate_tree(&target).unwrap();
        let native = serde_json::to_value(&agent.session).unwrap();
        assert!(native.to_string().contains("data:image/png;base64,"));
        match &agent.session {
            Session::Completion(request) => {
                tool_images::validate_chat(&request.messages).unwrap();
                assert_eq!(
                    tool_images::chat_id(request.messages.last().unwrap()),
                    Some("image")
                );
                assert_eq!(
                    context::completion_boundaries(&request.messages)
                        .unwrap()
                        .last(),
                    Some(&request.messages.len())
                );
            }
            Session::Response(request) => {
                let InputParam::Items(items) = &request.input else {
                    panic!()
                };
                assert_eq!(
                    context::response_boundaries(items).unwrap().last(),
                    Some(&items.len())
                );
            }
        }
        assert!(server.try_recv().unwrap().is_none());
        let view = agent.session_tree().unwrap();
        assert_eq!(
            view.tree
                .nodes
                .iter()
                .filter(|node| node.kind == persistence::tree::Kind::User)
                .count(),
            1
        );
    }
}

#[test]
fn read_image_execution_is_allowed_in_plan_and_review() {
    for mode in [EditMode::Replace, EditMode::Hashline] {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), mode).unwrap();
        image::DynamicImage::new_rgb8(1, 1)
            .save(dir.path().join("fixture.png"))
            .unwrap();
        let document = crate::plan::PlanDocument::new().unwrap();
        for policy in [
            crate::tools::ToolPolicy::Review,
            crate::tools::ToolPolicy::Plan {
                document: &document,
                sealed: false,
                input: None,
            },
        ] {
            let mut events = Vec::new();
            let result = tools.execute_with_fetch(
                "read",
                r#"{"path":"fixture.png"}"#,
                "image",
                policy,
                &CancellationToken::new(),
                &mut |event| events.push(event),
                &mut |_, _| unreachable!(),
            );
            assert_eq!(result.images.len(), 1);
            let output: Value = serde_json::from_str(&result.output).unwrap();
            assert_eq!(output["kind"], "image");
            assert!(!serde_json::to_string(&events).unwrap().contains("base64"));
        }
    }
}

#[test]
fn skill_read_execution_and_disablement_apply_to_every_workflow() {
    for mode in [EditMode::Replace, EditMode::Hashline] {
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let registry_dir = home.path().join(".pp/agent");
        let skill_dir = registry_dir.join("skills/workflow");
        std::fs::create_dir_all(&skill_dir).unwrap();
        let body = "---\nname: workflow\ndescription: Workflow guidance\n---\nRead workflow guidance.\nSKILL_BODY_END\n";
        let skill_path = skill_dir.join("SKILL.md");
        std::fs::write(&skill_path, body).unwrap();
        let script = format!("touch '{}'\n", dir.path().join("executed").display());
        std::fs::write(skill_dir.join("run.sh"), &script).unwrap();
        let mut tools = Tools::new(dir.path().into(), mode).unwrap();
        tools.skills = crate::skills::Skills::from_dir(&registry_dir).shared();
        let document = crate::plan::PlanDocument::new().unwrap();
        let execute_workflow = crate::plan::WorkflowState::default();
        for workflow in ["execute", "plan", "review"] {
            let policy = || match workflow {
                "execute" => crate::tools::ToolPolicy::Execute {
                    workflow: &execute_workflow,
                },
                "plan" => crate::tools::ToolPolicy::Plan {
                    document: &document,
                    sealed: false,
                    input: None,
                },
                "review" => crate::tools::ToolPolicy::Review,
                _ => unreachable!(),
            };
            for (uri, expected) in [
                ("skill://workflow", body),
                ("skill://workflow/run.sh", script.as_str()),
            ] {
                let execution = tools.execute_with_fetch(
                    "read",
                    &json!({"path":format!("{uri}:raw")}).to_string(),
                    "skill",
                    policy(),
                    &CancellationToken::new(),
                    &mut |_| {},
                    &mut |_, _| unreachable!(),
                );
                let result: Value = serde_json::from_str(&execution.output).unwrap();
                assert_eq!(result["ok"], true, "{workflow}: {result}");
                assert_eq!(result["source"], uri);
                assert_eq!(result["content"], expected);
                assert_eq!(result["truncated"], false);
                assert_eq!(
                    result["base_dir"],
                    std::fs::canonicalize(&skill_dir).unwrap().to_str().unwrap()
                );
            }
            assert!(!dir.path().join("executed").exists());
            tools
                .skills
                .lock()
                .unwrap()
                .save_selection(&[("workflow".into(), false)])
                .unwrap();
            let execution = tools.execute_with_fetch(
                "read",
                r#"{"path":"skill://workflow"}"#,
                "disabled",
                policy(),
                &CancellationToken::new(),
                &mut |_| {},
                &mut |_, _| unreachable!(),
            );
            let result: Value = serde_json::from_str(&execution.output).unwrap();
            assert_eq!(result["ok"], false, "{workflow}: {result}");
            assert_eq!(result["error"], "Skill does not exist or is disabled");
            tools
                .skills
                .lock()
                .unwrap()
                .save_selection(&[("workflow".into(), true)])
                .unwrap();
        }

        for (name, arguments) in [
            (
                "write",
                json!({"path":"skill://workflow","content":"changed"}).to_string(),
            ),
            (
                "edit",
                json!({"path":"skill://workflow","edits":[]}).to_string(),
            ),
            (
                "bash",
                json!({"command":"sh skill://workflow/run.sh"}).to_string(),
            ),
            (
                "mcp__fixture__run",
                json!({"path":"skill://workflow/run.sh"}).to_string(),
            ),
            ("write", "{".into()),
            ("edit", "{".into()),
            ("bash", "{".into()),
            ("mcp__fixture__run", "{".into()),
        ] {
            for (policy, error) in [
                (
                    crate::tools::ToolPolicy::Review,
                    "Tool is not available during Plan review.",
                ),
                (
                    crate::tools::ToolPolicy::Plan {
                        document: &document,
                        sealed: false,
                        input: None,
                    },
                    "Tool is not available in Plan mode.",
                ),
            ] {
                let execution = tools.execute_with_fetch(
                    name,
                    &arguments,
                    "denied",
                    policy,
                    &CancellationToken::new(),
                    &mut |_| {},
                    &mut |_, _| unreachable!(),
                );
                let result: Value = serde_json::from_str(&execution.output).unwrap();
                assert_eq!(result["ok"], false, "{name}: {result}");
                assert_eq!(result["error"], error, "{name}: {result}");
            }
        }
        assert_eq!(std::fs::read_to_string(skill_path).unwrap(), body);
        assert!(!dir.path().join("skill:").exists());
        assert!(!dir.path().join("executed").exists());
    }
}

#[test]
fn read_history_restores_unknown_tool_results_without_special_handling() {
    for protocol in ["completion", "response"] {
        let dir = tempfile::tempdir().unwrap();
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let mut agent =
            runtime_input_agent(dir.path(), protocol, &server.server_addr().to_string(), "");
        seed_session(&mut agent, "restore an archived tool result");
        let payload = json!({
            "ok": true,
            "content": "---\narchived-tool-body\n---\nARCHIVED_BODY_END\n",
            "metadata": {"source":"archive://fixture","values":[1,"verbatim",true]}
        });
        let arguments = json!({"resource":"archived fixture"}).to_string();
        match &mut agent.session {
            Session::Completion(request) => {
                for message in [
                    json!({"role":"assistant","content":null,"tool_calls":[{"id":"archived","type":"function","function":{"name":"future_archive_tool","arguments":arguments}}]}),
                    json!({"role":"tool","tool_call_id":"archived","content":payload.to_string()}),
                ] {
                    request
                        .messages
                        .push(serde_json::from_value(message).unwrap());
                }
            }
            Session::Response(request) => {
                for item in [
                    json!({"type":"function_call","id":"fc-archived","call_id":"archived","name":"future_archive_tool","arguments":arguments,"status":"completed"}),
                    json!({"type":"function_call_output","call_id":"archived","output":payload.to_string()}),
                ] {
                    response_input(request).push(serde_json::from_value(item).unwrap());
                }
            }
        }
        agent.journal.event(&AgentEvent::ToolStarted {
            id: "archived".into(),
            name: "future_archive_tool".into(),
            arguments: arguments.clone(),
        });
        agent.journal.event(&AgentEvent::ToolFinished {
            id: "archived".into(),
            result: payload.clone(),
        });
        agent.journal.commit((&agent.session).into()).unwrap();
        let id = agent.session_id().unwrap().to_owned();
        drop(agent);
        let saved = persistence::load(&dir.path().join("sessions"), Some(&id), true).unwrap();
        let mut restored = Agent::restore(
            ModelCatalog::from_files(
                &dir.path().join("settings.yml"),
                &dir.path().join("models.yml"),
            )
            .unwrap(),
            Tools::new(dir.path().into(), EditMode::Replace).unwrap(),
            saved,
        )
        .unwrap();
        let replay = restored.take_replay();
        assert!(replay.iter().any(|record| matches!(
            record,
            persistence::ReplayRecord::Event(AgentEvent::ToolStarted { name, arguments: saved_arguments, .. })
                if name == "future_archive_tool" && saved_arguments == &arguments
        )));
        assert!(replay.iter().any(|record| matches!(
            record,
            persistence::ReplayRecord::Event(AgentEvent::ToolFinished { id, result })
                if id == "archived" && result == &payload
        )));
        let native = serde_json::to_value(&restored.session).unwrap().to_string();
        assert!(native.contains("future_archive_tool"));
        assert!(native.contains("ARCHIVED_BODY_END"));
        assert!(server.try_recv().unwrap().is_none());
    }
}
