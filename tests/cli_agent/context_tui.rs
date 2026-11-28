// TUI context compaction lifecycle (plan verification item 4): a real 100-column PTY drives — the Ctx
// field visible in the status line; the Compacting state while the server holds the summary SSE for a
// manual or automatic compaction; the draft, accepted input and tool results kept after an Esc cancel
// without continuing ordinary requests; the old conversation kept after a successful compaction while
// usage drops and Compact Note precedes the task assistant; the next ordinary request carrying only the
// summary wrapper plus the native recent suffix; the Ctx field taking priority on a narrow resize and
// the Ctrl-C terminal flags restored; the Ctx field staying white at the configured threshold and
// above capacity; and, under ulimit -f, compaction state writes leaving a partial tail while the
// previously complete snapshot is still read back. It also covers the live token metrics at the top
// edge of the input area: the unprefixed estimated speed mid-stream, the average speed after
// authoritative usage with In/Out/CRead/CWrite coloring, missing usage and the /new reset, the rate
// stopping after cancellation, and Ctx priority with the metrics block degrading at 60/40/20 columns.
// Helpers and test names all start with context_; no use declaration is added, the same module's
// private Pty and the existing HTTP fixture are reused directly.

const CONTEXT_TUI_SUMMARY: &str = "SUMMARY: kept constraint";
const CONTEXT_TUI_INSTRUCTION: &str = "Summarize the preceding conversation";

fn context_tui_settings(auto_compact: bool, threshold_percent: u8) -> String {
    format!(
        "{{\"default_model\":\"fixture/primary\",\"auto_compact\":{auto_compact},\
\"auto_compact_threshold_percent\":{threshold_percent},\
\"notification_timeout_seconds\":2}}"
    )
}

// The first start still imports the old four fields into agent.db through migration; after migration
// settings.yml keeps only non-preference fields, and later restarts that rewrite the policy must not
// write default_model again.
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

// Dual config for a standalone HOME: automatic compaction off by default, 2-second notifications.
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

// Bracketed paste delivers a large block at once, avoiding per-key events.
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

// One exchange: paste the prompt, answer the request, wait for the answer and the idle input box.
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

// Read usage from the status line percentage and capacity, allowing for the interface's single-decimal
// rounding.
fn context_tui_ctx_tokens(screen: &vt100::Screen) -> f64 {
    let contents = screen.contents();
    let rest = contents.split_once("\u{e62a} ").expect("context field").1;
    let (percent, capacity) = rest.split_once("%/").expect("percentage and capacity");
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
        .expect("context field")
        .1
        .split_once('%')
        .unwrap()
        .0
        .parse::<f64>()
        .unwrap() as u128
}

// Locate text in real screen cells, returning (row, start column). The column must come from a cell
// scan: the display width of Nerd icons and wide characters equals neither the UTF-8 byte count nor
// chars().count().
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
    let (row, column) = context_tui_locate(screen, "\u{e62a} ").expect("missing context field");
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

// The speed field's value and whether it is prefixed: the field itself is formatted as `N.N[k|m]
// tok/s`, and only `— tok/s` when unknown.
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

// A non-zero speed must be shown as a bare number, without the ~ prefix.
fn context_tui_positive_speed(screen: &vt100::Screen) -> bool {
    context_tui_speed(screen).is_some_and(|(rate, marked)| !marked && rate > 0.0)
}

// Authoritative tail of a streaming request: Chat uses a usage-only tail chunk + DONE, Responses uses a
// completed event carrying the same usage; the text matches the deltas already sent so the rate can be
// calibrated against the server's output.
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

// Reply to the summary request: it terminates like an ordinary stream, but compaction streams do not
// feed the metrics (isolated by an empty callback).
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

// Shape of the summary request: no tool configuration, an output cap, and only the old prefix plus one
// fixed summary instruction; the recent suffix is never sent to the summary model.
fn context_tui_assert_summary_body(
    body: &Value,
    protocol: &str,
    old_marker: &str,
    recent_marker: &str,
) {
    let text = serde_json::to_string(body).unwrap();
    assert!(text.contains(CONTEXT_TUI_INSTRUCTION));
    assert!(text.contains(old_marker), "the old prefix must take part in the summary");
    assert!(!text.contains(recent_marker), "the recent suffix must not be sent to the summary model");
    assert!(
        body["tools"].as_array().is_none() || body["tool_choice"] == "none",
        "summary requests must disable tool calls (no tools or tool_choice=none)"
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

// Three 8KB exchanges push the old prefix past the 40000/5-token retention budget, so the summary
// covers only the first exchange and the most recent exchange is always kept natively.
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

// The first 35KB batch stays below the 80% threshold of 32768; once the second 40KB batch completes
// the full history crosses the threshold, but the kept recent batch plus system/tools is still below it,
// so the next ordinary request must be preceded by an automatic summary and the candidate must be
// accepted.
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

// Page up until the old conversation marker is visible; return false honestly on failure, and the
// caller can still page back to the bottom.
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
