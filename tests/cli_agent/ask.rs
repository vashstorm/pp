// Headless ask 契约（计划 Verification「headless 契约」行）：-p 与管道行模式、
// 两种协议的请求都不得携带 ask schema；fixture 仍返回 ask 调用时得到固定的
// `ask requires interactive TUI mode` 工具错误，结果按原 call id 成对进入历史，
// 不挂住、不把 stdin 下一行当答案消费。全部通过真实子进程、loopback HTTP 与
// HTTP 栅栏观察，不用 sleep 猜阶段。辅助项均以 `ask_` 前缀命名。

use super::streaming::{LiveCli, Pipe};
use super::*;

fn ask_receive(server: &Server, protocol: &str) -> (Request, Value) {
    receive_at(
        server,
        if protocol == "completion" {
            "/v1/chat/completions"
        } else {
            "/v1/responses"
        },
    )
}

// ask schema 是否出现在请求 tools 中（Chat 在 tools[].function，Responses 在 tools[]）。
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

// 按 history 顺序返回 (call_id, 结果 JSON)，覆盖两种协议的成对工具结果。
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

fn ask_calls_reply(request: Request, protocol: &str, calls: Value) {
    if protocol == "completion" {
        calls_reply(request, calls);
    } else {
        response_reply(request, calls);
    }
}

fn ask_final(request: Request, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

// 参数本身完全合法：错误只能来自缺少交互 handler，而不是参数解析。
fn ask_headless_arguments() -> Value {
    json!({"questions":[{"id":"storage","question":"Which database?","options":[{"label":"SQLite"},{"label":"Postgres"}],"recommended":0}]})
}

#[test]
fn ask_headless_omits_schema_and_answers_fixed_error_without_consuming_stdin() {
    for protocol in ["completion", "response"] {
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
                // 两行任务一次性写好：若 ask 处理消费了下一行，第二个任务就不会
                // 以独立的用户消息出现。
                cli.input("headless ask task one\nheadless ask task two\n");
            }

            // 第一个请求：工具列表存在但不含 ask。
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

            // 错误结果按原 call id 成对回传，模型得以继续。
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
                // 第一个任务正常收尾，缓冲的下一行立即成为独立任务。
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
}

// 防御性回归：行模式在 ask 错误后依然把下一行作为独立任务处理，绝不把
// stdin 当作答案通道（保持非 TTY stdin 恒为任务输入的既有契约）。
#[test]
fn ask_headless_line_mode_keeps_reading_tasks_after_unsupported_ask() {
    for protocol in ["completion", "response"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        let models = match protocol {
            "completion" => fixture_models(&server),
            _ => response_models(&server, Some("response")),
        };
        let mut cli = LiveCli::new(Cli::start_with(
            dir.path(),
            None,
            fixture_settings(),
            &models,
        ));
        cli.input("unsupported ask one\nplain tail task\n");
        let (request, _) = ask_receive(&server, protocol);
        ask_calls_reply(
            request,
            protocol,
            json!([ask_call(
                protocol,
                "unsupported-ask",
                ask_headless_arguments()
            )]),
        );
        let (request, body) = ask_receive(&server, protocol);
        assert_eq!(
            ask_user_texts(&body, protocol),
            ["unsupported ask one"],
            "second line must wait behind the running task"
        );
        ask_final(request, protocol, "first answered anyway");
        cli.wait_for(Pipe::Stderr, "Session:");
        let (request, body) = ask_receive(&server, protocol);
        assert_eq!(
            ask_user_texts(&body, protocol),
            ["unsupported ask one", "plain tail task"]
        );
        assert!(!ask_schema_offered(&body), "{body}");
        ask_final(request, protocol, "tail answered");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("tail answered"),
            "stdin tasks keep flowing after the fixed ask error"
        );
        no_more(&server);
    }
}
