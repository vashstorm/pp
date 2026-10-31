//! 独立计划审阅：使用 REVIEW Role 的模型和 effort；未配置时沿用主会话。
//! 审阅运行在独立临时上下文中，报告作为绑定计划修订的工作流记录保存。
//!
//! The reviewer reuses the real SDK clients and the shared tool gate, but
//! never touches the main session's native history, context usage, request
//! stats or journal stream: the scratch client's only journal connection is
//! the read-only `stats_context` that records request costs under the
//! `plan_review` purpose. Every tool call runs under [`ToolPolicy::Review`],
//! so writes, shell, ask, plan authoring and MCP are refused at the execution
//! layer regardless of what the reviewer requests.

use super::turn::{append_user_to, response_input};
use super::{Agent, Session, input::InputQueue, persistence::Metadata};
use crate::{
    cancellation::{self, CancellationToken},
    context::estimate_tokens,
    events::AgentEvent,
    llm::{CompletionTurn, OpenAiClient, ResponseTurn},
    plan::{PLAN_MAX_BYTES, PlanDocument, PlanReviewReport, ReviewToken},
    tools::ToolPolicy,
};
use anyhow::{Result, bail, ensure};
use async_openai::types::{
    chat::{
        ChatCompletionMessageToolCalls, ChatCompletionRequestAssistantMessageContent,
        ChatCompletionRequestMessage, ChatCompletionRequestToolMessage,
        CreateChatCompletionRequest,
    },
    responses::{
        CreateResponse, FunctionCallOutputItemParam, InputItem, Item, OutputItem,
        OutputMessageContent,
    },
};

const REVIEW_REFUSED: &str = "Plan review was refused by the model.";
const REVIEW_CONTEXT: &str = "Plan review context limit reached.";

const REVIEW_INSTRUCTIONS: &str = include_str!("../prompts/plan-review.md");

struct ReviewModels {
    reference: String,
    window: Option<u64>,
    fallback: Option<crate::config::RoleModel>,
}

impl Agent {
    /// 使用 REVIEW Role 独立审阅已保存计划，将报告写入 `workflow.review`。
    ///
    /// The token is validated exactly like an approval (mode, Ready status,
    /// session, revision, queue, journal health and exact on-disk bytes); a
    /// changed file fails with the shared changed message so the UI refreshes
    /// via `read_plan` and re-issues the review. On any failure or
    /// cancellation the previous report, plan document and mode stay
    /// untouched and the scratch client's pending I/O is retired.
    pub(crate) fn review_plan(
        &mut self,
        token: ReviewToken,
        input: &InputQueue,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<PlanReviewReport> {
        cancellation::check(cancellation)?;
        self.validate_plan_token(&token, input)?;
        let document = self
            .workflow
            .plan
            .as_ref()
            .expect("validated token has a Ready plan")
            .clone();
        let selection = self.catalog.role_selection(
            crate::config::ModelRole::Review,
            &self.current_model.reference,
            self.think_level,
        )?;
        let config = self.catalog.resolve(&selection.model)?;
        let metadata = Metadata {
            role_route: None,
            cwd: self.tools.cwd(),
            model_reference: &selection.model,
            think_level: selection.effort,
            edit_mode: self.tools.edit_mode(),
            tool_limits: self.tools.limits,
            base_system: &self.base_system,
            prompt_source: self.prompt_source.as_ref(),
        };
        let mut client = OpenAiClient::new(&config)?;
        // Costs are recorded for the source session under the review purpose;
        // they never become main native exchanges or context usage.
        client.set_stats_context(self.journal.stats_context(
            &metadata,
            "plan_review",
            &self.request_skills,
        ));
        let mut session = Session::new(
            config.model.clone(),
            config.protocol,
            self.review_system_for(&config.model)?,
            self.tools.edit_mode(),
        );
        session.set_tools(self.tools.review_definitions());
        session.set_think_level(selection.effort);
        append_user_to(&mut session, &review_prompt(&document));
        let mut models = ReviewModels {
            reference: selection.model.clone(),
            window: config.context_window,
            fallback: selection.fallback.clone().filter(|_| selection.enabled),
        };
        let outcome = self.review_loop(&mut client, &mut session, &mut models, cancellation, emit);
        if outcome.is_err() {
            client.cancel_pending_io();
        }
        drop(client);
        let content = outcome?;
        let report = PlanReviewReport {
            plan_id: document.id.clone(),
            revision: document.revision,
            model_reference: models.reference,
            content,
        };
        let previous = self.workflow.clone();
        self.workflow.review = Some(report.clone());
        self.commit_workflow(previous)?;
        Ok(report)
    }

    fn review_loop(
        &self,
        client: &mut OpenAiClient,
        session: &mut Session,
        models: &mut ReviewModels,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<String> {
        loop {
            cancellation::check(cancellation)?;
            // Refresh project instructions and the skill summary every round
            // so the reviewer always audits against current rules.
            let model = match session {
                Session::Completion(request) => request.model.as_str(),
                Session::Response(request) => request.model.as_deref().unwrap_or_default(),
            };
            let system = self.review_system_for(model)?;
            set_review_system(session, &system);
            if let Some(window) = models.window {
                let estimated = estimate_tokens(session)?;
                ensure!(
                    u128::from(estimated) < u128::from(window),
                    "{REVIEW_CONTEXT}"
                );
            }
            emit(AgentEvent::AssistantStarted);
            let final_text = match session {
                Session::Completion(request) => {
                    let mut sent = || -> Result<()> { Ok(()) };
                    let outcome = client.complete(request, cancellation, &mut sent, emit);
                    let turn: CompletionTurn = match outcome {
                        Ok(turn) => turn,
                        Err(error)
                            if crate::llm::model_request_failed(&error)
                                && models.fallback.is_some() =>
                        {
                            cancellation::check(cancellation)?;
                            let target = models.fallback.take().expect("已确认备用模型");
                            models.window = super::fallback::switch_scratch_model(
                                client,
                                session,
                                &self.catalog,
                                &target,
                                self.tools.edit_mode(),
                            )?;
                            session.set_tools(self.tools.review_definitions());
                            models.reference = target.model.clone();
                            emit(AgentEvent::ModelRouteChanged {
                                model: target.model,
                                effort: target.effort,
                                fallback: true,
                                temporary: true,
                            });
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    cancellation::check(cancellation)?;
                    self.completion_review_round(client, request, turn, cancellation, emit)?
                }
                Session::Response(request) => {
                    let mut sent = || -> Result<()> { Ok(()) };
                    let outcome = client.respond(request, cancellation, &mut sent, emit);
                    let turn: ResponseTurn = match outcome {
                        Ok(turn) => turn,
                        Err(error)
                            if crate::llm::model_request_failed(&error)
                                && models.fallback.is_some() =>
                        {
                            cancellation::check(cancellation)?;
                            let target = models.fallback.take().expect("已确认备用模型");
                            models.window = super::fallback::switch_scratch_model(
                                client,
                                session,
                                &self.catalog,
                                &target,
                                self.tools.edit_mode(),
                            )?;
                            session.set_tools(self.tools.review_definitions());
                            models.reference = target.model.clone();
                            emit(AgentEvent::ModelRouteChanged {
                                model: target.model,
                                effort: target.effort,
                                fallback: true,
                                temporary: true,
                            });
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    cancellation::check(cancellation)?;
                    self.response_review_round(client, request, turn, cancellation, emit)?
                }
            };
            if let Some(text) = final_text {
                return Ok(text);
            }
        }
    }

    /// One Chat Completions round: append the assistant turn, run every tool
    /// call under the Review policy with complete call-ID pairing, and return
    /// the final report text only when the model produced no tool calls.
    fn completion_review_round(
        &self,
        client: &mut OpenAiClient,
        request: &mut CreateChatCompletionRequest,
        turn: CompletionTurn,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<String>> {
        let text = match &turn.message.content {
            Some(ChatCompletionRequestAssistantMessageContent::Text(text)) => text.clone(),
            _ => String::new(),
        };
        emit(AgentEvent::AssistantFinished { text: text.clone() });
        let count = turn.message.tool_calls.as_deref().unwrap_or_default().len();
        let refusal = turn.refusal;
        request
            .messages
            .push(ChatCompletionRequestMessage::Assistant(turn.message));
        if count == 0 {
            ensure!(!refusal, "{REVIEW_REFUSED}");
            return Ok(Some(final_report(&text)?));
        }
        let assistant_index = request.messages.len() - 1;
        let mut stop_error = None;
        let mut attachments = Vec::new();
        for index in 0..count {
            let ChatCompletionRequestMessage::Assistant(message) =
                &request.messages[assistant_index]
            else {
                bail!("plan review invariant: missing assistant message");
            };
            let Some(ChatCompletionMessageToolCalls::Function(call)) = message
                .tool_calls
                .as_ref()
                .and_then(|calls| calls.get(index))
            else {
                bail!("plan review invariant: missing function call");
            };
            let execution = self.tools.execute_with_fetch(
                &call.function.name,
                &call.function.arguments,
                &call.id,
                ToolPolicy::Review,
                cancellation,
                emit,
                &mut |url, token| client.fetch_read_url(url, token),
            );
            if stop_error.is_none() {
                stop_error = execution.stop_error;
            }
            attachments.push((call.id.clone(), execution.images));
            request.messages.push(ChatCompletionRequestMessage::Tool(
                ChatCompletionRequestToolMessage {
                    tool_call_id: call.id.clone(),
                    content: execution.output.into(),
                },
            ));
        }
        super::tool_images::append_chat(&mut request.messages, attachments);
        if let Some(error) = stop_error {
            return Err(error);
        }
        Ok(None)
    }

    /// One Responses round: append the native output items, dispatch every
    /// function call under the Review policy with complete call-ID pairing,
    /// and return the final report text only when there were no calls.
    fn response_review_round(
        &self,
        client: &mut OpenAiClient,
        request: &mut CreateResponse,
        turn: ResponseTurn,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<Option<String>> {
        emit(AgentEvent::AssistantFinished {
            text: turn.text.clone(),
        });
        let refusal_part = turn.output.iter().any(|item| match item {
            OutputItem::Message(message) => message
                .content
                .iter()
                .any(|part| matches!(part, OutputMessageContent::Refusal(_))),
            _ => false,
        });
        let refusal = turn.refusal;
        let input = response_input(request);
        let start = input.len();
        input.extend(turn.output.into_iter().map(InputItem::from));
        if !turn.has_calls {
            ensure!(!refusal && !refusal_part, "{REVIEW_REFUSED}");
            return Ok(Some(final_report(&turn.text)?));
        }
        let end = input.len();
        let mut stop_error = None;
        for index in start..end {
            let InputItem::Item(Item::FunctionCall(call)) = &input[index] else {
                continue;
            };
            let execution = self.tools.execute_with_fetch(
                &call.name,
                &call.arguments,
                &call.call_id,
                ToolPolicy::Review,
                cancellation,
                emit,
                &mut |url, token| client.fetch_read_url(url, token),
            );
            if stop_error.is_none() {
                stop_error = execution.stop_error;
            }
            let call_id = Some(call.call_id.clone());
            input.push(InputItem::Item(Item::FunctionCallOutput(
                FunctionCallOutputItemParam {
                    call_id,
                    output: super::tool_images::response_output(execution.output, execution.images),
                    id: None,
                    status: None,
                    name: None,
                    namespace: None,
                    caller: None,
                },
            )));
        }
        if let Some(error) = stop_error {
            return Err(error);
        }
        Ok(None)
    }

    /// Reviewer system prompt: current project instructions and skill summary
    /// plus the fixed review-only rules. Plan-mode drafting rules are never
    /// mixed in; the reviewer cannot write or authorize anything.
    fn review_system_for(&self, model: &str) -> Result<String> {
        self.compose_prompt(
            REVIEW_INSTRUCTIONS,
            "review",
            model,
            self.tools
                .review_definitions()
                .into_iter()
                .map(|tool| tool.name)
                .collect(),
        )
    }
}

/// Only a non-empty, NUL-free final answer within the plan size bound is a
/// complete report; streamed fragments and tool status are never one.
fn final_report(text: &str) -> Result<String> {
    ensure!(!text.trim().is_empty(), "Plan review produced no report.");
    ensure!(!text.contains('\0'), "Plan review report contains NUL.");
    ensure!(
        text.len() <= PLAN_MAX_BYTES,
        "Plan review report is too large."
    );
    Ok(text.to_owned())
}

/// The reviewer's only user message: the original user task recorded for this
/// plan plus the exact reviewed revision. No planning chat, native reasoning
/// or prior tool results are attached.
fn review_prompt(document: &PlanDocument) -> String {
    use std::fmt::Write;
    let mut prompt = String::new();
    // reviewer 只审阅已保存（revision>=1）的计划；路径必然可解析。
    let path = document
        .path()
        .expect("reviewed plan has a saved lifecycle path");
    write!(
        prompt,
        "Review the saved implementation plan below against the current code. Report findings as specified in the review instructions; do not modify anything and do not implement the plan.\n\nOriginal user task recorded for this plan:\n{}\n\nPlan ID: {}\nPath: {}\nRevision: {}\n\n[Plan Markdown snapshot]\n{}\n[/Plan Markdown snapshot]",
        document.origin_prompt,
        document.id,
        path,
        document.revision,
        document.content
    )
    .expect("writing to String");
    prompt
}

fn set_review_system(session: &mut Session, system: &str) {
    match session {
        Session::Completion(request) => {
            let Some(ChatCompletionRequestMessage::System(message)) = request.messages.first_mut()
            else {
                unreachable!("review session has system instructions");
            };
            message.content = system.to_owned().into();
        }
        Session::Response(request) => request.instructions = Some(system.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::{
        EXPLORATION, ORIGIN, PLAN_CONTENT, completion_stream, err_message, final_for,
        journal_workflow, plan_workflow, response_stream, runtime_input_agent, seed_session,
        write_plan_file,
    };
    use crate::config::ThinkLevel;
    use crate::plan::{PlanStatus, WorkMode};
    use serde_json::{Value, json};

    const REPORT: &str = "## Findings\n\n- Minor: verification step is vague.\n";

    #[test]
    fn model_roles_review_uses_its_own_model_protocol_and_effort_without_changing_main() {
        for protocol in ["completion", "response"] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let address = server.server_addr().to_string();
            let dir = tempfile::tempdir().unwrap();
            let (mut agent, token) = ready_reviewer(
                dir.path(),
                if protocol == "completion" {
                    "response"
                } else {
                    "completion"
                },
                &address,
            );
            let path = dir.path().join("models.yml");
            let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            models["providers"]["audit"] = json!({"api_key":"fixture-key", "protocol":protocol,
                "base_url":format!("http://{address}/v1"), "models":{"review":{"model":"audit-model"}}});
            std::fs::write(path, models.to_string()).unwrap();
            agent.reload_models().unwrap();
            agent
                .save_model_role(
                    crate::config::ModelRole::Review,
                    Some(crate::config::RoleAssignment {
                        fallback: None,
                        enabled: true,
                        model: "audit/review".into(),
                        effort: crate::config::ThinkLevel::High,
                    }),
                )
                .unwrap();
            let main = serde_json::to_value(&agent.session).unwrap();
            let handle = std::thread::spawn(move || {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap()
                    .unwrap();
                let mut body = String::new();
                request.as_reader().read_to_string(&mut body).unwrap();
                let body: Value = serde_json::from_str(&body).unwrap();
                assert_eq!(body["model"], "audit-model");
                let effort = if protocol == "completion" {
                    &body["reasoning_effort"]
                } else {
                    &body["reasoning"]["effort"]
                };
                assert_eq!(effort, "high");
                final_for(protocol, request, REPORT, false);
            });
            let report = agent
                .review_plan(
                    token,
                    &InputQueue::default(),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .unwrap();
            handle.join().unwrap();
            assert_eq!(report.model_reference, "audit/review");
            assert_eq!(agent.current_model.reference, "fixture/one");
            assert_eq!(serde_json::to_value(&agent.session).unwrap(), main);
        }
    }

    #[test]
    fn fallback_plan_review_switches_scratch_protocol_without_changing_main() {
        for protocol in ["completion", "response"] {
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let address = server.server_addr().to_string();
            let dir = tempfile::tempdir().unwrap();
            let (mut agent, token) = ready_reviewer(dir.path(), protocol, &address);
            let backup_protocol = if protocol == "completion" {
                "response"
            } else {
                "completion"
            };
            let path = dir.path().join("models.yml");
            let mut models: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            models["providers"]["audit"] = json!({"api_key":"fixture-key","protocol":backup_protocol,
                "base_url":format!("http://{address}/v1"),"models":{"backup":{"model":"review-backup"}}});
            std::fs::write(path, models.to_string()).unwrap();
            agent.reload_models().unwrap();
            agent
                .save_model_role(
                    crate::config::ModelRole::Review,
                    Some(crate::config::RoleAssignment {
                        enabled: true,
                        model: "fixture/one".into(),
                        effort: ThinkLevel::None,
                        fallback: Some(crate::config::RoleModel {
                            model: "audit/backup".into(),
                            effort: ThinkLevel::High,
                        }),
                    }),
                )
                .unwrap();
            let before = serde_json::to_value(&agent.session).unwrap();
            let handle = std::thread::spawn(move || {
                let request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                request.respond(tiny_http::Response::empty(503)).unwrap();
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    request.url(),
                    if backup_protocol == "completion" {
                        "/v1/chat/completions"
                    } else {
                        "/v1/responses"
                    }
                );
                let body: Value = serde_json::from_reader(request.as_reader()).unwrap();
                assert_eq!(body["model"], "review-backup");
                final_for(backup_protocol, request, REPORT, false);
            });
            let report = agent
                .review_plan(
                    token,
                    &InputQueue::default(),
                    &CancellationToken::new(),
                    &mut |_| {},
                )
                .unwrap();
            handle.join().unwrap();
            assert_eq!(report.model_reference, "audit/backup");
            assert_eq!(agent.current_model.reference, "fixture/one");
            assert_eq!(serde_json::to_value(&agent.session).unwrap(), before);
        }
    }

    // --- fixtures ---------------------------------------------------------

    /// A Ready revision-1 plan with its file and a seeded session, plus the
    /// matching review token.
    fn ready_reviewer(
        dir: &std::path::Path,
        protocol: &str,
        server_addr: &str,
    ) -> (Agent, ReviewToken) {
        let mut agent = runtime_input_agent(dir, protocol, server_addr, "");
        agent.workflow = plan_workflow(PLAN_CONTENT, ORIGIN, PlanStatus::Ready);
        write_plan_file(&agent, PLAN_CONTENT);
        seed_session(&mut agent, EXPLORATION);
        let review = agent.read_plan().unwrap();
        (agent, review.token)
    }

    fn previous_report(agent: &Agent) -> PlanReviewReport {
        PlanReviewReport {
            plan_id: agent.workflow.plan.as_ref().unwrap().id.clone(),
            revision: agent.workflow.plan.as_ref().unwrap().revision,
            model_reference: "fixture/one".into(),
            content: "# Previous report\n\nOlder findings.\n".into(),
        }
    }

    // --- protocol stream fixtures ----------------------------------------
    // The SSE/stream builders are shared with `crate::agent::tests`. The
    // reviewer fixture intentionally omits `response.content_part.done`.

    fn refusal_for(protocol: &str, request: tiny_http::Request) {
        match protocol {
            "completion" => completion_stream(
                request,
                json!({"role":"assistant","content":null,"refusal":"Cannot help"}),
                "stop",
            ),
            _ => response_stream(
                request,
                std::slice::from_ref(&json!({
                    "type":"message", "id":"msg-refusal", "role":"assistant",
                    "status":"completed",
                    "content":[{"type":"refusal","refusal":"Cannot help",
                                "annotations":[],"logprobs":null}]
                })),
                false,
            ),
        }
    }

    fn calls_for(protocol: &str, request: tiny_http::Request, calls: &[(&str, &str, Value)]) {
        if protocol == "completion" {
            let calls: Vec<_> = calls
                .iter()
                .enumerate()
                .map(|(index, (id, name, args))| {
                    json!({"id":id, "type":"function", "index":index,
                           "function":{"name":name,"arguments":args.to_string()}})
                })
                .collect();
            completion_stream(
                request,
                json!({"role":"assistant","content":null,"tool_calls":calls}),
                "tool_calls",
            );
        } else {
            let calls: Vec<_> = calls
                .iter()
                .map(|(id, name, args)| {
                    json!({"type":"function_call", "id":format!("fc-{id}"),
                           "call_id":id, "name":name,
                           "arguments":args.to_string(), "status":"completed"})
                })
                .collect();
            response_stream(request, &calls, false);
        }
    }

    fn request_body(request: &mut tiny_http::Request) -> Value {
        let mut body = String::new();
        std::io::Read::read_to_string(request.as_reader(), &mut body).unwrap();
        serde_json::from_str(&body).unwrap()
    }

    fn history_entries(body: &Value, protocol: &str) -> Vec<Value> {
        match protocol {
            "completion" => body["messages"].as_array().unwrap().clone(),
            _ => body["input"].as_array().unwrap().clone(),
        }
    }

    // --- unit behavior ----------------------------------------------------

    #[test]
    fn final_report_rejects_blank_nul_and_oversize() {
        assert_eq!(final_report("  report  ").unwrap(), "  report  ");
        assert_eq!(
            err_message(final_report("   ")),
            "Plan review produced no report."
        );
        assert_eq!(
            err_message(final_report("bad\0report")),
            "Plan review report contains NUL."
        );
        let oversize = "x".repeat(PLAN_MAX_BYTES + 1);
        assert_eq!(
            err_message(final_report(&oversize)),
            "Plan review report is too large."
        );
        assert_eq!(PLAN_MAX_BYTES, 262_144);
    }

    // --- preconditions ----------------------------------------------------

    #[test]
    fn plan_review_rejects_invalid_states_without_requests() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let (mut agent, token) = ready_reviewer(dir.path(), protocol, "127.0.0.1:1");
            let queue = InputQueue::default();
            let session_before = serde_json::to_value(&agent.session).unwrap();

            // Execute mode never reviews.
            agent.workflow.mode = WorkMode::Execute;
            let error = err_message(agent.review_plan(
                token.clone(),
                &queue,
                &CancellationToken::new(),
                &mut |_| {},
            ));
            assert_eq!(error, "Plan review is no longer current.");
            agent.workflow.mode = WorkMode::Plan;

            // Draft plans cannot be reviewed.
            agent.workflow.plan.as_mut().unwrap().status = PlanStatus::Draft;
            let error = err_message(agent.review_plan(
                token.clone(),
                &queue,
                &CancellationToken::new(),
                &mut |_| {},
            ));
            assert_eq!(error, "Plan review is no longer current.");
            agent.workflow.plan.as_mut().unwrap().status = PlanStatus::Ready;

            // A token bound to other bytes is stale.
            let mut stale = token.clone();
            stale.content = format!("{PLAN_CONTENT}\ndrift\n");
            let error = err_message(agent.review_plan(
                stale,
                &queue,
                &CancellationToken::new(),
                &mut |_| {},
            ));
            assert_eq!(error, "Plan review is no longer current.");

            // Pending steering input blocks the review.
            queue.begin_run().unwrap();
            queue
                .enqueue(
                    crate::agent::input::PendingKind::Steering,
                    "interrupt".into(),
                    None,
                    false,
                )
                .unwrap();
            let error = err_message(agent.review_plan(
                token.clone(),
                &queue,
                &CancellationToken::new(),
                &mut |_| {},
            ));
            assert_eq!(error, "Apply pending input before approving the plan.");
            queue.close().unwrap();

            // An externally changed file is not silently reviewed; the
            // pending queue above must not mask the file check.
            write_plan_file(&agent, &format!("{PLAN_CONTENT}\nexternal edit\n"));
            let error = err_message(agent.review_plan(
                token.clone(),
                &InputQueue::default(),
                &CancellationToken::new(),
                &mut |_| {},
            ));
            assert_eq!(
                error,
                "Plan changed; review the new revision before executing."
            );

            // None of the failures changed the session or granted a report.
            assert_eq!(
                serde_json::to_value(&agent.session).unwrap(),
                session_before
            );
            assert!(agent.workflow.review.is_none());
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Ready
            );
        }
    }

    #[test]
    fn plan_review_cancelled_before_request_retires_scratch_io() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            let (mut agent, token) = ready_reviewer(dir.path(), protocol, "127.0.0.1:1");
            let previous = previous_report(&agent);
            agent.workflow.review = Some(previous.clone());
            let session_before = serde_json::to_value(&agent.session).unwrap();
            let cancellation = CancellationToken::new();
            cancellation.cancel();
            let error = err_message(agent.review_plan(
                token,
                &InputQueue::default(),
                &cancellation,
                &mut |_| {},
            ));
            assert!(error.contains("cancel"), "unexpected error: {error}");
            assert_eq!(agent.workflow.review, Some(previous));
            assert_eq!(
                serde_json::to_value(&agent.session).unwrap(),
                session_before
            );
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Ready
            );
        }
    }

    // --- full review rounds ------------------------------------------------

    #[test]
    fn plan_review_rounds_gate_tools_and_persist_report() {
        for protocol in ["completion", "response"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("canary"), "original\n").unwrap();
            std::fs::write(dir.path().join("source.txt"), "needle here\n").unwrap();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let (mut agent, token) =
                ready_reviewer(dir.path(), protocol, &server.server_addr().to_string());
            let plan_id = agent.workflow.plan.as_ref().unwrap().id.clone();
            let session_before = serde_json::to_value(&agent.session).unwrap();
            let journal = dir
                .path()
                .join("sessions")
                .join(format!("{}.jsonl", agent.session_id().unwrap()));
            let lines_before = std::fs::read_to_string(&journal).unwrap().lines().count();

            let serving = protocol.to_owned();
            let worker = std::thread::spawn(move || {
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let body = request_body(&mut request);
                // The only user message carries the origin prompt and the
                // exact plan revision, never the planning chat.
                let entries = history_entries(&body, &serving);
                let user_text = entries
                    .iter()
                    .filter(|entry| entry["role"] == "user")
                    .map(|entry| match &entry["content"] {
                        Value::String(text) => text.clone(),
                        parts => parts
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<String>(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(user_text.contains(ORIGIN));
                assert!(user_text.contains(&plan_id));
                assert!(user_text.contains("Revision: 1"));
                assert!(user_text.contains(PLAN_CONTENT));
                assert!(!user_text.contains(EXPLORATION));
                assert_eq!(entries.iter().filter(|e| e["role"] == "user").count(), 1);
                // Review-only system instructions and read-only toolset.
                let instructions = if serving == "completion" {
                    body["messages"][0]["content"].as_str().unwrap()
                } else {
                    body["instructions"].as_str().unwrap()
                };
                assert!(instructions.contains("PLAN REVIEW MODE"));
                let tools: Vec<&str> = body["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|tool| {
                        tool["function"]["name"]
                            .as_str()
                            .or(tool["name"].as_str())
                            .unwrap()
                    })
                    .collect();
                assert!(
                    tools
                        .iter()
                        .all(|name| matches!(*name, "read" | "grep" | "glob"))
                );
                calls_for(
                    &serving,
                    request,
                    &[
                        ("read-canary", "read", json!({"path":"canary"})),
                        ("search", "grep", json!({"pattern":"needle"})),
                        ("paths", "glob", json!({"path":"canary"})),
                        ("shell", "bash", json!({"command":"touch pwned"})),
                        ("native", "find", json!({"args":[]})),
                        ("ask", "ask", json!({"questions":[]})),
                        (
                            "plan-write",
                            "plan_write",
                            json!({"title":"X","content":"- [ ] x","expected_revision":1}),
                        ),
                        ("mystery", "mcp_unknown", json!({})),
                    ],
                );
                let mut request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                let body = request_body(&mut request);
                // The tool batch was appended natively with paired results.
                let entries = history_entries(&body, &serving);
                let outputs = entries
                    .iter()
                    .filter(|item| item["role"] == "tool" || item["type"] == "function_call_output")
                    .count();
                assert_eq!(outputs, 8);
                final_for(&serving, request, REPORT, false);
                (server, plan_id)
            });

            let mut results = Vec::new();
            let report = agent
                .review_plan(
                    token,
                    &InputQueue::default(),
                    &CancellationToken::new(),
                    &mut |event| {
                        if let AgentEvent::ToolFinished { id, result } = event {
                            results.push((id, result));
                        }
                    },
                )
                .unwrap();
            let (server, plan_id) = worker.join().unwrap();

            assert!(server.try_recv().unwrap().is_none());
            assert_eq!(results.len(), 8);
            assert_eq!(results[0].1["ok"], true, "read result: {:?}", results[0]);
            assert_eq!(
                results[1].1["ok"], true,
                "plan_search result: {:?}",
                results[1]
            );
            assert_eq!(results[2].1["ok"], true, "glob result: {:?}", results[2]);
            assert_eq!(results[2].1["content"], "canary");
            for (id, result) in &results[3..] {
                assert_eq!(
                    result["error"], "Tool is not available during Plan review.",
                    "{id}: {result}"
                );
            }
            assert_eq!(
                std::fs::read_to_string(dir.path().join("canary")).unwrap(),
                "original\n"
            );
            assert!(!dir.path().join("pwned").exists());

            // The report is persisted as a revision-bound workflow record;
            // plan document, file, mode and session all stay untouched.
            assert_eq!(report.plan_id, plan_id);
            assert_eq!(report.revision, 1);
            assert_eq!(report.model_reference, "fixture/one");
            assert_eq!(report.content, REPORT);
            assert_eq!(agent.workflow.review.as_ref().unwrap().content, REPORT);
            assert_eq!(
                serde_json::to_value(&agent.session).unwrap(),
                session_before
            );
            let document = agent.workflow.plan.as_ref().unwrap();
            assert_eq!(document.revision, 1);
            assert_eq!(document.status, PlanStatus::Ready);
            assert_eq!(agent.workflow.mode, WorkMode::Plan);
            assert_eq!(
                std::fs::read_to_string(dir.path().join(document.path().expect("plan has a path")))
                    .unwrap(),
                PLAN_CONTENT
            );
            let id = agent.session_id().unwrap().to_owned();
            let expected = agent.workflow.clone();
            assert_eq!(journal_workflow(dir.path(), &id), expected);
            // Exactly one journal record was added: the workflow commit.
            let lines_after = std::fs::read_to_string(&journal).unwrap().lines().count();
            assert_eq!(lines_after, lines_before + 1);
        }
    }

    #[test]
    fn plan_review_failures_preserve_previous_report() {
        for protocol in ["completion", "response"] {
            // Provider refusal replaces nothing.
            let dir = tempfile::tempdir().unwrap();
            let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
            let (mut agent, token) =
                ready_reviewer(dir.path(), protocol, &server.server_addr().to_string());
            let previous = previous_report(&agent);
            agent.workflow.review = Some(previous.clone());
            let session_before = serde_json::to_value(&agent.session).unwrap();
            let worker = std::thread::spawn(move || {
                let request = server
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap()
                    .unwrap();
                refusal_for(protocol, request);
                server
            });
            let error = err_message(agent.review_plan(
                token,
                &InputQueue::default(),
                &CancellationToken::new(),
                &mut |_| {},
            ));
            worker.join().unwrap();
            assert_eq!(error, "Plan review was refused by the model.");
            assert_eq!(agent.workflow.review, Some(previous));
            assert_eq!(
                serde_json::to_value(&agent.session).unwrap(),
                session_before
            );
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Ready
            );

            // An unreachable provider fails the review without state changes.
            let dir = tempfile::tempdir().unwrap();
            let (mut agent, token) = ready_reviewer(dir.path(), protocol, "127.0.0.1:1");
            let previous = previous_report(&agent);
            agent.workflow.review = Some(previous.clone());
            let error = err_message(agent.review_plan(
                token,
                &InputQueue::default(),
                &CancellationToken::new(),
                &mut |_| {},
            ));
            assert!(!error.is_empty());
            assert_eq!(agent.workflow.review, Some(previous));
            assert_eq!(
                agent.workflow.plan.as_ref().unwrap().status,
                PlanStatus::Ready
            );
        }
    }
}
