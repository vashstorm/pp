use super::*;
use crate::agent::tests::{completion_stream, final_for, response_stream, runtime_input_agent};
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
fn read_prompt_schema_and_image_gate_are_shared_by_plan_and_review() {
    for mode in [EditMode::Replace, EditMode::Hashline] {
        let dir = tempfile::tempdir().unwrap();
        let tools = Tools::new(dir.path().into(), mode).unwrap();
        let definitions = tools.all_definitions(crate::plan::WorkMode::Execute);
        let reference = definitions.iter().find(|tool| tool.name == "read").unwrap();
        for definitions in [
            tools.all_definitions(crate::plan::WorkMode::Plan),
            tools.review_definitions(),
        ] {
            let read = definitions.iter().find(|tool| tool.name == "read").unwrap();
            assert_eq!(read.parameters, reference.parameters);
            assert_eq!(read.description, reference.description);
        }
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
