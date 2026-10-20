// TUI 上下文压缩生命周期（计划验证项 4）：真实 100 列 PTY 驱动 —— 状态栏
// ctx 可见、手动或自动压缩被服务端握住摘要 SSE 时的 Compacting 状态、Esc
// 取消后草稿与已接受输入/工具结果保留且不继续普通请求、成功压缩后旧对话
// 保留而用量下降且 Compact Note 先于任务 assistant、下一个普通请求只含
// 摘要包装与原生近期后缀、窄屏 resize 的上下文字段优先与 Ctrl-C 终端标志
// 恢复、达到配置阈值时上下文字段变黄，以及 ulimit -f 下压缩 state 写入
// 留下 partial tail 后恢复读取此前完整快照。辅助项与测试名均以 context_
// 开头；不新增 use 声明，直接复用同模块私有 Pty 与既有 HTTP fixture。

const CONTEXT_TUI_SUMMARY: &str = "SUMMARY: kept constraint";
const CONTEXT_TUI_INSTRUCTION: &str = "Summarize the preceding conversation";

fn context_tui_settings(auto_compact: bool, threshold_percent: u8) -> String {
    format!(
        "{{\"default_model\":\"fixture/primary\",\"auto_compact\":{auto_compact},\
\"auto_compact_threshold_percent\":{threshold_percent},\
\"notification_timeout_seconds\":2}}"
    )
}

fn context_tui_models(server: &Server, protocol: &str, window: u64) -> String {
    json!({
        "providers": [{
            "id": "fixture",
            "protocol": protocol,
            "api_key": "fixture-key",
            "base_url": format!("http://{}/v1", server.server_addr()),
            "models": [{"id": "primary", "model": "fixture-model",
                        "context_window": window}]
        }]
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
        config_path(home.path(), "settings.jsonc"),
        context_tui_settings(auto_compact, threshold_percent),
    )
    .unwrap();
    std::fs::write(
        config_path(home.path(), "models.jsonc"),
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
    // Model candidates only become visible after the worker's terminal event.
    pty.input("/model ");
    let answer = format!("{answer} {}", prompt.split_whitespace().next().unwrap());
    if protocol == "completion" {
        final_reply(request, &answer);
    } else {
        response_final(request, &answer);
    }
    pty.text(&answer);
    pty.text("fixture/primary");
    pty.input("\x01\x0b");
    pty.draft("");
}

// 从输入栏上沿的 `ctx N/W` 读取简写后的估算 token 数。
fn context_tui_ctx_tokens(screen: &vt100::Screen) -> f64 {
    let contents = screen.contents();
    let start = contents.find("ctx ").expect("状态栏必须显示 ctx 字段");
    let rest = &contents[start + "ctx ".len()..];
    let figure: String = rest
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    let value: f64 = figure.parse().expect("ctx 数字必须可解析");
    if rest[figure.len()..].starts_with('k') {
        value * 1000.0
    } else {
        value
    }
}

// 从当前用量与容量计算百分比，界面本身只显示大小。
fn context_tui_ctx_percent(screen: &vt100::Screen) -> u128 {
    let contents = screen.contents();
    let start = contents.find("ctx ").expect("状态栏必须显示 ctx 字段");
    let rest = &contents[start..];
    let (_, capacity) = rest.split_once('/').expect("已知容量必须显示总容量");
    let figure: String = capacity
        .chars()
        .take_while(|ch| ch.is_ascii_digit() || *ch == '.')
        .collect();
    let mut window: f64 = figure.parse().expect("ctx 容量必须可解析");
    if capacity[figure.len()..].starts_with('k') {
        window *= 1000.0;
    }
    (context_tui_ctx_tokens(screen) * 100.0 / window) as u128
}

// 上下文字段首字符的前景色：达到阈值时为终端调色板 3 号（黄）。
fn context_tui_ctx_color(pty: &Pty) -> vt100::Color {
    let screen = pty.parser.screen();
    let contents = screen.contents();
    let row = contents
        .lines()
        .position(|line| line.contains("ctx "))
        .expect("状态栏必须显示 ctx 字段") as u16;
    let line = contents
        .lines()
        .nth(row as usize)
        .unwrap();
    let start = line
        .find("ctx")
        .expect("ctx 字段必须有前缀位置");
    let column = line[..start].chars().count() as u16;
    screen.cell(row, column).expect("ctx 首字符").fgcolor()
}

#[test]
fn context_tui_shows_current_and_window_on_input_top_right() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 40000, 80);
        let contents = pty.parser.screen().contents();
        let lines: Vec<_> = contents.lines().collect();
        assert!(lines[26].contains(" ctx "));
        assert!(lines[26].trim_end().ends_with("/40k"));
        assert_eq!(lines[26].trim_end().chars().count(), 99);
        assert!(!lines[26].contains('%'));
        assert!(!lines[26].contains('~'));
        assert!(!lines[26].contains('.'));
        assert_eq!(context_tui_ctx_color(&pty), vt100::Color::Rgb(122, 162, 247));
        assert!(!lines[29].contains("ctx "));
        pty.input("context-position-draft");
        pty.draft("context-position-draft");
        pty.resize(12, 40);
        pty.wait_for("缩放后用量仍在输入栏上沿右端", |screen| {
            screen.contents().lines().nth(8).is_some_and(|line| {
                line.contains(" ctx ") && line.trim_end().ends_with("/40k")
            })
        });
        pty.draft("context-position-draft");
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
        config_path(home, "settings.jsonc"),
        context_tui_settings(false, 80),
    )
    .unwrap();
    std::fs::write(
        config_path(home, "models.jsonc"),
        context_tui_models(server, protocol, 32768),
    )
    .unwrap();
    let mut settings: Value = serde_json::from_str(&context_tui_settings(false, 80)).unwrap();
    settings["max_model_requests"] = json!(2);
    let cli = super::streaming::LiveCli::new(Cli::start_with(
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
    // End at the complete second tool batch, not a later plain assistant
    // exchange that would let retention discard both large batches.
    let output = cli.finish();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("2 model request limit reached"));
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
        pty.text("ctx ");
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
        pty.text("Context compacted:");
        pty.text("fixture/primary");
        pty.input("\x01\x0b");
        pty.draft("");
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
        pty.resize(30, 60);
        assert!(
            pty.parser.screen().contents().lines().nth(26).unwrap().contains("ctx "),
            "{}",
            pty.diagnostics()
        );
        pty.resize(30, 30);
        assert!(
            pty.parser.screen().contents().lines().nth(26).unwrap().contains("ctx "),
            "{}",
            pty.diagnostics()
        );
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
            config_path(home.path(), "settings.jsonc"),
            context_tui_settings(false, 80),
        )
        .unwrap();
        let mut resumed = Pty::start_with_args(home.path(), home.path(), &["-c", &id]);
        resumed.ready();
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

        // 新输入与审计可写完；候选含完整 40KB 最新批次，超过 16KB 余量。
        let limit = (seeded_len + 16384).div_ceil(1024) * 1024;
        std::fs::write(
            config_path(home.path(), "settings.jsonc"),
            context_tui_settings(true, 80),
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

        // 失败的 state 以 EFBIG 结束在文件大小上限，尾行不是完整 JSON。
        let failed = std::fs::read(&path).unwrap();
        assert_eq!(
            failed.len() as u64,
            limit,
            "rejected state write must stop at the file-size limit"
        );
        assert_ne!(failed.last(), Some(&b'\n'), "expected a partial final record");
        let cut = failed
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|index| index + 1)
            .expect("previous complete checkpoint line");
        assert!(cut < failed.len());
        let complete = String::from_utf8_lossy(&failed[..cut]);
        assert!(
            !complete.contains("[Context summary]"),
            "失败候选不得成为已提交快照"
        );
        assert!(!complete.contains(CONTEXT_TUI_SUMMARY));

        // 不继续任务，只恢复：读到此前完整快照，不把 partial tail 当成功。
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
