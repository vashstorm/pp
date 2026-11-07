//! 行模式 CLI：stdin 逐行读取、事件 → stdout/stderr 输出、`/command` 分发与退出收尾。

use anyhow::{Context, Result};
use crossterm::style::Stylize;
use std::{
    io::{self, BufRead, IsTerminal, Write},
    sync::mpsc,
    thread,
    time::Duration,
};

use crate::commands;
use crate::tips;
use pp_config::skills;
use pp_core::cancellation;
use pp_core::context;
use pp_core::events;
use pp_runtime::agent;
#[cfg(debug_assertions)]
use pp_runtime::mock_ai;
use pp_runtime::tools;

/// 行模式 stdin 的逐行读取结果；专用线程只负责 I/O，命令仍在主线程分发。
pub(crate) enum CliInput {
    Line(String),
    Eof,
    Error(io::Error),
}

/// 空闲轮询周期：既驱动迟到 LSP 诊断展示，也让主线程保持对输入的响应。
const CLI_IDLE_TICK: Duration = Duration::from_millis(50);

pub(crate) fn cli_input_lines() -> Result<mpsc::Receiver<CliInput>> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("pp-stdin".into())
        .spawn(move || {
            let stdin = io::stdin();
            loop {
                let mut line = String::new();
                match stdin.lock().read_line(&mut line) {
                    Ok(0) => {
                        let _ = sender.send(CliInput::Eof);
                        break;
                    }
                    Ok(_) => {
                        if sender.send(CliInput::Line(line)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(CliInput::Error(error));
                        break;
                    }
                }
            }
        })
        .context("starting stdin reader")?;
    Ok(receiver)
}

/// CLI 各正常/错误退出边界显式回收 language server；`CleanupError` 保持类型并
/// 合并进返回错误，不能只依赖 Tools 的 Drop 兜底。
pub fn stop_cli_lsp(agent: Option<&agent::Agent>, result: Result<()>) -> Result<()> {
    let stop = match agent {
        Some(agent) => agent.stop_lsp(),
        None => Ok(()),
    };
    match (result, stop) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) | (Err(error), Ok(())) => Err(error),
        (Err(error), Err(stop)) => {
            if stop.is::<cancellation::CleanupError>() {
                Err(stop.context(format!("{error:#}")))
            } else {
                Err(error.context(format!("stopping language servers: {stop:#}")))
            }
        }
    }
}

/// stderr 上的 LSP 报告；正文与助手输出分开，控制字符先经显示过滤。
pub(crate) fn cli_lsp_report(
    report: &pp_core::lsp_report::DiagnosticReport,
    late: bool,
    stderr: &mut impl Write,
    broken: &mut bool,
    first_error: &mut Option<io::Error>,
) {
    let body = pp_runtime::lsp::diagnostics::render(
        report,
        Some(pp_runtime::lsp::diagnostics::AUTO_MESSAGE_LIMIT),
    );
    let label = if late {
        "LSP diagnostics (late)"
    } else {
        "LSP diagnostics"
    };
    cli_write(
        stderr,
        &format!("{label}\n{}\n", pp_core::text::display_text(&body)),
        broken,
        first_error,
    );
}

/// 非 TUI 启动横幅：使用提示、skills/command 诊断与恢复会话提示。
pub fn prelude(
    skills: &skills::SharedSkills,
    file_commands: &commands::FileCommands,
    resumed_id: Option<&str>,
    stderr: &mut impl Write,
) -> Result<()> {
    writeln!(
        stderr,
        "write/edit only modify files inside the startup directory; bash has no directory isolation, so run in a trusted or temporary directory."
    )?;
    writeln!(
        stderr,
        "bash uses an embedded shell; timeouts reclaim managed external processes and cooperatively stop the interpreter. Ctrl + C exits the whole program."
    )?;
    for diagnostic in &skills
        .lock()
        .map_err(|_| anyhow::anyhow!("skill registry unavailable"))?
        .diagnostics
    {
        writeln!(stderr, "skill: {diagnostic}")?;
    }
    for diagnostic in &file_commands.diagnostics {
        writeln!(stderr, "command: {diagnostic}")?;
    }
    if let Some(id) = resumed_id {
        writeln!(stderr, "Resumed session: {id}")?;
    }
    Ok(())
}

pub fn write_resume_hint(
    stderr: &mut (impl Write + IsTerminal),
    session_id: Option<&str>,
) -> io::Result<()> {
    if let Some(id) = session_id {
        if stderr.is_terminal() {
            let hint = format!(
                "{}\x1b[0m\n",
                format!("Resume with: pp --resume {id}").dark_grey()
            );
            stderr.write_all(hint.as_bytes())?;
        } else {
            writeln!(stderr, "Resume with: pp --resume {id}")?;
        }
    }
    Ok(())
}

// Display failures must not interrupt recording results of tools that already ran.
pub(crate) fn cli_write(
    writer: &mut impl Write,
    text: &str,
    broken: &mut bool,
    first_error: &mut Option<io::Error>,
) {
    if *broken {
        return;
    }
    if let Err(error) = writer
        .write_all(text.as_bytes())
        .and_then(|()| writer.flush())
    {
        *broken = true;
        if first_error.is_none() {
            *first_error = Some(error);
        }
    }
}

// 头尾窗口滚动后停止追加，结束时仅补显示最终尾窗，避免反复打印头部。
pub(crate) fn cli_output_delta(output: &str, previous: &mut String, finished: bool) -> String {
    if let Some(delta) = output.strip_prefix(previous.as_str()) {
        let delta = delta.to_owned();
        *previous = output.to_owned();
        delta
    } else if finished {
        let tail = output
            .find("[OUTPUT TRUNCATED:")
            .map_or(output, |offset| &output[offset..]);
        format!("\n[Final output preview]\n{tail}")
    } else {
        String::new()
    }
}

pub(crate) fn cli_output_text(
    output: &str,
    previous: &mut String,
    finished: bool,
    label: &str,
) -> String {
    let delta = cli_output_delta(output, previous, finished);
    if delta.is_empty() {
        if finished && !previous.is_empty() && !previous.ends_with('\n') {
            "\n".into()
        } else {
            String::new()
        }
    } else {
        format!(
            "[{label} output] {delta}{}",
            if finished && !delta.ends_with('\n') {
                "\n"
            } else {
                ""
            }
        )
    }
}

pub(crate) fn cli_output_result(
    value: &serde_json::Value,
    previous: &mut String,
    label: &str,
) -> String {
    let mut text = cli_output_text(
        value["output"].as_str().unwrap_or_default(),
        previous,
        true,
        label,
    );
    let notice = tools::output::notice(value);
    if !notice.is_empty() {
        text.push_str(&notice);
        text.push('\n');
    }
    // 阶段正文不进入主输出；这里只追加有界状态与补读 URI。
    if let Some(status) = tools::output::pipes_status(&value["pipes"]) {
        text.push_str(&status.notice());
        text.push('\n');
    }
    text
}

/// 行模式 `/pipes`：有界的 run/流列表与补读 URI，不输出阶段正文。
pub(crate) fn cli_pipes_list(store: &tools::PipeStore, run: Option<&str>) -> String {
    const MAX_RUNS: usize = 8;
    const MAX_STREAMS: usize = 8;
    let runs = store
        .list()
        .into_iter()
        .map(|info| info.to_json())
        .collect::<Vec<_>>();
    if runs.is_empty() {
        return "No temporary pipe runs in this session.\n".to_owned();
    }
    let selected = match run {
        Some(id) => match store.snapshot(id) {
            Ok(info) => info.to_json(),
            Err(error) => return format!("{}\n", error.message()),
        },
        None => runs[0].clone(),
    };
    let mut text = format!("Pipe runs: {} (newest first)\n", runs.len());
    for info in runs.iter().take(MAX_RUNS) {
        text.push_str(&format!("  {}\n", pipes_run_line(info)));
    }
    if runs.len() > MAX_RUNS {
        text.push_str(&format!("  … {} older runs\n", runs.len() - MAX_RUNS));
    }
    let uri = selected["uri"].as_str().unwrap_or_default();
    text.push_str(&format!("Selected run: {uri}\n"));
    let streams = selected["streams"].as_array().cloned().unwrap_or_default();
    if streams.is_empty() {
        text.push_str("  no captured streams\n");
    } else {
        for stream in streams.iter().take(MAX_STREAMS) {
            let pipeline = stream["pipeline"].as_u64().unwrap_or_default();
            let producer = stream["producer_stage"].as_u64().unwrap_or_default();
            let consumer = stream["consumer_stage"].as_u64().unwrap_or_default();
            let state = if stream["open"] == true {
                "open"
            } else {
                "closed"
            };
            let retained_start = stream["retained_start"].as_u64().unwrap_or_default();
            let retained_end = stream["retained_end"].as_u64().unwrap_or_default();
            let total = stream["total_bytes"].as_u64().unwrap_or_default();
            text.push_str(&format!(
                "  {} · pipeline {pipeline} stage {producer}→{consumer} · {state} · bytes {retained_start}..{retained_end} of {total}\n",
                stream["uri"].as_str().unwrap_or_default()
            ));
        }
        if streams.len() > MAX_STREAMS {
            text.push_str(&format!(
                "  … {} more streams\n",
                streams.len() - MAX_STREAMS
            ));
        }
        text.push_str(
            "Read a stream with read(path=\"pipe://<run>/<stream>\"); bodies are not printed here.\n",
        );
    }
    text
}

fn pipes_run_line(info: &serde_json::Value) -> String {
    let mut line = format!(
        "{} · {} · {}",
        info["uri"].as_str().unwrap_or_default(),
        info["owner"].as_str().unwrap_or("-"),
        info["state"].as_str().unwrap_or("unknown"),
    );
    if let Some(code) = info["exit_code"].as_i64() {
        line.push_str(&format!(" · exit {code}"));
    }
    let pipelines = info["pipelines"].as_array().map_or(0, Vec::len);
    let omitted = info["omitted_pipelines"].as_u64().unwrap_or(0);
    line.push_str(&format!(" · pipelines {pipelines}"));
    if omitted > 0 {
        line.push_str(&format!(" (+{omitted} omitted)"));
    }
    let nonzero = info["nonzero_stages"].as_u64().unwrap_or(0);
    if nonzero > 0 {
        line.push_str(&format!(" · Non-zero stage results: {nonzero}"));
    }
    if info["incomplete"] == true || info["capture_error"].is_string() {
        line.push_str(" · stage output incomplete");
    }
    line
}

pub(crate) fn run_cli_shell(
    agent: &mut agent::Agent,
    command: &str,
    stderr: &mut impl Write,
) -> Result<()> {
    use events::{AgentEvent, OutputStream};
    let mut broken = false;
    let mut first_error = None;
    let mut newline = true;
    let mut streamed_output = String::new();
    let result = agent.run_shell(
        command,
        "local-shell-cli",
        &cancellation::CancellationToken::new(),
        &mut |event| match event {
            AgentEvent::ToolStarted { .. } => {
                cli_write(stderr, "shell: running\n", &mut broken, &mut first_error)
            }
            AgentEvent::ToolPipelineSnapshot { .. } => {}
            AgentEvent::ToolOutputSnapshot { output, .. } => {
                let delta = cli_output_text(&output, &mut streamed_output, false, "shell");
                cli_write(stderr, &delta, &mut broken, &mut first_error);
                newline = delta.ends_with('\n');
            }
            AgentEvent::ToolOutput { stream, text, .. } => {
                let label = match stream {
                    OutputStream::Stdout => "stdout",
                    OutputStream::Stderr => "stderr",
                };
                cli_write(
                    stderr,
                    &format!("[shell {label}] {text}"),
                    &mut broken,
                    &mut first_error,
                );
                newline = text.ends_with('\n');
            }
            AgentEvent::ToolFinished { result, .. } => {
                if result["output"].is_string() {
                    cli_write(
                        stderr,
                        &cli_output_result(&result, &mut streamed_output, "shell"),
                        &mut broken,
                        &mut first_error,
                    );
                }
                if !newline {
                    cli_write(stderr, "\n", &mut broken, &mut first_error);
                }
                let status = if result["cancelled"] == true {
                    "shell: cancelled".to_owned()
                } else if result["timed_out"] == true {
                    "shell: timed out".to_owned()
                } else if let Some(code) = result["exit_code"].as_i64() {
                    format!("shell: exit {code}")
                } else {
                    "shell: failed".to_owned()
                };
                cli_write(
                    stderr,
                    &format!("{status}\n"),
                    &mut broken,
                    &mut first_error,
                );
                for key in ["error", "warning"] {
                    if let Some(message) = result[key].as_str() {
                        cli_write(
                            stderr,
                            &format!("shell: {message}\n"),
                            &mut broken,
                            &mut first_error,
                        );
                    }
                }
                if let Some(warnings) = result["warnings"].as_array() {
                    for warning in warnings.iter().filter_map(serde_json::Value::as_str) {
                        cli_write(
                            stderr,
                            &format!("shell: {warning}\n"),
                            &mut broken,
                            &mut first_error,
                        );
                    }
                }
                if result["stdout_truncated"] == true
                    || result["stderr_truncated"] == true
                    || result["truncated"] == true
                {
                    cli_write(
                        stderr,
                        "shell: output truncated\n",
                        &mut broken,
                        &mut first_error,
                    );
                }
            }
            _ => {}
        },
    );
    if result.as_ref().is_err_and(|error| {
        error
            .chain()
            .any(|cause| cause.is::<cancellation::CleanupError>())
    }) {
        return result;
    }
    if let Some(error) = first_error {
        return Err(error).context("writing shell output");
    }
    result
}

pub(crate) fn cli_context_event(
    event: events::AgentEvent,
    usage: &mut Option<context::ContextUsage>,
    stderr: &mut impl Write,
    broken: &mut bool,
    first_error: &mut Option<io::Error>,
) {
    match event {
        events::AgentEvent::ContextUpdated { usage: current } => *usage = Some(current),
        events::AgentEvent::CompactionStarted { .. } => {
            cli_write(stderr, "Compacting context...\n", broken, first_error)
        }
        events::AgentEvent::CompactionFinished {
            before_tokens,
            after_tokens,
        } => cli_write(
            stderr,
            &format!("Context compacted: ~{before_tokens} → ~{after_tokens} tokens (estimated)\n"),
            broken,
            first_error,
        ),
        _ => {}
    }
}

pub(crate) fn run_cli_compaction(agent: &mut agent::Agent, stderr: &mut impl Write) -> Result<()> {
    use agent::context::CompactionOutcome;
    let mut usage = None;
    let mut broken = false;
    let mut first_error = None;
    let result = agent.compact(&cancellation::CancellationToken::new(), &mut |event| {
        cli_context_event(event, &mut usage, stderr, &mut broken, &mut first_error);
    });
    match &result {
        Ok(CompactionOutcome::Unchanged) => cli_write(
            stderr,
            "Nothing to compact.\n",
            &mut broken,
            &mut first_error,
        ),
        Ok(CompactionOutcome::Cancelled) => cli_write(
            stderr,
            "Context compaction cancelled.\n",
            &mut broken,
            &mut first_error,
        ),
        _ => {}
    }
    if let Some(id) = agent.take_session_notice() {
        cli_write(
            stderr,
            &format!("Session: {id}\n"),
            &mut broken,
            &mut first_error,
        );
    }
    let usage = match usage {
        Some(usage) => usage,
        None => agent.context_usage()?,
    };
    cli_write(
        stderr,
        &usage.cli_status(&agent.current_model().reference),
        &mut broken,
        &mut first_error,
    );
    if let Some(error) = first_error {
        return Err(error).context("writing context output");
    }
    result.map(|_| ())
}

/// 行模式下单个执行单元：正常 Agent 任务，或 dev 构建的内嵌 mock 场景。
enum CliExecution<'a> {
    Agent {
        agent: &'a mut agent::Agent,
        prompt: Option<&'a str>,
    },
    #[cfg(debug_assertions)]
    Mock {
        test: mock_ai::MockTest,
        endpoint: mock_ai::MockEndpoint,
    },
}

pub fn run_cli_task(
    agent: &mut agent::Agent,
    prompt: Option<&str>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<()> {
    run_cli_execution(CliExecution::Agent { agent, prompt }, stdout, stderr)
}

/// dev/test 专用入口：用内嵌 mock server 运行真实 Agent 场景，不触碰正常会话。
#[cfg(debug_assertions)]
pub fn run_cli_mock_test(
    test: mock_ai::MockTest,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<()> {
    let endpoint = mock_ai::current_endpoint()?;
    run_cli_execution(CliExecution::Mock { test, endpoint }, stdout, stderr)
}

fn run_cli_execution(
    mut execution: CliExecution<'_>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> Result<()> {
    use events::{AgentEvent, OutputStream};
    match &execution {
        CliExecution::Agent { agent, .. } => {
            for warning in agent.mcp_view().warnings() {
                writeln!(stderr, "{warning}")?;
            }
        }
        #[cfg(debug_assertions)]
        CliExecution::Mock { .. } => {}
    }
    let mut first_error = None;
    let (mut out_broken, mut err_broken) = (false, false);
    let (mut visible, mut content, mut newline) = (false, false, true);
    let mut current_tool = None;
    let mut thinking_newline = true;
    let mut context_usage = None;
    let mut streamed_output = String::new();
    let mut emit = |event: AgentEvent| {
        // 运行时指标事件只服务 TUI，CLI 不输出，也不得触发思考换行。
        if matches!(
            &event,
            AgentEvent::ModelRequestStarted
                | AgentEvent::ToolArgumentsDelta { .. }
                | AgentEvent::ModelOutputProgress { .. }
                | AgentEvent::ModelRequestFinished { .. }
                | AgentEvent::UserInputAccepted { .. }
                | AgentEvent::PlanProgressUpdated { .. }
                | AgentEvent::ProjectInstructionsLoaded { .. }
        ) {
            return;
        }
        if !matches!(
            &event,
            AgentEvent::ThinkingDelta { .. } | AgentEvent::ReasoningSummaryDelta { .. }
        ) && !thinking_newline
        {
            cli_write(stderr, "\n", &mut err_broken, &mut first_error);
            thinking_newline = true;
        }
        match event {
            event @ (AgentEvent::ContextUpdated { .. }
            | AgentEvent::CompactionStarted { .. }
            | AgentEvent::CompactionFinished { .. }) => cli_context_event(
                event,
                &mut context_usage,
                stderr,
                &mut err_broken,
                &mut first_error,
            ),
            AgentEvent::ModelAttemptFailed {
                attempt,
                model,
                error,
            } => {
                if visible && !newline {
                    cli_write(stdout, "\n", &mut out_broken, &mut first_error);
                }
                cli_write(
                    stderr,
                    &format!(
                        "Model attempt {attempt} failed; partial output is excluded from history.\nModel: {model}\nReason: {error}\n"
                    ),
                    &mut err_broken,
                    &mut first_error,
                );
                visible = false;
                newline = true;
            }
            AgentEvent::ModelRouteChanged {
                model, fallback, ..
            } => {
                cli_write(
                    stderr,
                    &format!(
                        "{}: {model}\n",
                        if fallback {
                            "Using fallback model"
                        } else {
                            "Restoring primary model"
                        }
                    ),
                    &mut err_broken,
                    &mut first_error,
                );
            }
            AgentEvent::AssistantStarted => {
                visible = false;
                content = false;
                newline = true;
            }
            AgentEvent::ThinkingDelta { text } | AgentEvent::ReasoningSummaryDelta { text } => {
                if !text.is_empty() {
                    thinking_newline = text.ends_with('\n');
                }
                cli_write(stderr, &text, &mut err_broken, &mut first_error);
            }
            AgentEvent::AssistantDelta {
                text,
                refusal: false,
            } => {
                if !text.is_empty() {
                    cli_write(stdout, &text, &mut out_broken, &mut first_error);
                    visible = true;
                    content |= !text.trim().is_empty();
                    newline = text.ends_with('\n');
                }
            }
            AgentEvent::AssistantDelta { refusal: true, .. } => {}
            AgentEvent::AssistantFinished { text } => {
                if !content && !text.is_empty() {
                    cli_write(stdout, &text, &mut out_broken, &mut first_error);
                    visible = true;
                    newline = text.ends_with('\n');
                }
                if visible && !newline {
                    cli_write(stdout, "\n", &mut out_broken, &mut first_error);
                }
                visible = false;
                newline = true;
            }
            AgentEvent::ToolStarted { id, name, .. } => {
                cli_write(
                    stderr,
                    &format!("tool {name}: running\n"),
                    &mut err_broken,
                    &mut first_error,
                );
                streamed_output.clear();
                current_tool = Some((id, name));
            }
            AgentEvent::FileChanged { .. } => {}
            AgentEvent::ToolOutputSnapshot { id, output } => {
                if !current_tool
                    .as_ref()
                    .is_some_and(|(current_id, name)| current_id == &id && name == "wait")
                {
                    let delta = cli_output_text(&output, &mut streamed_output, false, "bash");
                    cli_write(stderr, &delta, &mut err_broken, &mut first_error);
                }
            }
            AgentEvent::ToolOutput { id, stream, text } => {
                let name = current_tool
                    .as_ref()
                    .filter(|(current_id, _)| current_id == &id)
                    .map_or("tool", |(_, name)| name.as_str());
                let label = match stream {
                    OutputStream::Stdout => "stdout",
                    OutputStream::Stderr => "stderr",
                };
                cli_write(
                    stderr,
                    &format!("[{name} {label}] {text}"),
                    &mut err_broken,
                    &mut first_error,
                );
            }
            AgentEvent::ToolFinished { id, result } => {
                if current_tool
                    .as_ref()
                    .is_some_and(|(current_id, name)| current_id == &id && name == "wait")
                    && let Some(jobs) = result["jobs"].as_array()
                {
                    let running = jobs
                        .iter()
                        .filter(|job| matches!(job["status"].as_str(), Some("queued" | "running")))
                        .count();
                    let suffix = if result["interrupted"] == true {
                        "; interrupted by steering"
                    } else if result["timed_out"] == true {
                        "; timed out"
                    } else {
                        ""
                    };
                    cli_write(
                        stderr,
                        &format!(
                            "wait: {} settled, {running} running{suffix}\n",
                            jobs.len() - running
                        ),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
                if result["output"].is_string() {
                    cli_write(
                        stderr,
                        &cli_output_result(&result, &mut streamed_output, "bash"),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
                if current_tool.as_ref().is_some_and(|(current_id, name)| {
                    current_id == &id
                        && (matches!(name.as_str(), "grep" | "glob" | "todo")
                            || name == "read"
                                && result["kind"] != "image"
                                && result.get("pipe").is_none())
                }) && let Some(content) = result["content"]
                    .as_str()
                    .or_else(|| result["error"].as_str())
                {
                    let content = if current_tool
                        .as_ref()
                        .is_some_and(|(_, name)| matches!(name.as_str(), "read" | "todo"))
                    {
                        pp_core::text::display_text(content)
                    } else {
                        std::borrow::Cow::Borrowed(content)
                    };
                    cli_write(
                        stderr,
                        &format!("{content}\n"),
                        &mut err_broken,
                        &mut first_error,
                    );
                    if let Some(warnings) = result["warnings"].as_array() {
                        for warning in warnings.iter().filter_map(serde_json::Value::as_str) {
                            cli_write(
                                stderr,
                                &format!("Warning: {warning}\n"),
                                &mut err_broken,
                                &mut first_error,
                            );
                        }
                    }
                }
                if result["kind"] == "image"
                    && let Some(content) = result["content"].as_str()
                {
                    cli_write(
                        stderr,
                        &format!("{content}\n"),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
                if let (Some(task_id), Some(status)) =
                    (result["task_id"].as_str(), result["status"].as_str())
                {
                    cli_write(
                        stderr,
                        &format!("task {task_id}: {status}\n"),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
                // 内联 LspDiagnostics 只来自 write/edit；lsp 工具正文在这里展示。
                let lsp_content = current_tool
                    .as_ref()
                    .is_some_and(|(current_id, name)| current_id == &id && name.as_str() == "lsp");
                if lsp_content && let Some(content) = result["content"].as_str() {
                    cli_write(
                        stderr,
                        &format!("\n{}\n", pp_core::text::display_text(content)),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
                if let Some((current_id, name)) = current_tool.take()
                    && current_id == id
                {
                    let status = if result["ok"] == true { "ok" } else { "failed" };
                    cli_write(
                        stderr,
                        &format!("\ntool {name}: {status}\n"),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
            }
            AgentEvent::TodoSnapshot {
                revision,
                phases,
                status,
                ..
            } => {
                let preview = if matches!(
                    status,
                    pp_core::plan::PlanStatus::Draft | pp_core::plan::PlanStatus::Ready
                ) {
                    " · Preview (not executing)"
                } else {
                    ""
                };
                cli_write(
                    stderr,
                    &format!(
                        "Plan tasks · {status:?} · r{revision}{preview}\n{}\n",
                        pp_core::text::display_text(&pp_runtime::tools::todo::format_summary(
                            &phases,
                            &[],
                            true
                        ))
                    ),
                    &mut err_broken,
                    &mut first_error,
                );
            }
            AgentEvent::TodoReminder {
                todos,
                attempt,
                max_attempts,
            } => {
                // stderr 提醒与 open 项；stdout 仍只保留助手文本。
                let open: Vec<&pp_core::todo::TodoItem> = todos
                    .iter()
                    .filter(|item| {
                        !matches!(
                            item.status,
                            pp_core::todo::TodoStatus::Completed
                                | pp_core::todo::TodoStatus::Abandoned
                        )
                    })
                    .collect();
                cli_write(
                    stderr,
                    &format!(
                        "{} incomplete todos - reminder {attempt}/{max_attempts}\n",
                        open.len()
                    ),
                    &mut err_broken,
                    &mut first_error,
                );
                for item in &open {
                    cli_write(
                        stderr,
                        &format!("  - {}\n", pp_core::text::display_text(&item.content)),
                        &mut err_broken,
                        &mut first_error,
                    );
                }
            }
            AgentEvent::RequestRetrying {
                reason,
                attempt,
                wait_ms,
            } => {
                cli_write(
                    stderr,
                    &format!(
                        "{reason}; retrying in {} (attempt {attempt})\n",
                        crate::tui::format_wait(wait_ms)
                    ),
                    &mut err_broken,
                    &mut first_error,
                );
            }
            AgentEvent::LspDiagnostics { id, report } => {
                cli_lsp_report(
                    &report,
                    id.is_none(),
                    stderr,
                    &mut err_broken,
                    &mut first_error,
                );
            }
            AgentEvent::ModelRequestStarted
            | AgentEvent::ToolArgumentsDelta { .. }
            | AgentEvent::ModelOutputProgress { .. }
            | AgentEvent::ModelRequestFinished { .. }
            | AgentEvent::UserInputAccepted { .. }
            | AgentEvent::ProjectInstructionsLoaded { .. }
            | AgentEvent::PlanProgressUpdated { .. }
            | AgentEvent::ToolPipelineSnapshot { .. }
            | AgentEvent::TodoStateUpdated { .. } => {}
        }
    };
    let cancellation = cancellation::CancellationToken::new();
    let result = match &mut execution {
        CliExecution::Agent { agent, prompt } => match prompt {
            Some(prompt) => agent.run(prompt, &cancellation, None, &mut emit),
            None => agent.continue_session(&cancellation, None, &mut emit),
        }
        .map(|_| ()),
        #[cfg(debug_assertions)]
        CliExecution::Mock { test, endpoint } => {
            mock_ai::run_test(*test, endpoint.clone(), None, &cancellation, &mut |event| {
                if let mock_ai::MockTestEvent::Progress(event) = event {
                    emit(event);
                }
            })
            .map(|_| ())
        }
    };
    if !thinking_newline {
        cli_write(stderr, "\n", &mut err_broken, &mut first_error);
    }
    if visible && !newline {
        cli_write(stdout, "\n", &mut out_broken, &mut first_error);
    }
    match &mut execution {
        CliExecution::Agent { agent, .. } => {
            // Report a newly created session even when the task itself failed;
            // stdout stays reserved for model answers.
            if let Some(id) = agent.take_session_notice() {
                cli_write(
                    stderr,
                    &format!("Session: {id}\n"),
                    &mut err_broken,
                    &mut first_error,
                );
            }
            if let Some(usage) = context_usage {
                cli_write(
                    stderr,
                    &usage.cli_status(&agent.current_model().reference),
                    &mut err_broken,
                    &mut first_error,
                );
            }
        }
        #[cfg(debug_assertions)]
        CliExecution::Mock { .. } => {
            if let Some(usage) = context_usage {
                cli_write(
                    stderr,
                    &usage.cli_status("mock-root/primary"),
                    &mut err_broken,
                    &mut first_error,
                );
            }
        }
    }
    if let Some(error) = first_error {
        return Err(error).context("writing task output");
    }
    result
}

/// 行模式交互循环：逐行读取 stdin、分发 `/command` 并输出结果。
///
/// `initial_command` 是 CLI 位置参数给出的启动命令：只合成一行、不打印提示符；
/// 未知命令或解析错误使本次运行失败（脚本可见非零退出码），真实 stdin 行保持原有语义。
pub fn run_line_mode(
    mut agent: Result<agent::Agent>,
    skills: skills::SharedSkills,
    file_commands: commands::FileCommands,
    initial_command: Option<String>,
) -> Result<()> {
    let mut stderr = io::stderr();
    let mut stdout = io::stdout().lock();
    writeln!(stderr, "pp — use /help for commands")?;
    if let Err(error) = &agent {
        writeln!(stderr, "error: {error:#}")?;
        writeln!(
            stderr,
            "Configuration unavailable; model tasks are disabled. Fix the config files and restart pp. /quit still exits cleanly."
        )?;
    }
    let input = match cli_input_lines() {
        Ok(input) => input,
        Err(error) => return stop_cli_lsp(agent.as_ref().ok(), Err(error)),
    };
    let mut initial = initial_command;
    let mut prompt_shown = false;
    loop {
        // 启动命令只合成一行：不打印提示符，执行后继续等待真实 stdin。
        let initial_line = initial.take();
        let synthetic = initial_line.is_some();
        if synthetic {
            prompt_shown = false;
        } else if !prompt_shown {
            write!(stderr, "> ")?;
            stderr.flush()?;
            prompt_shown = true;
        }
        let line = match initial_line {
            Some(line) => line,
            None => match input.recv_timeout(CLI_IDLE_TICK) {
                Ok(CliInput::Line(line)) => {
                    prompt_shown = false;
                    line
                }
                Ok(CliInput::Eof) => break,
                Ok(CliInput::Error(error)) => {
                    return stop_cli_lsp(agent.as_ref().ok(), Err(error).context("reading stdin"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // 空闲时只展示迟到诊断，不续跑模型、不自动继续会话。
                    let polled = match agent.as_mut() {
                        Ok(active) => {
                            let mut reports = Vec::new();
                            let polled =
                                active.poll_lsp_diagnostics(&mut |event| reports.push(event));
                            let mut broken = false;
                            let mut first_error = None;
                            if !reports.is_empty() {
                                if prompt_shown {
                                    cli_write(&mut stderr, "\n", &mut broken, &mut first_error);
                                }
                                for event in reports {
                                    if let events::AgentEvent::LspDiagnostics { report, .. } = event
                                    {
                                        cli_lsp_report(
                                            &report,
                                            true,
                                            &mut stderr,
                                            &mut broken,
                                            &mut first_error,
                                        );
                                    }
                                }
                                if prompt_shown {
                                    write!(stderr, "> ")?;
                                    stderr.flush()?;
                                }
                            }
                            match (polled, first_error) {
                                (Ok(()), None) => Ok(()),
                                (Ok(()), Some(error)) => {
                                    Err(error).context("writing LSP diagnostics")
                                }
                                (Err(error), _) => Err(error),
                            }
                        }
                        Err(_) => Ok(()),
                    };
                    if let Err(error) = polled {
                        return stop_cli_lsp(agent.as_ref().ok(), Err(error));
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            },
        };
        let prompt = line.trim();
        if prompt.is_empty() {
            continue;
        }
        let command = match commands::parse(line.trim_end_matches(['\r', '\n']), &file_commands) {
            Ok(command) => command,
            Err(error) => {
                if synthetic {
                    return stop_cli_lsp(agent.as_ref().ok(), Err(error));
                }
                writeln!(stderr, "error: {error:#}")?;
                continue;
            }
        };
        // 未知命令静默忽略，与 TUI 一致；显式启动命令例外，必须给出非零退出码。
        if matches!(command, Some(commands::Command::Unknown)) {
            if synthetic {
                return stop_cli_lsp(
                    agent.as_ref().ok(),
                    Err(anyhow::anyhow!("unknown command: {prompt}; use /help")),
                );
            }
            continue;
        }
        if matches!(command, Some(commands::Command::Subagents)) {
            if let Ok(agent) = agent.as_mut() {
                agent.ensure_subagents()?;
                if let Some(binding) = agent.subagents() {
                    writeln!(
                        stderr,
                        "{}",
                        serde_json::to_string_pretty(&binding.snapshots())?
                    )?;
                } else {
                    writeln!(
                        stderr,
                        "No subagents; use /Roles or configured .pp/agents and ~/.pp/agent/agents definitions."
                    )?;
                }
            }
            continue;
        }
        match command {
            Some(commands::Command::Roles) => {
                writeln!(
                    stderr,
                    "/Roles requires an interactive terminal; configure model_roles in settings.yml instead."
                )?;
                continue;
            }
            Some(commands::Command::Quit) => break,
            Some(commands::Command::Mcp(action)) => {
                if matches!(action, pp_runtime::mcp::Action::Help) {
                    writeln!(stderr, "{}", pp_runtime::mcp::help())?;
                } else {
                    match agent.as_mut() {
                        Ok(agent) => match agent
                            .manage_mcp(&action, &pp_core::cancellation::CancellationToken::new())
                        {
                            Ok(view) => writeln!(stderr, "{}", view.text())?,
                            Err(error) => {
                                if error.is::<pp_core::cancellation::CleanupError>() {
                                    return stop_cli_lsp(Some(&*agent), Err(error));
                                }
                                writeln!(stderr, "error: {error:#}")?;
                            }
                        },
                        Err(_) => writeln!(
                            stderr,
                            "MCP management unavailable: fix the startup config first"
                        )?,
                    }
                }
                continue;
            }
            Some(commands::Command::Skills(selection)) => {
                let result = (|| -> Result<()> {
                    let mut skills = skills
                        .lock()
                        .map_err(|_| anyhow::anyhow!("skill registry unavailable"))?;
                    match selection {
                        commands::SkillCommand::List | commands::SkillCommand::Sources => {}
                        commands::SkillCommand::Toggle(name, enabled) => {
                            skills.save_selection(&[(name.to_owned(), enabled)])?
                        }
                        commands::SkillCommand::Source(source, enabled) => {
                            skills.save_source(source, enabled)?
                        }
                    }
                    for source in skills::SkillSource::ALL {
                        writeln!(
                            stderr,
                            "[{}] source:{} ({})",
                            if skills.sources.contains(&source) {
                                "x"
                            } else {
                                " "
                            },
                            source.name(),
                            source.roots().join(", ")
                        )?;
                    }
                    for skill in &skills.entries {
                        writeln!(
                            stderr,
                            "[{}] {} — {} ({})",
                            if skill.enabled { "x" } else { " " },
                            skill.name,
                            skill.description,
                            skill.path.display()
                        )?;
                    }
                    for diagnostic in &skills.diagnostics {
                        writeln!(stderr, "skill: {diagnostic}")?;
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    writeln!(stderr, "error: {error:#}")?;
                }
                continue;
            }
            Some(commands::Command::Help) => {
                writeln!(stderr, "{}", commands::help(&file_commands))?;
                continue;
            }
            Some(commands::Command::Tips) => {
                writeln!(stderr, "{}", tips::text())?;
                continue;
            }
            // 行模式没有会话选择界面；按 ID 恢复仍用启动参数。
            Some(commands::Command::Resume) => {
                writeln!(
                    stderr,
                    "/resume requires the TUI; use pp --resume SESSION_ID."
                )?;
                continue;
            }
            Some(commands::Command::Tree) => {
                writeln!(stderr, "/tree requires the TUI; run pp in a terminal.")?;
                continue;
            }
            Some(commands::Command::Changes) => {
                writeln!(stderr, "/changes requires the TUI; run pp in a terminal.")?;
                continue;
            }
            Some(commands::Command::Plan(_)) => {
                writeln!(stderr, "/plan requires the TUI; run pp in a terminal.")?;
                continue;
            }
            Some(commands::Command::Login | commands::Command::Logout) => {
                writeln!(
                    stderr,
                    "/login and /logout require the TUI; run pp in a terminal."
                )?;
                continue;
            }
            Some(commands::Command::PlanList) => {
                writeln!(stderr, "/plan-list requires the TUI; run pp in a terminal.")?;
                continue;
            }
            _ => {}
        }
        // `/test-*` 不依赖真实配置：启动失败时仍使用内嵌 mock server。
        #[cfg(debug_assertions)]
        if let Some(commands::Command::MockTest(test)) = &command {
            if let Err(error) = run_cli_mock_test(*test, &mut stdout, &mut stderr) {
                if error
                    .chain()
                    .any(|cause| cause.is::<cancellation::CleanupError>())
                {
                    return stop_cli_lsp(agent.as_ref().ok(), Err(error));
                }
                writeln!(stderr, "error: {error:#}")?;
            }
            continue;
        }
        let agent = match &mut agent {
            Ok(agent) => agent,
            Err(error) => {
                writeln!(stderr, "error: {error:#}")?;
                continue;
            }
        };
        match command {
            Some(commands::Command::Pipes(run)) => {
                let listed = cli_pipes_list(&agent.pipe_store(), run);
                write!(stderr, "{listed}")?;
            }
            Some(commands::Command::Shell(command)) => {
                if let Err(error) = run_cli_shell(agent, command, &mut stderr) {
                    if error
                        .chain()
                        .any(|cause| cause.is::<cancellation::CleanupError>())
                    {
                        return stop_cli_lsp(Some(&*agent), Err(error));
                    }
                    writeln!(stderr, "error: {error:#}")?;
                }
            }
            Some(commands::Command::Think(level)) => {
                if let Some(level) = level
                    && let Err(error) = agent.set_think_level(level)
                {
                    writeln!(stderr, "error: {error:#}")?;
                    continue;
                }
                write!(
                    stderr,
                    "Thinking level: {} (options: ",
                    agent.think_level().label()
                )?;
                for (index, level) in agent.current_model().thinking.levels().enumerate() {
                    if index != 0 {
                        write!(stderr, ", ")?;
                    }
                    write!(stderr, "{}", level.as_str())?;
                }
                writeln!(stderr, ")")?;
            }
            Some(commands::Command::Compact) => {
                if let Err(error) = run_cli_compaction(agent, &mut stderr) {
                    writeln!(stderr, "error: {error:#}")?;
                }
            }
            Some(commands::Command::New) => {
                agent.reset();
                writeln!(stderr, "Session reset.")?;
            }
            Some(commands::Command::Model(None)) => {
                if let Err(error) = agent.reload_models() {
                    writeln!(stderr, "error: {error:#}")?;
                }
                for choice in agent.model_choices() {
                    let marker = if choice.reference == agent.current_model().reference {
                        "*"
                    } else {
                        " "
                    };
                    writeln!(
                        stderr,
                        "{marker} {} ({}) [{}]",
                        choice.reference, choice.model, choice.protocol
                    )?;
                }
                writeln!(stderr, "Use /model provider/model to switch.")?;
            }
            Some(commands::Command::Model(Some(reference))) => {
                match agent
                    .reload_models()
                    .and_then(|()| agent.switch_model(reference))
                {
                    Ok(()) => {
                        let choice = agent.current_model();
                        writeln!(
                            stderr,
                            "Current model: {} ({})",
                            choice.reference, choice.model
                        )?;
                    }
                    Err(error) => writeln!(stderr, "error: {error:#}")?,
                }
            }
            Some(
                commands::Command::Help
                | commands::Command::Tips
                | commands::Command::Quit
                | commands::Command::Skills(_)
                | commands::Command::Mcp(_)
                | commands::Command::Resume
                | commands::Command::Tree
                | commands::Command::Changes
                | commands::Command::Roles
                | commands::Command::Subagents
                | commands::Command::PlanList
                | commands::Command::Login
                | commands::Command::Logout
                | commands::Command::Plan(_),
            ) => unreachable!(),
            Some(commands::Command::Unknown) => unreachable!(),
            #[cfg(debug_assertions)]
            Some(commands::Command::MockTest(_)) => unreachable!(),
            Some(commands::Command::Prompt(prompt)) => {
                if let Err(error) = run_cli_task(agent, Some(&prompt), &mut stdout, &mut stderr) {
                    writeln!(stderr, "error: {error:#}")?;
                }
            }
            None => match run_cli_task(agent, Some(prompt), &mut stdout, &mut stderr) {
                Ok(()) => {}
                Err(error) => writeln!(stderr, "error: {error:#}")?,
            },
        }
    }
    let hint = match agent.as_ref().ok().and_then(|agent| agent.session_id()) {
        Some(id) => writeln!(stderr)
            .and_then(|()| write_resume_hint(&mut stderr, Some(id)))
            .map_err(anyhow::Error::from),
        None => Ok(()),
    };
    stop_cli_lsp(agent.as_ref().ok(), hint)
}
