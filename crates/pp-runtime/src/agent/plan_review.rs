//! Standalone plan review: uses the REVIEW Role's model and effort, falling back to the main
//! session when unconfigured.
//! The review runs in a separate temporary context and the report is saved as a workflow record
//! bound to the plan revision.
//!
//! The reviewer reuses the real SDK clients and the shared tool gate, but
//! never touches the main session's native history, context usage, request
//! stats or journal stream: the scratch client's only journal connection is
//! the read-only `stats_context` that records request costs under the
//! `plan_review` purpose. Every tool call runs under [`ToolPolicy::Review`],
//! so writes, shell, ask, plan authoring and unauthorized MCP calls are refused at the execution
//! layer regardless of what the reviewer requests.

use super::turn::{append_user_to, response_input};
use super::{Agent, Session, input::InputQueue, persistence::Metadata};
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
use pp_core::cancellation;
use pp_core::cancellation::CancellationToken;
use pp_core::context::estimate_tokens;
use pp_core::events::AgentEvent;
use pp_core::plan::PLAN_MAX_BYTES;
use pp_core::plan::PlanDocument;
use pp_core::plan::PlanReviewReport;
use pp_core::plan::ReviewToken;
use pp_llm::CompletionTurn;
use pp_llm::OpenAiClient;
use pp_llm::ResponseTurn;
use pp_tools::ToolPolicy;

const REVIEW_REFUSED: &str = "Plan review was refused by the model.";
const REVIEW_CONTEXT: &str = "Plan review context limit reached.";

const REVIEW_INSTRUCTIONS: &str = include_str!("../prompts/plan-review.md");

struct ReviewModels {
    reference: String,
    window: Option<u64>,
    fallback: Option<pp_config::config::RoleModel>,
}

impl Agent {
    /// Review a saved plan independently with the REVIEW Role and write the report into
    /// `workflow.review`.
    ///
    /// The token is validated exactly like an approval (mode, Ready status,
    /// session, revision, queue, journal health and exact on-disk bytes); a
    /// changed file fails with the shared changed message so the UI refreshes
    /// via `read_plan` and re-issues the review. On any failure or
    /// cancellation the previous report, plan document and mode stay
    /// untouched and the scratch client's pending I/O is retired.
    pub fn review_plan(
        &mut self,
        token: ReviewToken,
        input: &InputQueue,
        cancellation: &CancellationToken,
        emit: &mut dyn FnMut(AgentEvent),
    ) -> Result<PlanReviewReport> {
        cancellation::check(cancellation)?;
        self.validate_plan_token(&token, input)?;
        self.connect_pending_plan_mcp(cancellation, pp_mcp::Access::ReadOnly)?;
        let document = self
            .workflow
            .plan
            .as_ref()
            .expect("validated token has a Ready plan")
            .clone();
        let selection = self.catalog.role_selection(
            pp_config::config::ModelRole::Review,
            &self.current_model.reference,
            self.think_level,
        )?;
        let config = self.catalog.resolve(&selection.model)?;
        self.continue_task_status();
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
        client.set_task_accounting(self.task_status.accounting.clone());
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
                            if pp_llm::retry::model_request_failed(&error)
                                && models.fallback.is_some() =>
                        {
                            cancellation::check(cancellation)?;
                            let target = models.fallback.take().expect("fallback model confirmed");
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
                                source: pp_core::model::ModelSource::Fallback,
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
                            if pp_llm::retry::model_request_failed(&error)
                                && models.fallback.is_some() =>
                        {
                            cancellation::check(cancellation)?;
                            let target = models.fallback.take().expect("fallback model confirmed");
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
                                source: pp_core::model::ModelSource::Fallback,
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
            let execution = self.tools.execute_with_web(
                &call.function.name,
                &call.function.arguments,
                &call.id,
                ToolPolicy::Review,
                None,
                cancellation,
                emit,
                &mut |action, token| super::tool_access::execute_web_action(client, action, token),
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
            let execution = self.tools.execute_with_web(
                &call.name,
                &call.arguments,
                &call.call_id,
                ToolPolicy::Review,
                None,
                cancellation,
                emit,
                &mut |action, token| super::tool_access::execute_web_action(client, action, token),
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
    // The reviewer only reviews saved (revision>=1) plans; the path is always resolvable.
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
mod tests;
