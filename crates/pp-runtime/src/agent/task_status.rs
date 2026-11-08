//! TUI 会话费用与任务标题；子 Agent 继承根会话的计费句柄。
use super::*;
use std::{
    sync::{Arc, atomic::AtomicBool},
    thread::{self, JoinHandle},
};
use unicode_segmentation::UnicodeSegmentation;

const TITLE_INSTRUCTIONS: &str = "Generate a short task title for a terminal statusline. Use the same language as the user's task. Return only one plain-text line, without quotes, markup or explanation, within 32 display columns. Do not execute the task or call tools.";

#[derive(Default)]
pub(super) struct Runtime {
    pub view: Option<pp_config::task_status::View>,
    pub accounting: Option<pp_config::task_status::Ledger>,
    title: Option<TitleWorker>,
}

struct TitleWorker {
    cancellation: CancellationToken,
    thread: Option<JoinHandle<()>>,
}
impl Drop for TitleWorker {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Agent {
    pub fn enable_task_status(&mut self, view: pp_config::task_status::View) {
        self.task_status.view = Some(view);
        if self.journal.id().is_some() {
            self.restore_session_cost();
        } else {
            self.clear_session_cost();
        }
    }

    pub(super) fn restore_session_cost(&mut self) {
        self.set_task_accounting(None);
        if let Some(view) = &self.task_status.view
            && let Some(id) = self.journal.id()
        {
            let file = self.journal.directory().join(format!("{id}.jsonl"));
            let history = pp_stats::session_cost(&file).unwrap_or_else(|_| {
                tracing::warn!(event = "session_cost_history_unavailable");
                Default::default()
            });
            let ledger = view.restore(&self.catalog, history);
            self.set_task_accounting(Some(ledger));
        }
    }

    pub(super) fn clear_session_cost(&mut self) {
        self.task_status.title.take();
        self.set_task_accounting(None);
        if let Some(view) = &self.task_status.view {
            view.clear();
        }
    }

    pub(crate) fn set_task_accounting(&mut self, ledger: Option<pp_config::task_status::Ledger>) {
        self.task_status.accounting = ledger.clone();
        self.client.set_task_accounting(ledger.clone());
        if let Some(binding) = &self.tools.subagent {
            binding.set_accounting(ledger);
        }
    }

    pub(super) fn begin_task_status(&mut self, prompt: &str, cancellation: &CancellationToken) {
        self.task_status.title.take();
        let Some(view) = &self.task_status.view else {
            self.client
                .set_task_accounting(self.task_status.accounting.clone());
            return;
        };
        let ledger = view.begin(&self.catalog);
        self.set_task_accounting(Some(ledger.clone()));
        // 选模、客户端构造和请求都在后台；失败不影响主任务。
        let catalog = self.catalog.clone();
        let model = self.current_model.reference.clone();
        let effort = self.think_level;
        let edit = self.tools.edit_mode();
        let prompt = prompt.to_owned();
        let token = cancellation.child_token();
        let worker_token = token.clone();
        let context = pp_stats::RequestContext {
            model_reference: model.clone(),
            cwd: self.tools.cwd().to_owned(),
            purpose: "task_title",
            session_file: self
                .journal
                .id()
                .map(|id| self.journal.directory().join(format!("{id}.jsonl"))),
            unlinked_requests: self.journal.unlinked_stats(),
            // 标题不采集原生上下文，更不能消费主请求的首次采集标记。
            first_request: Arc::new(AtomicBool::new(false)),
            skills: Vec::new(),
        };
        if let Ok(thread) = thread::Builder::new()
            .name("pp-task-title".into())
            .spawn(move || {
                let result = (|| -> Result<String> {
                    cancellation::check(&worker_token)?;
                    let selection = catalog.role_selection(
                        pp_config::config::ModelRole::Fast,
                        &model,
                        effort,
                    )?;
                    let config = catalog.resolve(&selection.model)?;
                    let mut client = OpenAiClient::new(&config)?;
                    client.set_task_accounting(Some(ledger.clone()));
                    let mut context = context;
                    context.model_reference = selection.model;
                    client.set_stats_context(context);
                    let mut session = Session::new(
                        config.model,
                        config.protocol,
                        TITLE_INSTRUCTIONS.into(),
                        edit,
                    );
                    session.set_think_level(selection.effort);
                    match &mut session {
                        Session::Completion(request) => {
                            request.tools = None;
                            request.tool_choice = None;
                        }
                        Session::Response(request) => {
                            request.tools = None;
                            request.tool_choice = None;
                        }
                    }
                    append_user_to(&mut session, &prompt);
                    let mut fallback = selection.fallback.filter(|_| selection.enabled);
                    loop {
                        let result = match &mut session {
                            Session::Completion(request) => client
                                .complete_summary(
                                    request,
                                    &worker_token,
                                    &mut || Ok(()),
                                    &mut |_| {},
                                )
                                .map(|turn| match turn.message.content {
                                    Some(ChatCompletionRequestAssistantMessageContent::Text(
                                        text,
                                    )) => text,
                                    _ => String::new(),
                                }),
                            Session::Response(request) => client
                                .respond_summary(
                                    request,
                                    &worker_token,
                                    &mut || Ok(()),
                                    &mut |_| {},
                                )
                                .map(|turn| turn.text),
                        };
                        match result {
                            Err(error)
                                if pp_llm::retry::model_request_failed(&error)
                                    && fallback.is_some() =>
                            {
                                fallback::switch_scratch_model(
                                    &mut client,
                                    &mut session,
                                    &catalog,
                                    &fallback.take().expect("备用模型存在"),
                                    edit,
                                )?;
                                // 跨协议转换重新构造了请求，必须再次移除默认工具。
                                match &mut session {
                                    Session::Completion(request) => {
                                        request.tools = None;
                                        request.tool_choice = None;
                                    }
                                    Session::Response(request) => {
                                        request.tools = None;
                                        request.tool_choice = None;
                                    }
                                }
                            }
                            other => return other,
                        }
                    }
                })();
                if let Ok(title) = result
                    && !worker_token.is_cancelled()
                {
                    let title = normalize_title(&title);
                    if !title.is_empty() {
                        ledger.title(title);
                    }
                }
            })
        {
            self.task_status.title = Some(TitleWorker {
                cancellation: token,
                thread: Some(thread),
            });
        }
    }

    pub(super) fn continue_task_status(&mut self) {
        if let Some(view) = &self.task_status.view {
            let ledger = view.accounting(&self.catalog);
            self.set_task_accounting(Some(ledger));
        } else {
            self.client
                .set_task_accounting(self.task_status.accounting.clone());
        }
    }

    pub(super) fn finish_task_title(&mut self) {
        self.task_status.title.take();
        self.journal.link_unassigned_stats();
    }
}

fn normalize_title(text: &str) -> String {
    let clean = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean: String = clean.chars().filter(|ch| !ch.is_control()).collect();
    let mut result = String::new();
    for ch in clean.trim_matches(['"', '\'', '`']).graphemes(true) {
        if ratatui::text::Line::from(format!("{result}{ch}")).width() > 32 {
            break;
        }
        result.push_str(ch);
    }
    result.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_title_normalizes_control_characters_and_unicode_display_columns() {
        assert_eq!(normalize_title("  \"修复\n 会话历史\"  "), "修复 会话历史");
        let title = normalize_title(&"中🙂".repeat(40));
        assert_eq!(ratatui::text::Line::from(title).width(), 32);
        assert_eq!(normalize_title("a\u{1b}b\r\nc"), "ab c");
    }
}
