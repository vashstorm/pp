// TUI 上下文压缩生命周期（计划验证项 4）：真实 100 列 PTY 驱动 —— 状态栏
// Ctx 可见、手动或自动压缩被服务端握住摘要 SSE 时的 Compacting 状态、Esc
// 取消后草稿与已接受输入/工具结果保留且不继续普通请求、成功压缩后旧对话
// 保留而用量下降且 Compact Note 先于任务 assistant、下一个普通请求只含
// 摘要包装与原生近期后缀、窄屏 resize 的上下文字段优先与 Ctrl-C 终端标志
// 恢复、达到配置阈值和超过容量时上下文字段仍为白色，以及 ulimit -f 下压缩 state 写入
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

fn context_tui_models(server: &TestServer, protocol: &str, window: u64) -> String {
    json!({
        "providers": {"fixture": {
            "protocol": protocol,
            "api_key": "fixture-key",
            "base_url": format!("http://{}/v1", server.addr()),
            "models": {"primary": { "model": "fixture-model",
                        "context_window": window}}
        }}
    })
    .to_string()
}

// 独立 HOME 的双配置：默认自动压缩关闭、通知 2 秒。
fn context_tui_start_with_policy(
    server: &TestServer,
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
    server: &TestServer,
    protocol: &str,
    window: u64,
    threshold_percent: u8,
) -> (tempfile::TempDir, Pty) {
    context_tui_start_with_policy(server, protocol, window, threshold_percent, false)
}

fn context_tui_start_auto(
    server: &TestServer,
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
        writeln!(
            text,
            "{marker}-{line:04} xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"
        )
        .unwrap();
        line += 1;
    }
    text.truncate(bytes);
    text
}

// 一轮交互：粘贴提示词、回答请求、等待回答与空闲输入框。
fn context_tui_exchange(
    pty: &mut Pty,
    server: &TestServer,
    protocol: &str,
    prompt: &str,
    answer: &str,
) {
    context_tui_paste(pty, prompt);
    pty.input("\r");
    let (request, _) = pty.request_at(server, api_path(protocol));
    let answer = format!("{answer} {}", prompt.split_whitespace().next().unwrap());
    if protocol == "completion" {
        final_reply(request, &answer);
    } else {
        response_final(request, &answer);
    }
    pty.completed_after(&answer);
}

// 从状态栏的百分比和容量读取用量，允许界面的一位小数舍入。
fn context_tui_ctx_tokens(screen: &vt100::Screen) -> f64 {
    let contents = screen.contents();
    let rest = contents.split_once("\u{e62a} ").expect("上下文字段").1;
    let (percent, capacity) = rest.split_once("%/").expect("百分比与容量");
    let number: String = capacity
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    let scale = match capacity[number.len()..].chars().next() {
        Some('K') => 1000.0,
        Some('M') => 1_000_000.0,
        _ => 1.0,
    };
    percent.parse::<f64>().unwrap() * number.parse::<f64>().unwrap() * scale / 100.0
}

fn context_tui_ctx_percent(screen: &vt100::Screen) -> u128 {
    screen
        .contents()
        .split_once("\u{e62a} ")
        .expect("上下文字段")
        .1
        .split_once('%')
        .unwrap()
        .0
        .parse::<f64>()
        .unwrap() as u128
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

fn context_tui_assert_ctx_white(pty: &Pty) {
    let screen = pty.parser.screen();
    let (row, column) = context_tui_locate(screen, "\u{e62a} ").expect("缺少上下文字段");
    for column in [column, column + 2] {
        assert_eq!(screen.cell(row, column).unwrap().fgcolor(), vt100::Color::Idx(15));
    }
}

fn context_tui_metrics_hidden(screen: &vt100::Screen) -> bool {
    let contents = screen.contents();
    !contents.contains("\u{e62a} ")
        && !contents.contains("tok/s")
        && !contents.contains("\u{f04e2}")
        && !contents.contains("\u{f01bc}")
}

// 速度字段的数值与是否带前缀：字段自身格式为 `N.N[k|m] tok/s`，未知时只有 `— tok/s`。
fn context_tui_speed(screen: &vt100::Screen) -> Option<(f64, bool)> {
    let (row, column) = context_tui_locate(screen, "tok/s")?;
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
    let (head, multiplier) = if let Some(head) = head.strip_suffix('K') {
        (head, 1000.0)
    } else if let Some(head) = head.strip_suffix('M') {
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
fn context_tui_stream_usage(stream: &mut SseWriter, protocol: &str, text: &str) {
    if protocol == "completion" {
        stream.event(&chat_chunk(json!({}), Some("stop"))).unwrap();
        stream.event(&json!({
            "id":"chatcmpl-fixture", "object":"chat.completion.chunk",
            "created":1, "model":"fixture-model", "choices":[],
            "usage":{"prompt_tokens":12000, "completion_tokens":300,
                     "total_tokens":12300,
                     "prompt_tokens_details":{"cached_tokens":40,"cache_write_tokens":120}}
        })).unwrap();
        stream.frame("[DONE]").unwrap();
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
        stream.event(&json!({
            "type":"response.completed", "sequence_number":100, "response":response
        })).unwrap();
    }
}

// 摘要请求的回复：与普通流一样收尾，但压缩流不进入指标（由空回调隔离）。
fn context_tui_summary_reply(stream: &mut SseWriter, protocol: &str) {
    if protocol == "completion" {
        stream.event(&chat_chunk(
            json!({"role":"assistant","content":CONTEXT_TUI_SUMMARY}),
            None,
        )).unwrap();
        stream.event(&chat_chunk(json!({}), Some("stop"))).unwrap();
        stream.frame("[DONE]").unwrap();
    } else {
        stream.event(&json!({
            "type":"response.completed", "sequence_number":100,
            "response": response_body(json!([response_message(
                "msg-summary",
                json!([response_text(CONTEXT_TUI_SUMMARY)])
            )]))
        })).unwrap();
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
        body["tools"].as_array().is_none() || body["tool_choice"] == "none",
        "摘要请求必须禁止工具调用（无 tools 或 tool_choice=none）"
    );
    if protocol == "completion" {
        assert_eq!(body["max_completion_tokens"], json!(4096));
        let instruction = body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(instruction["role"], "user");
        assert!(
            instruction["content"]
                .as_str()
                .unwrap()
                .contains(CONTEXT_TUI_INSTRUCTION)
        );
    } else {
        assert_eq!(body["max_output_tokens"], json!(4096));
        let instruction = body["input"].as_array().unwrap().last().unwrap();
        assert_eq!(instruction["role"], "user");
        assert!(slash_message_text(instruction).contains(CONTEXT_TUI_INSTRUCTION));
    }
}

// 三个 8KB 交互使旧前缀超过 40000/5 token 的保留预算，
// 摘要只覆盖第一个交互，最近交互始终原生保留。
fn context_tui_fill_three_exchanges(pty: &mut Pty, server: &TestServer, protocol: &str) {
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

fn context_tui_read_calls(request: CapturedRequest, protocol: &str, id: &str, path: &str) {
    let args = json!({"path": path});
    if protocol == "completion" {
        calls_reply(request, json!([call(id, "read", args)]));
    } else {
        response_reply(request, json!([response_call(id, "read", args)]));
    }
}

fn context_tui_final(request: CapturedRequest, protocol: &str, text: &str) {
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


include!("context_tui/compact_cases.rs");
include!("context_tui/metrics_cases.rs");
