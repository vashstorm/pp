// TUI 上下文压缩生命周期（计划验证项 4）：真实 100 列 PTY 驱动 —— 状态栏
// Ctx 可见、手动或自动压缩被服务端握住摘要 SSE 时的 Compacting 状态、Esc
// 取消后草稿与已接受输入/工具结果保留且不继续普通请求、成功压缩后旧对话
// 保留而用量下降且 Compact Note 先于任务 assistant、下一个普通请求只含
// 摘要包装与原生近期后缀、窄屏 resize 的上下文字段优先与 Ctrl-C 终端标志
// 恢复、达到配置阈值时上下文字段变黄，以及 ulimit -f 下压缩 state 写入
// 留下 partial tail 后恢复读取此前完整快照。此外覆盖输入栏上沿的实时 token
// 指标：流中不带前缀的估算速度、权威 usage 后的平均速度与 In/Out/CRead/CWrite 配色、
// 缺失 usage 与 /new 的清理、取消后的速率停止，以及 60/40/20 列下 Ctx 优先
// 与指标整块降级。辅助项与测试名均以 context_ 开头；不新增 use 声明，直接
// 复用同模块私有 Pty 与既有 HTTP fixture。

const CONTEXT_TUI_SUMMARY: &str = "SUMMARY: kept constraint";
const CONTEXT_TUI_INSTRUCTION: &str = "Summarize the preceding conversation";

fn context_tui_settings(auto_compact: bool, threshold_percent: u8) -> String {
    format!(
        "{{\"default_model\":\"fixture/primary\",\"auto_compact\":{auto_compact},\
\"auto_compact_threshold_percent\":{threshold_percent},\
\"notification_timeout_seconds\":2}}"
    )
}

// 首次启动仍由旧四字段经迁移导入 agent.db；迁移完成后 settings.yml 只保留
// 非偏好字段，后续重启改写策略时不得再写 default_model。
fn context_tui_live_settings(auto_compact: bool, threshold_percent: u8) -> String {
    format!(
        "{{\"auto_compact\":{auto_compact},\
\"auto_compact_threshold_percent\":{threshold_percent},\
\"notification_timeout_seconds\":2}}"
    )
}

fn context_tui_models(server: &Server, protocol: &str, window: u64) -> String {
    json!({
        "providers": {"fixture": {
            "protocol": protocol,
            "api_key": "fixture-key",
            "base_url": format!("http://{}/v1", server.server_addr()),
            "models": {"primary": { "model": "fixture-model",
                        "context_window": window}}
        }}
    })
    .to_string()
}

// 独立 HOME 的双配置：默认自动压缩关闭、通知 2 秒。
fn context_tui_start_with_policy(
    server: &Server,
    protocol: &str,
    window: u64,
    threshold_percent: u8,
    auto_compact: bool,
) -> (tempfile::TempDir, Pty) {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        config_path(home.path(), "settings.yml"),
        context_tui_settings(auto_compact, threshold_percent),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        context_tui_models(server, protocol, window),
    )
    .unwrap();
    let mut pty = Pty::start_with_existing_files(home.path());
    pty.ready();
    (home, pty)
}

fn context_tui_start(
    server: &Server,
    protocol: &str,
    window: u64,
    threshold_percent: u8,
) -> (tempfile::TempDir, Pty) {
    context_tui_start_with_policy(server, protocol, window, threshold_percent, false)
}

fn context_tui_start_auto(
    server: &Server,
    protocol: &str,
    window: u64,
    threshold_percent: u8,
) -> (tempfile::TempDir, Pty) {
    context_tui_start_with_policy(server, protocol, window, threshold_percent, true)
}

// 括号粘贴一次送达大段文本，避免逐键事件。
fn context_tui_paste(pty: &mut Pty, text: &str) {
    pty.input(&format!("\x1b[200~{text}\x1b[201~"));
}

fn context_tui_filler(marker: &str, bytes: usize) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes + 100);
    let mut line = 0;
    while text.len() < bytes {
        writeln!(text, "{marker}-{line:04} xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx").unwrap();
        line += 1;
    }
    text.truncate(bytes);
    text
}

// 一轮交互：粘贴提示词、回答请求、等待回答与空闲输入框。
fn context_tui_exchange(
    pty: &mut Pty,
    server: &Server,
    protocol: &str,
    prompt: &str,
    answer: &str,
) {
    context_tui_paste(pty, prompt);
    pty.input("\r");
    let (request, _) = pty.request_at(server, api_path(protocol));
    // 模型面板只在 worker 的终结事件后出现，用它作为完成栅栏。
    pty.input("/model ");
    let answer = format!("{answer} {}", prompt.split_whitespace().next().unwrap());
    if protocol == "completion" {
        final_reply(request, &answer);
    } else {
        response_final(request, &answer);
    }
    pty.model_search("");
    pty.text("fixture/primary");
    pty.input("\x1b");
    pty.draft("");
    pty.text(&answer);
}

// 从输入栏上沿的 `Ctx N/W` 读取简写后的估算 token 数。
fn context_tui_ctx_tokens(screen: &vt100::Screen) -> f64 {
    let contents = screen.contents();
    let start = contents.find("Ctx ").expect("状态栏必须显示 Ctx 字段");
    let rest = &contents[start + "Ctx ".len()..];
    let figure: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    let value: f64 = figure.parse().expect("Ctx 数字必须可解析");
    match rest[figure.len()..].chars().next() {
        Some('k') => value * 1000.0,
        Some('m') => value * 1_000_000.0,
        _ => value,
    }
}

// 从当前用量与容量计算百分比，界面本身只显示大小。
fn context_tui_ctx_percent(screen: &vt100::Screen) -> u128 {
    let contents = screen.contents();
    let start = contents.find("Ctx ").expect("状态栏必须显示 Ctx 字段");
    let rest = &contents[start..];
    let (_, capacity) = rest.split_once('/').expect("已知容量必须显示总容量");
    let figure: String = capacity
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    let mut window: f64 = figure.parse().expect("Ctx 容量必须可解析");
    window *= match capacity[figure.len()..].chars().next() {
        Some('k') => 1000.0,
        Some('m') => 1_000_000.0,
        _ => 1.0,
    };
    (context_tui_ctx_tokens(screen) * 100.0 / window) as u128
}

// 在真实屏幕单元格中定位文本，返回 (行, 起始列)。列号必须来自 cell 扫描：
// Nerd 图标与宽字符的显示宽度既不等于 UTF-8 字节数，也不等于 chars().count()。
fn context_tui_locate(screen: &vt100::Screen, needle: &str) -> Option<(u16, u16)> {
    let (rows, columns) = screen.size();
    for row in 0..rows {
        for start in 0..columns {
            let mut text = String::new();
            for column in start..columns {
                let Some(cell) = screen.cell(row, column) else {
                    break;
                };
                text.push_str(cell.contents());
                if text == needle {
                    return Some((row, start));
                }
                if !needle.starts_with(text.as_str()) {
                    break;
                }
            }
        }
    }
    None
}

// 上下文字段首字符的前景色：达到阈值时为终端调色板 3 号（黄）。
fn context_tui_ctx_color(pty: &Pty) -> vt100::Color {
    let screen = pty.parser.screen();
    let (row, column) =
        context_tui_locate(screen, "Ctx ").expect("状态栏必须显示 Ctx 字段，且必须能在真实 cell 上定位");
    screen.cell(row, column).expect("Ctx 首字符").fgcolor()
}

// 未完成任何普通请求时五个指标都是未知，而不是伪装成 0。
fn context_tui_metrics_unknown(screen: &vt100::Screen) -> bool {
    let contents = screen.contents();
    ["— Tok/s", "In —", "Out —", "CRead —", "CWrite —"]
        .iter()
        .all(|field| contents.contains(*field))
}

// 指标的图标与字段文本必须同时出现：整字段降级，不允许截断字形或半个字段。
fn context_tui_assert_metric_whole(screen: &vt100::Screen, icon: &str, text: &str) {
    assert_eq!(
        context_tui_locate(screen, icon).is_some(),
        context_tui_locate(screen, text).is_some(),
        "指标字段必须整块显示或整块隐藏：{icon:?} 与 {text:?}\n{}",
        screen.contents()
    );
}

// 速度字段的数值与是否带前缀：字段自身格式为 `N.N[k|m] Tok/s`，未知时只有 `— Tok/s`。
fn context_tui_speed(screen: &vt100::Screen) -> Option<(f64, bool)> {
    let (row, column) = context_tui_locate(screen, "Tok/s")?;
    let head: String = (0..column)
        .rev()
        .take(32)
        .filter_map(|column| screen.cell(row, column))
        .map(|cell| cell.contents())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let head = head.trim_end();
    let (head, multiplier) = if let Some(head) = head.strip_suffix('k') {
        (head, 1000.0)
    } else if let Some(head) = head.strip_suffix('m') {
        (head, 1_000_000.0)
    } else {
        (head, 1.0)
    };
    let figure: String = head
        .chars()
        .rev()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if figure.is_empty() {
        return None;
    }
    Some((
        figure.parse::<f64>().ok()? * multiplier,
        head[..head.len() - figure.len()].ends_with('~'),
    ))
}

// 非零速度必须直接显示数值，不带 ~ 前缀。
fn context_tui_positive_speed(screen: &vt100::Screen) -> bool {
    context_tui_speed(screen).is_some_and(|(rate, marked)| !marked && rate > 0.0)
}

// 流式请求的权威收尾：Chat 用 usage-only 尾块 + DONE，Responses 用 completed
// 事件携带同一 usage；文本与已发送的增量一致，供速率按服务端 output 校正。
fn context_tui_stream_usage(stream: &mut SseReply, protocol: &str, text: &str) {
    if protocol == "completion" {
        stream.event(chat_chunk(json!({}), Some("stop")));
        stream.event(json!({
            "id":"chatcmpl-fixture", "object":"chat.completion.chunk",
            "created":1, "model":"fixture-model", "choices":[],
            "usage":{"prompt_tokens":12000, "completion_tokens":300,
                     "total_tokens":12300,
                     "prompt_tokens_details":{"cached_tokens":40,"cache_write_tokens":120}}
        }));
        stream.frame("[DONE]");
    } else {
        let mut response = response_body(json!([response_message(
            "msg-live",
            json!([response_text(text)])
        )]));
        response["usage"] = json!({
            "input_tokens":12000,
            "input_tokens_details":{"cached_tokens":40,"cache_write_tokens":120},
            "output_tokens":300,
            "output_tokens_details":{"reasoning_tokens":50},
            "total_tokens":12300
        });
        stream.event(json!({
            "type":"response.completed", "sequence_number":100, "response":response
        }));
    }
}

// 摘要请求的回复：与普通流一样收尾，但压缩流不进入指标（由空回调隔离）。
fn context_tui_summary_reply(stream: &mut SseReply, protocol: &str) {
    if protocol == "completion" {
        stream.event(chat_chunk(
            json!({"role":"assistant","content":CONTEXT_TUI_SUMMARY}),
            None,
        ));
        stream.event(chat_chunk(json!({}), Some("stop")));
        stream.frame("[DONE]");
    } else {
        stream.event(json!({
            "type":"response.completed", "sequence_number":100,
            "response": response_body(json!([response_message(
                "msg-summary",
                json!([response_text(CONTEXT_TUI_SUMMARY)])
            )]))
        }));
    }
}

#[test]
fn context_tui_shows_current_and_window_on_input_top_right() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 1_280_000, 80);
        assert_eq!(context_tui_ctx_tokens(pty.parser.screen()), 0.0);
        let contents = pty.parser.screen().contents();
        let lines: Vec<_> = contents.lines().collect();
        assert!(lines[26].trim_end().ends_with("/1.3m"));
        assert!(!lines[26].contains('%'));
        // 右对齐位置用真实 screen cell 验证；新指标占据左侧，字面长度不再固定。
        let (row, column) = context_tui_locate(pty.parser.screen(), "Ctx 0/1.3m")
            .expect("状态栏必须在输入栏上沿显示 Ctx 容量");
        assert_eq!(row, 26);
        assert_eq!(usize::from(column) + "Ctx 0/1.3m".len(), 99);
        assert_eq!(context_tui_ctx_color(&pty), vt100::Color::Rgb(122, 162, 247));
        assert!(!lines[29].contains("Ctx "));
        pty.input("context-position-draft");
        pty.draft("context-position-draft");
        assert_eq!(context_tui_ctx_tokens(pty.parser.screen()), 0.0);
        pty.resize(12, 40);
        pty.wait_for("缩放后用量仍在输入栏上沿右端", |screen| {
            screen.contents().lines().nth(8).is_some_and(|line| {
                line.contains(" Ctx ") && line.trim_end().ends_with("/1.3m")
            })
        });
        pty.draft("context-position-draft");
        assert_eq!(context_tui_ctx_tokens(pty.parser.screen()), 0.0);
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn context_tui_new_session_resets_context_to_zero_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 40000, 80);
        context_tui_exchange(
            &mut pty,
            &fixture,
            protocol,
            "context-first-message",
            "context-first-answer",
        );
        assert!(context_tui_ctx_tokens(pty.parser.screen()) > 0.0);
        pty.input("/new\r");
        pty.wait_for("新会话上下文归零", |screen| {
            screen.contents().contains("Ctx 0/40.0k")
        });
        context_tui_exchange(
            &mut pty,
            &fixture,
            protocol,
            "context-after-new",
            "context-after-new-answer",
        );
        assert!(context_tui_ctx_tokens(pty.parser.screen()) > 0.0);
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

// 摘要请求的形状：取消工具配置、携带输出上限、只含旧前缀与一条固定
// 摘要指令；近期后缀绝不发送给摘要模型。
fn context_tui_assert_summary_body(
    body: &Value,
    protocol: &str,
    old_marker: &str,
    recent_marker: &str,
) {
    let text = serde_json::to_string(body).unwrap();
    assert!(text.contains(CONTEXT_TUI_INSTRUCTION));
    assert!(text.contains(old_marker), "旧前缀必须参与摘要");
    assert!(!text.contains(recent_marker), "近期后缀不得发给摘要模型");
    assert!(
        body["tools"].as_array().is_none(),
        "摘要请求必须取消工具配置"
    );
    if protocol == "completion" {
        assert_eq!(body["max_completion_tokens"], json!(4096));
        let instruction = body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(instruction["role"], "user");
        assert!(instruction["content"].as_str().unwrap().contains(CONTEXT_TUI_INSTRUCTION));
    } else {
        assert_eq!(body["max_output_tokens"], json!(4096));
        let instruction = body["input"].as_array().unwrap().last().unwrap();
        assert_eq!(instruction["role"], "user");
        assert!(slash_message_text(instruction).contains(CONTEXT_TUI_INSTRUCTION));
    }
}

// 三个 8KB 交互使旧前缀超过 40000/5 token 的保留预算，
// 摘要只覆盖第一个交互，最近交互始终原生保留。
fn context_tui_fill_three_exchanges(pty: &mut Pty, server: &Server, protocol: &str) {
    for marker in ["context-old-one", "context-old-two", "context-recent-keep"] {
        context_tui_exchange(
            pty,
            server,
            protocol,
            &context_tui_filler(marker, 8000),
            "noted",
        );
    }
}

// 首个 35KB 批次低于 32768 的 80% 阈值；第二个 40KB 批次完成后完整历史
// 越过阈值，但保留的最近批次加系统/工具仍低于阈值，下一次普通请求前必须
// 自动摘要且候选可被接受。
const CONTEXT_TUI_AUTO_FIRST_BYTES: usize = 35_000;
const CONTEXT_TUI_AUTO_SECOND_BYTES: usize = 40_000;

fn context_tui_write_batch(home: &Path, name: &str, marker: &str, bytes: usize) {
    std::fs::write(home.join(name), context_tui_filler(marker, bytes)).unwrap();
}

fn context_tui_read_calls(request: Request, protocol: &str, id: &str, path: &str) {
    let args = json!({"path": path});
    if protocol == "completion" {
        calls_reply(request, json!([call(id, "read", args)]));
    } else {
        response_reply(request, json!([response_call(id, "read", args)]));
    }
}

fn context_tui_final(request: Request, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

// 向上翻页直到旧对话标记可见；失败时如实返回 false，调用方仍可翻回底部。
fn context_tui_page_to(pty: &mut Pty, needle: &str) -> bool {
    for _ in 0..80 {
        if pty.parser.screen().contents().contains(needle) {
            return true;
        }
        if !resume_history_page(pty, "\x1b[5~") {
            return false;
        }
    }
    pty.parser.screen().contents().contains(needle)
}

fn context_tui_page_to_bottom(pty: &mut Pty) {
    for _ in 0..80 {
        if !resume_history_page(pty, "\x1b[6~") {
            break;
        }
    }
}

// 保存以完整工具批次结尾的大会话，返回 ID 和实际日志长度。
fn context_tui_seed_large_session(
    home: &Path,
    server: &Server,
    protocol: &str,
) -> (String, u64) {
    std::fs::write(
        config_path(home, "settings.yml"),
        context_tui_settings(false, 80),
    )
    .unwrap();
    std::fs::write(
        config_path(home, "models.yml"),
        context_tui_models(server, protocol, 32768),
    )
    .unwrap();
    let mut settings: Value = serde_json::from_str(&context_tui_settings(false, 80)).unwrap();
    settings["rate_limit_retry_budget_seconds"] = json!(0);
    let mut cli = super::streaming::LiveCli::new(Cli::start_with(
        home,
        Some("context checkpoint seed prompt"),
        &settings.to_string(),
        &context_tui_models(server, protocol, 32768),
    ));
    let (request, _) = receive_at(server, api_path(protocol));
    context_tui_read_calls(
        request,
        protocol,
        "context-checkpoint-one",
        "context-checkpoint-one.txt",
    );
    let (request, body) = receive_at(server, api_path(protocol));
    assert!(
        serde_json::to_string(&body)
            .unwrap()
            .contains("context-checkpoint-one"),
        "seeded history must include the first tool result"
    );
    context_tui_read_calls(
        request,
        protocol,
        "context-checkpoint-two",
        "context-checkpoint-two.txt",
    );
    // 模型错误终止种子任务：日志停在第 2 个完整工具批次，且不会出现
    // 之后的普通文本交换（保留机制会丢弃两个大批次）。
    let (request, _) = receive_at(server, api_path(protocol));
    request
        .respond(
            Response::from_string(
                json!({"error":{"message":"seed termination","type":"server_error"}}).to_string(),
            )
            .with_status_code(500),
        )
        .unwrap();
    cli.wait_for(super::streaming::Pipe::Stderr, "error: ");
    // End at the complete second tool batch, not a later plain assistant
    // exchange that would let retention discard both large batches.
    cli.input("/quit\n");
    let output = cli.finish();
    let _ = output;
    no_more(server);
    let id = saved_session_id(home);
    let path = home
        .join(".pp/agent/sessions")
        .join(format!("{id}.jsonl"));
    let bytes = std::fs::read(&path).unwrap();
    (id, bytes.len() as u64)
}

// 验证项 4（手动）：摘要 SSE 被服务端握住时屏幕可见 Compacting；压缩期间
// 保留新草稿；Esc 取消后等待真实终态回到 Ready，草稿不动，下一个任务请求
// 携带完整旧历史与新输入，供应商摘要输出永不进入可见对话。
#[test]
fn context_tui_esc_cancel_during_held_summary_preserves_draft_and_history() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 40000, 80);
        context_tui_fill_three_exchanges(&mut pty, &fixture, protocol);
        pty.input("/compact\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        context_tui_assert_summary_body(&body, protocol, "context-old-one", "context-recent-keep");
        // 服务端握住摘要 SSE：屏幕保持 Compacting 状态。
        let _held = SseReply::new(request);
        pty.text("Compacting context...");
        // 压缩期间继续编辑草稿；Esc 取消并等待压缩终态。
        pty.input("context-draft-保留");
        pty.draft("context-draft-保留");
        pty.input("\x1b");
        pty.text("Context compaction cancelled.");
        pty.ready();
        assert!(!pty.parser.screen().contents().contains("Compacting context..."));
        pty.draft("context-draft-保留");
        // 可继续任务：取消后的下一次普通请求包含完整旧历史与新输入。
        pty.input("吧\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let text = serde_json::to_string(&body).unwrap();
        assert!(text.contains("context-draft-保留吧"));
        assert!(text.contains("context-old-one"), "取消不得丢弃旧历史");
        assert!(text.contains("context-recent-keep"));
        assert!(!text.contains("[Context summary]"));
        assert!(
            !text.contains(CONTEXT_TUI_INSTRUCTION),
            "取消后不得残留摘要指令"
        );
        assert_eq!(cancel_user_messages(&body, protocol).len(), 4);
        if protocol == "completion" {
            final_reply(request, "context recovered answer");
        } else {
            response_final(request, "context recovered answer");
        }
        pty.text("context recovered answer");
        pty.ready();
        assert!(!pty.parser.screen().contents().contains("SUMMARY"));
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

// 验证项 4（手动）：成功压缩不显示任务 Completed、不添加 You: /compact、
// 不清空 Transcript；状态栏用量下降；下一个普通请求只含一个摘要包装与
// 原生近期后缀；60/30 列 resize 上下文字段优先保留，Ctrl-C 恢复终端标志。
#[test]
fn context_tui_compact_success_keeps_dialogue_drops_usage_and_survives_narrow_resize() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 40000, 80);
        pty.text("Ctx ");
        context_tui_fill_three_exchanges(&mut pty, &fixture, protocol);
        let before = context_tui_ctx_tokens(pty.parser.screen());
        pty.input("/compact\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        context_tui_assert_summary_body(&body, protocol, "context-old-one", "context-recent-keep");
        pty.input("/model ");
        if protocol == "completion" {
            reply(
                request,
                json!({"role": "assistant", "content": CONTEXT_TUI_SUMMARY}),
            );
        } else {
            response_final(request, CONTEXT_TUI_SUMMARY);
        }
        pty.model_search("");
        pty.text("fixture/primary");
        pty.input("\x1b");
        pty.draft("");
        pty.text("Context compacted:");
        let after = context_tui_ctx_tokens(pty.parser.screen());
        assert!(
            after < before,
            "压缩后估算必须下降：before {before} after {after}"
        );
        // 旧对话仍在 Transcript：向上翻页可见最早的旧标记。
        let mut paged = false;
        for _ in 0..80 {
            if pty.parser.screen().contents().contains("context-old-one") {
                paged = true;
                break;
            }
            if !resume_history_page(&mut pty, "\x1b[5~") {
                break;
            }
        }
        assert!(paged, "压缩不得清除旧对话：{}", pty.diagnostics());
        // 摘要文本与摘要指令永不进入可见对话，命令本身也不作为对话条目。
        let visible = pty.parser.screen().contents();
        assert!(!visible.contains(CONTEXT_TUI_SUMMARY), "{visible}");
        assert!(!visible.contains(CONTEXT_TUI_INSTRUCTION), "{visible}");
        assert!(!visible.contains("/compact"), "{visible}");
        for _ in 0..80 {
            if !resume_history_page(&mut pty, "\x1b[6~") {
                break;
            }
        }
        // 下一个普通请求：一个摘要包装 + 原生近期后缀，无旧大段原文。
        pty.input("context-after-question\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let text = serde_json::to_string(&body).unwrap();
        assert_eq!(text.matches("[Context summary]").count(), 1);
        assert!(text.contains(CONTEXT_TUI_SUMMARY));
        assert!(text.contains("context-recent-keep"));
        assert!(!text.contains("context-old-one"), "旧前缀原文不得复活");
        assert!(text.contains("context-after-question"));
        assert!(text.contains("noted context-recent-keep"));
        if protocol == "completion" {
            final_reply(request, "context after answer");
        } else {
            response_final(request, "context after answer");
        }
        pty.text("context after answer");
        pty.ready();
        // 窄屏 resize：上下文字段优先保留，任何宽度不溢出、不 panic。
        // resize 的首批字节可能来自上一帧，须以目标布局出现作为重绘栅栏。
        for width in [60, 30] {
            pty.resize(30, width);
            pty.wait_for("窄屏重绘保留上下文字段", |screen| {
                screen.contents().lines().nth(26).is_some_and(|line| line.contains("Ctx "))
            });
        }
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

// 验证项 4（阈值颜色）：auto_compact 关闭时阈值仍驱动状态栏配色；以实测
// 估算为基准把第二个交互推入 [阈值, 100%) 区间，上下文字段变为黄色。
#[test]
fn context_tui_context_field_turns_yellow_at_configured_threshold() {
    let fixture = server();
    let (_home, mut pty) = context_tui_start(&fixture, "completion", 40000, 20);
    context_tui_exchange(
        &mut pty,
        &fixture,
        "completion",
        "remember the color constraint",
        "noted",
    );
    let base = context_tui_ctx_tokens(pty.parser.screen());
    let target = 25000.0_f64.max(base + 3000.0);
    let filler_bytes = ((target - base) * 3.0) as usize;
    context_tui_exchange(
        &mut pty,
        &fixture,
        "completion",
        &context_tui_filler("context-color-fill", filler_bytes),
        "noted",
    );
    let percent = context_tui_ctx_percent(pty.parser.screen());
    assert!(percent >= 20, "百分比 {percent} 必须越过阈值");
    assert!(percent < 100, "百分比 {percent} 不得越过 100");
    assert_eq!(context_tui_ctx_color(&pty), vt100::Color::Idx(3));
    context_tui_exchange(&mut pty, &fixture, "completion", &context_tui_filler("context-over-capacity", 70000), "over capacity observed");
    assert!(context_tui_ctx_percent(pty.parser.screen()) >= 100);
    assert_eq!(context_tui_ctx_color(&pty), vt100::Color::Idx(1));
    pty.input("\x03");
    pty.finish();
}

// 验证项 4（自动取消）：两个完整只读批次把 32768 的用量推过 80% 后，服务端
// 握住自动摘要 SSE；屏幕可见 Compacting，草稿不被压缩抢占。Esc 取消等待真实
// 任务终态（Cancelled Note + Ready），取消后不得发普通请求；以自动压缩关闭
// 的配置恢复同一会话，下一次普通请求完整携带已接受输入与两批 tool results。
#[test]
fn context_tui_auto_compact_cancel_keeps_draft_and_recovers_accepted_history() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (home, mut pty) = context_tui_start_auto(&fixture, protocol, 32768, 80);
        let first = "context-auto-cancel-first";
        let second = "context-auto-cancel-second";
        context_tui_write_batch(
            home.path(),
            "context-auto-cancel-one.txt",
            first,
            CONTEXT_TUI_AUTO_FIRST_BYTES,
        );
        context_tui_write_batch(
            home.path(),
            "context-auto-cancel-two.txt",
            second,
            CONTEXT_TUI_AUTO_SECOND_BYTES,
        );

        pty.input("context auto cancel prompt\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(body["tools"].as_array().is_some(), "首批批次前不得摘要");
        context_tui_read_calls(
            request,
            protocol,
            "context-auto-cancel-one",
            "context-auto-cancel-one.txt",
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(
            body["tools"].as_array().is_some(),
            "首个批次不得提前触发自动摘要"
        );
        assert!(serde_json::to_string(&body).unwrap().contains(first));
        context_tui_read_calls(
            request,
            protocol,
            "context-auto-cancel-two",
            "context-auto-cancel-two.txt",
        );

        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        context_tui_assert_summary_body(&body, protocol, first, second);
        let _held = SseReply::new(request);
        pty.text("Compacting context");
        pty.wait_for("自动压缩触发后的用量超过 80%", |screen| {
            context_tui_ctx_percent(screen) >= 80
        });

        pty.input("context-auto-draft");
        pty.draft("context-auto-draft");
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        // 取消后不发送普通请求，也不把摘要正文写入可见对话。
        no_more(&fixture);
        assert!(!pty.parser.screen().contents().contains(CONTEXT_TUI_SUMMARY));
        assert!(
            context_tui_page_to(&mut pty, "context auto cancel prompt"),
            "取消不得清除已接受输入：{}",
            pty.diagnostics()
        );
        context_tui_page_to_bottom(&mut pty);
        drop(_held);

        // 关闭自动压缩后恢复：此前已接受的输入与两个完整工具批次仍可用。
        let id = saved_session_id(home.path());
        pty.input("\x01\x0b");
        pty.draft("");
        pty.input("/quit\r");
        pty.finish();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            context_tui_live_settings(false, 80),
        )
        .unwrap();
        let mut resumed = Pty::start_with_args(home.path(), home.path(), &["-c", &id]);
        resumed.wait_for("auto-continue keeps input editable", input_is_ready);
        let (request, body) = resumed.request_at(&fixture, api_path(protocol));
        let text = serde_json::to_string(&body).unwrap();
        assert!(text.contains("context auto cancel prompt"));
        assert!(text.contains(first), "取消不得丢弃第一批 tool result");
        assert!(text.contains(second), "取消不得丢弃第二批 tool result");
        assert!(text.contains("context-auto-cancel-one"));
        assert!(text.contains("context-auto-cancel-two"));
        assert!(!text.contains("[Context summary]"));
        assert!(!text.contains(CONTEXT_TUI_INSTRUCTION));
        assert_eq!(
            cancel_user_messages(&body, protocol),
            ["context auto cancel prompt"]
        );
        context_tui_final(request, protocol, "context auto cancel recovered");
        resumed.text("context auto cancel recovered");
        resumed.ready();
        resumed.input("/quit\r");
        resumed.finish();
        no_more(&fixture);
    }
}

// 验证项 4（自动成功）：摘要请求先于普通任务请求，成功后 Compact Note 在
// 任务 assistant 之前；旧对话仍在 Transcript，用量下降，摘要正文不产生
// assistant 气泡；普通请求只含摘要包装与原生近期批次。
#[test]
fn context_tui_auto_compact_success_keeps_dialogue_drops_usage_without_summary_bubble() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (home, mut pty) = context_tui_start_auto(&fixture, protocol, 32768, 80);
        let old = "context-auto-success-old";
        let recent = "context-auto-success-recent";
        context_tui_write_batch(
            home.path(),
            "context-auto-success-old.txt",
            old,
            CONTEXT_TUI_AUTO_FIRST_BYTES,
        );
        context_tui_write_batch(
            home.path(),
            "context-auto-success-new.txt",
            recent,
            CONTEXT_TUI_AUTO_SECOND_BYTES,
        );

        pty.input("context auto success prompt\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(body["tools"].as_array().is_some());
        context_tui_read_calls(
            request,
            protocol,
            "context-auto-success-one",
            "context-auto-success-old.txt",
        );
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        assert!(
            body["tools"].as_array().is_some(),
            "首个批次不得提前触发自动摘要"
        );
        context_tui_read_calls(
            request,
            protocol,
            "context-auto-success-two",
            "context-auto-success-new.txt",
        );

        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        context_tui_assert_summary_body(&body, protocol, old, recent);
        pty.text("Compacting context");
        pty.wait_for("自动压缩触发后的用量超过 80%", |screen| {
            context_tui_ctx_percent(screen) >= 80
        });
        let before = context_tui_ctx_tokens(pty.parser.screen());
        context_tui_final(request, protocol, CONTEXT_TUI_SUMMARY);

        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        pty.text("Context compacted:");
        assert!(
            !pty.parser.screen().contents().contains("context auto compacted answer"),
            "Compact Note 必须在任务 assistant 之前出现"
        );
        let text = serde_json::to_string(&body).unwrap();
        assert_eq!(text.matches("[Context summary]").count(), 1);
        assert!(text.contains(CONTEXT_TUI_SUMMARY));
        assert!(text.contains(recent), "最近完整批次必须原生保留");
        assert!(!text.contains(old), "旧前缀原文不得复活");
        assert!(body["tools"].as_array().is_some());
        context_tui_final(request, protocol, "context auto compacted answer");

        pty.text("context auto compacted answer");
        pty.ready();
        let after = context_tui_ctx_tokens(pty.parser.screen());
        assert!(
            after < before,
            "自动压缩后估算必须下降：before {before} after {after}"
        );
        let visible = pty.parser.screen().contents();
        let note = visible
            .find("Context compacted:")
            .expect("自动压缩必须显示 Compact Note");
        let answer = visible.find("context auto compacted answer").unwrap();
        assert!(note < answer, "Compact Note 必须在任务 assistant 之前");
        assert!(!visible.contains(CONTEXT_TUI_SUMMARY));
        assert!(!visible.contains(CONTEXT_TUI_INSTRUCTION));
        assert!(
            context_tui_page_to(&mut pty, "context auto success prompt"),
            "自动压缩不得清除旧对话：{}",
            pty.diagnostics()
        );
        context_tui_page_to_bottom(&mut pty);
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

// ulimit -f 允许记录新输入，却拒绝完整压缩 state；保留 partial tail，
// 由真正的恢复路径忽略它，不能宣告成功或继续普通请求。
#[test]
fn context_tui_auto_compact_checkpoint_failure_leaves_previous_snapshot() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let first = "context-checkpoint-first";
        let second = "context-checkpoint-second";
        context_tui_write_batch(
            home.path(),
            "context-checkpoint-one.txt",
            first,
            CONTEXT_TUI_AUTO_FIRST_BYTES,
        );
        context_tui_write_batch(
            home.path(),
            "context-checkpoint-two.txt",
            second,
            CONTEXT_TUI_AUTO_SECOND_BYTES,
        );
        let (id, seeded_len) = context_tui_seed_large_session(home.path(), &fixture, protocol);
        let path = home
            .path()
            .join(".pp/agent/sessions")
            .join(format!("{id}.jsonl"));

        // 为工具清单及新输入审计预留 32KB；候选还包含完整 40KB 最新批次，仍会超限。
        let limit = (seeded_len + 32768).div_ceil(1024) * 1024;
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            context_tui_live_settings(true, 80),
        )
        .unwrap();
        let mut pty = Pty::start_with_file_limit(
            home.path(),
            home.path(),
            limit / 1024,
            &["-r", &id],
        );
        pty.ready();
        pty.input("context checkpoint resume prompt\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        context_tui_assert_summary_body(&body, protocol, first, second);
        // 已接受输入的原生追加与审计在摘要前完整写入。
        let sent_bytes = std::fs::read(&path).unwrap();
        assert!(
            sent_bytes.len() as u64 > seeded_len,
            "已接受输入必须在摘要请求前写入"
        );
        assert_eq!(sent_bytes.last(), Some(&b'\n'));
        assert!(
            String::from_utf8_lossy(&sent_bytes).contains("context checkpoint resume prompt"),
            "已接受输入必须在摘要前审计落盘"
        );
        context_tui_final(request, protocol, CONTEXT_TUI_SUMMARY);
        pty.wait_for("压缩提交失败显示 persisting session", |screen| {
            cancel_normalize(screen).contains("persistingsession")
        });
        assert!(
            !pty.parser.screen().contents().contains("Context compacted:"),
            "写失败不得宣告 CompactFinished"
        );
        // 失败后不继续普通任务请求。
        no_more(&fixture);
        pty.input("/quit\r");
        pty.finish();

        // 同步失败回滚候选，已接受的用户输入与旧 native 历史逐字节保留。
        let failed = std::fs::read(&path).unwrap();
        assert_eq!(failed, sent_bytes, "failed compaction must preserve the prior committed journal");
        let complete = String::from_utf8_lossy(&failed);
        assert!(
            !complete.contains("[Context summary]"),
            "失败候选不得成为已提交快照"
        );
        assert!(!complete.contains(CONTEXT_TUI_SUMMARY));

        // 不继续任务，只恢复：读到此前完整快照，不把失败候选当成功。
        let mut resumed = Pty::start_with_args(home.path(), home.path(), &["-r", &id]);
        resumed.ready();
        assert!(
            context_tui_page_to(&mut resumed, "context checkpoint seed prompt"),
            "恢复必须读到此前完整快照：{}",
            resumed.diagnostics()
        );
        assert!(!resumed.parser.screen().contents().contains("Context compacted:"));
        assert!(!resumed.parser.screen().contents().contains(CONTEXT_TUI_SUMMARY));
        assert!(
            context_tui_ctx_percent(resumed.parser.screen()) >= 80,
            "恢复的旧历史必须仍有高用量"
        );
        assert_eq!(std::fs::read(&path).unwrap(), failed, "read-only resume must not rewrite the incomplete tail");
        no_more(&fixture);
        resumed.input("\x03");
        resumed.finish();
        no_more(&fixture);
    }
}

// 验证项 4（token 指标，双协议）：流式生成期间在 Ctx 左侧显示不带前缀的非零估算
// 速度；服务端权威 usage 到达后更新平均速度，In/Out/CRead/CWrite 使用最近一次已完成
// 普通请求（input 仍含缓存命中总数，CRead/CWrite 只是其中的明细）；下一轮缺失 usage
// 时计数退回未知而估算速度冻结；/new 清空全部新指标。
#[test]
fn context_tui_token_metrics_live_then_authoritative_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 40000, 80);
        // 初始 100×30：Ctx 正常，新计数字段全部为未知而不是 0。
        pty.wait_for("初始状态栏显示未知 token 指标", context_tui_metrics_unknown);
        assert!(pty.parser.screen().contents().contains("Ctx 0/40.0k"));
        assert_eq!(
            context_tui_ctx_color(&pty),
            vt100::Color::Rgb(122, 162, 247)
        );

        pty.input("context-metrics-live\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = SseReply::new(request);
        let first = format!("context-metrics-live-one {}", "x".repeat(600));
        streaming_delta(&mut stream, protocol, &first);
        pty.text("context-metrics-live-one");
        // SSE 仍被服务端握住：速度只能是估算，且 In/Out/CRead/CWrite 尚无完成请求。
        pty.wait_for("流式生成中显示非零估算速度", context_tui_positive_speed);
        let live = pty.parser.screen().contents();
        assert!(live.contains("In —") && live.contains("Out —") && live.contains("CRead —"));
        let (speed_row, speed_column) = context_tui_locate(pty.parser.screen(), "Tok/s")
            .expect("流式生成中必须显示速率字段");
        let (ctx_row, ctx_column) =
            context_tui_locate(pty.parser.screen(), "Ctx ").expect("Ctx 必须可见");
        assert_eq!(speed_row, ctx_row, "指标必须与 Ctx 同在输入栏上沿");
        assert!(speed_column < ctx_column, "速度必须位于 Ctx 左侧");

        streaming_delta(&mut stream, protocol, " context-metrics-live-two");
        pty.text("context-metrics-live-two");
        assert!(
            context_tui_speed(pty.parser.screen()).is_some_and(|(_, marked)| !marked),
            "流式速度不得带 ~ 前缀"
        );

        context_tui_stream_usage(
            &mut stream,
            protocol,
            &format!("{first} context-metrics-live-two"),
        );
        stream.finish();
        pty.wait_for("权威 usage 更新完成态指标", |screen| {
            let contents = screen.contents();
            contents.contains("In 12.0k")
                && contents.contains("Out 300")
                && contents.contains("CRead 40")
                && contents.contains("CWrite 120")
        });
        pty.ready();
        let screen = pty.parser.screen();
        let (rate, marked) = context_tui_speed(screen).expect("完成后必须显示精确速度");
        assert!(!marked, "权威 usage 到达后不得保留 ~");
        assert!(rate > 0.0, "完成速度必须为正：{rate}");
        let (row, ctx_column) = context_tui_locate(screen, "Ctx ").expect("Ctx 必须可见");
        let mut previous = context_tui_locate(screen, "Tok/s")
            .expect("速率字段必须可见")
            .1;
        assert_eq!(
            screen.cell(row, previous).expect("速率数值").fgcolor(),
            vt100::Color::Rgb(224, 175, 104),
            "速率数值必须使用 speed 颜色"
        );
        for (label, offset, color) in [
            ("In 12.0k", 3, vt100::Color::Rgb(125, 207, 255)),
            ("Out 300", 4, vt100::Color::Rgb(158, 206, 106)),
            ("CRead 40", 6, vt100::Color::Rgb(187, 154, 247)),
            ("CWrite 120", 7, vt100::Color::Rgb(255, 158, 100)),
        ] {
            let (field_row, column) = context_tui_locate(screen, label)
                .unwrap_or_else(|| panic!("状态栏必须显示 {label}\n{}", screen.contents()));
            assert_eq!(field_row, row, "指标必须与 Ctx 同一行：{label}");
            assert!(
                column > previous,
                "字段顺序必须是 speed → In → Out → CRead → CWrite：{label}"
            );
            previous = column;
            assert_eq!(
                screen
                    .cell(field_row, column + offset)
                    .expect("字段数值")
                    .fgcolor(),
                color,
                "{label} 数值颜色"
            );
            if label.starts_with('C') {
                let (icon_row, icon_column) = context_tui_locate(screen, &format!("\u{f1c0} {label}"))
                    .expect("缓存字段必须包含自己的完整图标");
                assert_eq!(icon_row, row);
                assert_eq!(screen.cell(icon_row, icon_column).unwrap().fgcolor(), color);
            }
        }
        assert!(previous < ctx_column, "指标必须整体位于 Ctx 左侧");
        for (icon, color) in [
            ("\u{f0e7}", vt100::Color::Rgb(224, 175, 104)),
            ("\u{f019}", vt100::Color::Rgb(125, 207, 255)),
            ("\u{f093}", vt100::Color::Rgb(158, 206, 106)),
            ("\u{f1c0}", vt100::Color::Rgb(187, 154, 247)),
        ] {
            let (icon_row, icon_column) =
                context_tui_locate(screen, icon).unwrap_or_else(|| panic!("状态栏必须显示 {icon:?}"));
            assert_eq!(icon_row, row, "图标必须与 Ctx 同一行");
            assert_eq!(
                screen
                    .cell(icon_row, icon_column)
                    .expect("图标 cell")
                    .fgcolor(),
                color,
                "{icon:?} 颜色"
            );
        }
        assert_eq!(
            context_tui_ctx_color(&pty),
            vt100::Color::Rgb(122, 162, 247)
        );

        // 下一轮：成功但没有 usage。计数退回未知，流式估算不带前缀并冻结。
        // 加宽以容纳请求临时状态，同时验证上一轮读写计数保留。
        pty.resize(30, 140);
        pty.input("context-metrics-missing\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = SseReply::new(request);
        // 服务端暂不返回首个 delta：新请求等待期间必须保留上一笔完成速度。
        pty.wait_for("下一请求等待首字节时速度不闪回未知", |screen| {
            screen.contents().contains("Requesting model")
                && screen.contents().contains("CRead 40")
                && screen.contents().contains("CWrite 120")
                && context_tui_speed(screen)
                    .is_some_and(|(current, marked)| !marked && current == rate)
        });
        streaming_delta(&mut stream, protocol, "context-metrics-missing-answer");
        pty.text("context-metrics-missing-answer");
        pty.wait_for(
            "缺失 usage 的请求也显示估算速度",
            context_tui_positive_speed,
        );
        streaming_complete(&mut stream, protocol, "context-metrics-missing-answer");
        stream.finish();
        pty.wait_for("缺失 usage 后完成态计数退回未知", |screen| {
            let contents = screen.contents();
            contents.contains("In —")
                && contents.contains("Out —")
                && contents.contains("CRead —")
                && contents.contains("CWrite —")
        });
        let (rate, marked) =
            context_tui_speed(pty.parser.screen()).expect("流式字节必须保留最终估算速度");
        assert!(
            !marked && rate > 0.0,
            "缺失 usage 时速度也不带 ~ 前缀：{rate}"
        );

        pty.ready();
        pty.resize(30, 100);
        pty.input("/new\r");
        pty.wait_for("/new 清空全部新指标", |screen| {
            screen.contents().contains("Ctx 0/40.0k") && context_tui_metrics_unknown(screen)
        });
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

// 验证项 4（token 指标布局与取消）：流中 Esc 后速度停止，下一轮请求重新计数；
// 含中文草稿与光标在 60/40/20 列 resize 中保持不变，Ctx 优先保留而指标只能整块
// 降级、不留图标残片；成功压缩清空普通请求指标，摘要等待期间临时状态与既有
// 指标同处输入栏上沿且互不侵占。
#[test]
fn context_tui_token_metrics_layout_and_cancel_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 40000, 80);
        pty.wait_for("初始状态栏显示未知 token 指标", context_tui_metrics_unknown);

        // 流式生成中 Esc：速度计时停止并从状态栏清除。
        pty.input("context-metrics-cancel\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = SseReply::new(request);
        streaming_delta(&mut stream, protocol, "context-metrics-cancel-partial");
        pty.text("context-metrics-cancel-partial");
        pty.wait_for("取消前显示估算速度", context_tui_positive_speed);
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        let cancelled = pty.parser.screen().contents();
        assert!(
            cancelled.contains("— Tok/s"),
            "取消后速度必须清除：{cancelled}"
        );
        assert!(
            context_tui_speed(pty.parser.screen()).is_none(),
            "取消后不得保留任何速率：{cancelled}"
        );
        drop(stream);

        // 下一轮请求重新计数：新请求再次出现不带前缀的非零估算速度。
        pty.input("context-metrics-recount\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = SseReply::new(request);
        streaming_delta(&mut stream, protocol, "context-metrics-recount-partial");
        pty.text("context-metrics-recount-partial");
        pty.wait_for("新请求重新显示估算速度", context_tui_positive_speed);
        pty.input("\x1b");
        pty.wait_for("second task cancellation committed", |screen| {
            input_is_ready(screen)
                && screen.contents().contains("✻ Cancelled")
                && cancel_count_normalized(screen, CANCEL_NOTE) == 2
        });
        drop(stream);
        pty.wait_for("本轮取消清空速度而非上一轮取消提示", |screen| {
            screen.contents().contains("— Tok/s") && context_tui_speed(screen).is_none()
        });

        // 中文草稿与光标在窄屏 resize 中保持不变；Ctx 优先，指标整块降级。
        pty.input("草稿-指标");
        pty.draft("草稿-指标");
        let cursor_column = pty.parser.screen().cursor_position().1;
        for (rows, columns) in [(30, 60), (30, 40), (30, 20)] {
            pty.resize(rows, columns);
            pty.wait_for("resize 后上下文字段仍位于输入栏上沿", |screen| {
                let contents = screen.contents();
                let lines: Vec<&str> = contents.lines().collect();
                lines.windows(2).any(|pair| {
                    pair[0].contains(" Ctx ")
                        && pair[0].trim_end().ends_with("/40.0k")
                        && pair[1].starts_with('❯')
                })
            });
            pty.draft("草稿-指标");
            let screen = pty.parser.screen();
            assert!(
                !screen.contents().contains('\u{fffd}'),
                "{columns} 列出现损坏字形\n{}",
                pty.diagnostics()
            );
            for (icon, text) in [
                ("\u{f0e7}", "Tok/s"),
                ("\u{f019}", "In —"),
                ("\u{f093}", "Out —"),
            ] {
                context_tui_assert_metric_whole(screen, icon, text);
            }
            // 缓存读写共用图标，逐字段验证前缀，并检查没有多余图标残片。
            let contents = screen.contents();
            let mut cache_fields = 0;
            for label in ["CRead —", "CWrite —"] {
                let visible = context_tui_locate(screen, label).is_some();
                assert_eq!(
                    context_tui_locate(screen, &format!("\u{f1c0} {label}")).is_some(),
                    visible,
                    "缓存字段必须整块显示：{label}\n{contents}"
                );
                cache_fields += usize::from(visible);
            }
            assert_eq!(contents.matches('\u{f1c0}').count(), cache_fields);
            if columns == 20 {
                // Ctx 之后只剩不足一个完整图标的宽度：指标整块隐藏，不切开 glyph。
                for icon in ["\u{f0e7}", "\u{f019}", "\u{f093}", "\u{f1c0}"] {
                    assert!(
                        context_tui_locate(screen, icon).is_none(),
                        "{columns} 列必须整块隐藏指标\n{}",
                        pty.diagnostics()
                    );
                }
            }
            assert_eq!(
                pty.parser.screen().cursor_position().1,
                cursor_column,
                "{columns} 列 resize 不得移动草稿光标"
            );
        }
        pty.resize(30, 100);
        pty.draft("草稿-指标");
        pty.input("\x01\x0b");
        pty.draft("");

        // 三个完整交互使手动压缩有可移除的旧前缀，最后一个带权威 usage。
        context_tui_exchange(
            &mut pty,
            &fixture,
            protocol,
            &context_tui_filler("context-metrics-first", 16000),
            "context-metrics-first-answer",
        );
        context_tui_exchange(
            &mut pty,
            &fixture,
            protocol,
            &context_tui_filler("context-metrics-second", 16000),
            "context-metrics-second-answer",
        );
        pty.input("context-metrics-authoritative\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = SseReply::new(request);
        streaming_delta(&mut stream, protocol, "context-metrics-authoritative-answer");
        context_tui_stream_usage(&mut stream, protocol, "context-metrics-authoritative-answer");
        stream.finish();
        pty.wait_for("普通请求完成并显示权威计数", |screen| {
            screen.contents().contains("In 12.0k")
                && screen.contents().contains("Out 300")
                && screen.contents().contains("CRead 40")
                && screen.contents().contains("CWrite 120")
        });
        pty.ready();

        // 已有权威计数时取消下一轮：最近成功的缓存读写量继续保留。
        pty.resize(30, 140);
        pty.input("context-metrics-retain-on-cancel\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut cancelled_stream = SseReply::new(request);
        streaming_delta(&mut cancelled_stream, protocol, "context-metrics-retain-partial");
        pty.text("context-metrics-retain-partial");
        pty.input("\x1b");
        pty.wait_for("取消下一轮后保留最近成功的缓存读写量", |screen| {
            let contents = screen.contents();
            input_is_ready(screen)
                && contents.contains("context-metrics-retain-partial")
                && contents.contains("✻ Cancelled")
                && contents.contains("— Tok/s")
                && contents.contains("CRead 40")
                && contents.contains("CWrite 120")
        });
        drop(cancelled_stream);
        pty.resize(30, 100);

        // 摘要被服务端握住：临时状态、既有指标与 Ctx 仍能在 100 列内共存。
        pty.input("/compact\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        context_tui_assert_summary_body(
            &body,
            protocol,
            "context-metrics-first",
            "context-metrics-second",
        );
        let mut stream = SseReply::new(request);
        pty.text("Compacting context...");
        pty.wait_for("压缩等待期间保留既有指标与临时状态", |screen| {
            let contents = screen.contents();
            contents.contains("Compacting context...")
                && contents.contains("In 12.0k")
                && contents.contains("Out 300")
        });
        assert!(!pty.parser.screen().contents().contains("CWrite"));
        // 加宽后读写量仍在，证明临时状态仅触发布局降级，没有清除计数。
        pty.resize(30, 140);
        pty.wait_for("宽屏压缩等待期间保留缓存读写量", |screen| {
            let contents = screen.contents();
            contents.contains("Compacting context...")
                && contents.contains("CRead 40")
                && contents.contains("CWrite 120")
        });
        let screen = pty.parser.screen();
        let (row, extra_column) = context_tui_locate(screen, "Compacting context...")
            .expect("压缩等待时必须显示临时状态");
        let (ctx_row, ctx_column) =
            context_tui_locate(screen, "Ctx ").expect("Ctx 必须可见");
        let (metrics_row, metrics_column) =
            context_tui_locate(screen, "Tok/s").expect("已有完成速率必须可见");
        assert_eq!(metrics_row, row, "指标必须与 Ctx 同一行");
        assert!(
            extra_column < metrics_column,
            "临时状态必须位于指标左侧：{extra_column} / {metrics_column}"
        );
        assert_eq!(ctx_row, row);
        assert!(metrics_column < ctx_column, "Ctx 必须仍在最右端");

        context_tui_summary_reply(&mut stream, protocol);
        stream.finish();
        pty.text("Context compacted:");
        pty.ready();
        pty.resize(30, 100);
        pty.wait_for("成功压缩后普通请求指标清空", context_tui_metrics_unknown);
        assert!(context_tui_locate(pty.parser.screen(), "Ctx ").is_some());
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}
