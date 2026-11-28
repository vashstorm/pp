// Headless ask contract (the plan's verification "headless contract" row): `-p` and piped line mode, and the
// requests of both protocols, must not carry the ask schema; when the fixture still returns an ask call the
// fixed `ask requires interactive TUI mode` tool error comes back, the result enters history paired with its
// original call id, nothing hangs and the next stdin line is not consumed as an answer. Everything is observed
// through the real child process, loopback HTTP and HTTP barriers, never sleep to guess stages. Helpers are all
// named with the `ask_` prefix.

use super::streaming::{LiveCli, Pipe};
use super::*;

fn ask_receive(server: &TestServer, protocol: &str) -> (CapturedRequest, Value) {
    receive_at(
        server,
        if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        },
    )
}

// Whether the ask schema appears in the request tools (Chat under tools[].function, Responses under tools[]).
fn ask_schema_offered(body: &Value) -> bool {
    body["tools"].as_array().is_some_and(|tools| {
        tools
            .iter()
            .any(|tool| tool["function"]["name"] == "ask" || tool["name"] == "ask")
    })
}

fn ask_base_tools_offered(body: &Value) -> bool {
    body["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty())
}

// Return (call_id, result JSON) in history order, covering the paired tool results of both protocols.
fn ask_tool_outputs(body: &Value, protocol: &str) -> Vec<(String, Value)> {
    let items = body[if protocol == "completion" {
        "messages"
    } else {
        "input"
    }]
    .as_array()
    .unwrap();
    items
        .iter()
        .filter_map(|item| {
            if protocol == "completion" && item["role"] == "tool" {
                let id = item["tool_call_id"].as_str().unwrap();
                Some((id.to_owned(), result(item, id)))
            } else if protocol == "response" && item["type"] == "function_call_output" {
                let id = item["call_id"].as_str().unwrap();
                Some((id.to_owned(), response_result(item, id)))
            } else {
                None
            }
        })
        .collect()
}

fn ask_user_texts(body: &Value, protocol: &str) -> Vec<String> {
    let items = body[if protocol == "completion" {
        "messages"
    } else {
        "input"
    }]
    .as_array()
    .unwrap();
    items
        .iter()
        .filter(|item| item["role"] == "user")
        .map(slash_message_text)
        .collect()
}

fn ask_call(protocol: &str, id: &str, arguments: Value) -> Value {
    if protocol == "completion" {
        call(id, "ask", arguments)
    } else {
        response_call(id, "ask", arguments)
    }
}

fn ask_calls_reply(request: CapturedRequest, protocol: &str, calls: Value) {
    if protocol == "completion" {
        calls_reply(request, calls);
    } else {
        response_reply(request, calls);
    }
}

fn ask_final(request: CapturedRequest, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

// The arguments themselves are fully valid: the error can only come from the missing interactive handler, not
// from argument parsing.
fn ask_headless_arguments() -> Value {
    json!({"questions":[{"id":"storage","question":"Which database?","options":[{"label":"SQLite"},{"label":"Postgres"}],"recommended":0}]})
}

#[test]
fn ask_headless_omits_schema_and_answers_fixed_error_without_consuming_stdin() {
    let protocol = "response";
    for mode in ["prompt", "lines"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = match protocol {
            "completion" => fixture_models(&server),
            _ => response_models(&server, Some("response")),
        };
        let prompt = (mode == "prompt").then_some("headless ask task one");
        let mut cli = LiveCli::new(Cli::start_with(
            dir.path(),
            prompt,
            fixture_settings(),
            &models,
        ));
        if mode == "lines" {
            // Both task lines are written at once: if ask handling consumed the next line, the second task would not
            // show up as its own user message.
            cli.input("headless ask task one\nheadless ask task two\n");
        }

        // First request: the tool list exists but does not contain ask.
        let (request, body) = ask_receive(&server, protocol);
        assert_eq!(
            ask_user_texts(&body, protocol),
            ["headless ask task one"],
            "protocol {protocol} mode {mode}"
        );
        assert!(
            ask_base_tools_offered(&body),
            "base tools must still be offered: {body}"
        );
        assert!(
            !ask_schema_offered(&body),
            "headless must not offer ask: {body}"
        );
        ask_calls_reply(
            request,
            protocol,
            json!([ask_call(protocol, "headless-ask", ask_headless_arguments())]),
        );

        // The error result travels back paired with its original call id, so the model can continue.
        let (request, body) = ask_receive(&server, protocol);
        assert!(
            !ask_schema_offered(&body),
            "follow-up must not offer ask either: {body}"
        );
        assert_eq!(
            ask_tool_outputs(&body, protocol),
            vec![(
                "headless-ask".to_owned(),
                json!({"ok":false,"error":"ask requires interactive TUI mode"})
            )],
            "protocol {protocol} mode {mode}"
        );
        ask_final(request, protocol, "headless ask answered");

        if mode == "prompt" {
            let output = cli.finish();
            assert!(output.status.success(), "{output:?}");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains("headless ask answered"), "{stdout}");
            assert!(
                !stdout.contains("ask requires"),
                "stdout must not contain tool errors: {stdout}"
            );
        } else {
            // The first task wraps up normally and the buffered next line immediately becomes its own task.
            cli.wait_for(Pipe::Stderr, "Session:");
            let (request, body) = ask_receive(&server, protocol);
            assert_eq!(
                ask_user_texts(&body, protocol),
                ["headless ask task one", "headless ask task two"],
                "ask error must not consume the next stdin task"
            );
            assert!(!ask_schema_offered(&body), "{body}");
            assert!(
                ask_tool_outputs(&body, protocol)
                    .iter()
                    .any(|(id, value)| id == "headless-ask"
                        && value["error"] == "ask requires interactive TUI mode")
            );
            ask_final(request, protocol, "headless ask second answered");
            let output = cli.finish();
            assert!(output.status.success(), "{output:?}");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                stdout.contains("headless ask answered")
                    && stdout.contains("headless ask second answered"),
                "{stdout}"
            );
        }
        no_more(&server);
    }
}
