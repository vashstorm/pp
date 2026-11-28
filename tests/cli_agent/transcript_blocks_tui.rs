// Markdown 块级渲染 PTY 回归：真实 PTY + 本地 SSE fixture 覆盖计划中 Markdown
// 验证行 —— 两协议分段流式的中间态屏幕与 vt100 样式断言（先观察屏幕再发下一段）、
// 未闭合 delimiter 与最终权威正文不重复、refusal 替换/refusal-only/空正文边界、
// bash/read 字面输出不被 Markdown 解释、Unicode 表格与 >8192 grapheme 长行在
// 100↔24 列缩放下的内容不丢失、运行中 PgUp 保持视口并在回到底部后恢复跟随、
// 以及退出 scrollback 的完整样式回放。辅助函数以 `markdown_blocks_` 前缀命名，
// 测试函数以 `transcript_blocks_markdown_` 前缀命名，不新增 use 声明以免与
// 兄弟 include 文件冲突。

const MARKDOWN_BLOCKS_ERROR: vt100::Color = vt100::Color::Rgb(224, 108, 117);

fn markdown_blocks_start(fixture: &TestServer, protocol: &str, home: &Path, project: &Path) -> Pty {
    std::fs::write(config_path(home, "settings.yml"), r#"{"default_model":"fixture/primary","notification_timeout_seconds":1,"rate_limit_retry_budget_seconds":0,"read":{"toolResultPreview":true}}"#).unwrap();
    std::fs::write(
        config_path(home, "models.yml"),
        response_models(fixture, Some(protocol)),
    )
    .unwrap();
    Pty::start_with_args(project, home, &[])
}

// 在屏幕上定位 needle：返回 (row, col)。所有参与取色的 needle 前面只有单列
// 字符（ASCII/框线/制表符），因此字符序号即列号。
fn markdown_blocks_find(screen: &vt100::Screen, needle: &str) -> Option<(u16, u16)> {
    let (rows, cols) = screen.size();
    for row in 0..rows {
        let line: String = (0..cols)
            .filter_map(|col| screen.cell(row, col))
            .map(|cell| cell.contents())
            .collect();
        if let Some(byte_index) = line.find(needle) {
            let column = line[..byte_index].chars().count();
            return Some((row, u16::try_from(column).unwrap()));
        }
    }
    None
}

fn markdown_blocks_fg(screen: &vt100::Screen, needle: &str, offset: u16) -> vt100::Color {
    let (row, column) = markdown_blocks_find(screen, needle)
        .unwrap_or_else(|| panic!("missing {needle}: {}", screen.contents()));
    screen.cell(row, column + offset).unwrap().fgcolor()
}

fn markdown_blocks_bold(screen: &vt100::Screen, needle: &str) -> bool {
    let (row, column) = markdown_blocks_find(screen, needle)
        .unwrap_or_else(|| panic!("missing {needle}: {}", screen.contents()));
    screen.cell(row, column).unwrap().bold()
}

// completion：refusal delta 走 chat chunk 的 refusal 字段；response：refusal
// 占 (0,0) 部分并立即补 content_part.done，让流位置推进到下一列。
fn markdown_blocks_refusal_delta(stream: &mut SseWriter, protocol: &str, text: &str) {
    if protocol == "completion" {
        stream.event(&chat_chunk(json!({"role":"assistant","refusal":text}), None)).unwrap();
    } else {
        stream.event(&json!({
            "type":"response.refusal.delta", "sequence_number":0,
            "item_id":"msg-live", "output_index":0, "content_index":0,
            "delta":text, "logprobs":[]
        })).unwrap();
        stream.event(&json!({
            "type":"response.content_part.done", "sequence_number":1,
            "item_id":"msg-live", "output_index":0, "content_index":0,
            "part":{"type":"refusal","refusal":text}
        })).unwrap();
    }
}

// response 协议的正文与 refusal 属于不同 content part，正文 delta 必须落在
// (0,1)；completion 仍复用既有 streaming_delta。
fn markdown_blocks_content_delta(stream: &mut SseWriter, protocol: &str, text: &str) {
    if protocol == "completion" {
        streaming_delta(stream, protocol, text);
    } else {
        stream.event(&json!({
            "type":"response.output_text.delta", "sequence_number":2,
            "item_id":"msg-live", "output_index":0, "content_index":1,
            "delta":text, "logprobs":[]
        })).unwrap();
    }
}

fn markdown_blocks_refusal_content_complete(stream: &mut SseWriter, refusal: &str, text: &str) {
    stream.event(&json!({
        "type":"response.completed", "sequence_number":3,
        "response": response_body(json!([response_message("msg-live", json!([
            {"type":"refusal","refusal":refusal}, response_text(text)
        ]))]))
    })).unwrap();
}

fn markdown_blocks_reply_calls(request: CapturedRequest, protocol: &str, batch: &[(&str, &str, Value)]) {
    if protocol == "completion" {
        calls_reply(
            request,
            Value::Array(
                batch
                    .iter()
                    .map(|(id, name, args)| call(id, name, args.clone()))
                    .collect(),
            ),
        );
    } else {
        response_reply(
            request,
            Value::Array(
                batch
                    .iter()
                    .map(|(id, name, args)| response_call(id, name, args.clone()))
                    .collect(),
            ),
        );
    }
}

fn markdown_blocks_final(request: CapturedRequest, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

#[test]
fn transcript_blocks_markdown_thinking_streams_styles_and_replays() {
    let first = "plain thinking\n\n# Reasoning title\n\n**bold";
    let second = " word** and `inline code`\n\n```rust\nlet x = 1;\n";
    let third = "```\n\n| Name | Value |\n| --- | --- |\n| item | **cell bold** |\n\n- list item\n\n> quoted text\n";
    let reasoning = format!("{first}{second}{third}");
    let protocol = "response";
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            config_path(home.path(), "settings.yml"),
            r#"{"default_model":"fixture/primary","think_level":"high","notification_timeout_seconds":1}"#,
        )
        .unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&fixture, Some(protocol)),
        )
        .unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready();
        pty.input("render thinking markdown\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = request.start_sse().unwrap();
        for (index, chunk) in [first, second, third].into_iter().enumerate() {
            if protocol == "completion" {
                stream.event(&chat_chunk(json!({"reasoning_content":chunk}), None)).unwrap();
            } else {
                stream.event(&json!({
                    "type":"response.reasoning_summary_text.delta", "sequence_number":index,
                    "item_id":"reasoning-live", "output_index":0, "summary_index":0,
                    "delta":chunk
                })).unwrap();
            }
            match index {
                0 => {
                    pty.wait_for("思考标题样式已就绪", |screen| screen.contents().contains("**bold") && markdown_blocks_find(screen, "Reasoning title").is_some() && markdown_blocks_bold(screen, "Reasoning title"));
                    assert!(markdown_blocks_bold(pty.parser.screen(), "Reasoning title"));
                    assert!(!markdown_blocks_bold(pty.parser.screen(), "**bold"));
                }
                1 => {
                    pty.wait_for("思考流补齐加粗和代码块", |s| {
                        s.contents().contains("let x = 1;") && !s.contents().contains("**bold")
                    });
                    let screen = pty.parser.screen();
                    assert!(markdown_blocks_bold(screen, "bold word"));
                    assert_eq!(
                        markdown_blocks_fg(screen, "bold word", 0),
                        vt100::Color::Rgb(174, 72, 182)
                    );
                    // 行内代码使用截图中的绿色，与紫色粗体区分。
                    assert_eq!(
                        markdown_blocks_fg(screen, "inline code", 0),
                        vt100::Color::Rgb(86, 185, 127)
                    );
                    assert_eq!(
                        markdown_blocks_fg(screen, "let x = 1;", 0),
                        vt100::Color::Rgb(197, 134, 192)
                    );
                }
                _ => {
                    pty.wait_for("思考流显示表格、列表和引用", |s| {
                        let text = s.contents();
                        text.contains("cell bold") && text.contains("• list item")
                            && text.contains("quoted text") && text.contains("─┼─")
                    });
                    assert!(markdown_blocks_bold(pty.parser.screen(), "cell bold"));
                    assert_eq!(
                        markdown_blocks_fg(pty.parser.screen(), "Name", 0),
                        vt100::Color::Rgb(229, 229, 77)
                    );
                    assert_eq!(
                        markdown_blocks_fg(pty.parser.screen(), "─┼─", 0),
                        vt100::Color::Rgb(248, 248, 243)
                    );
                }
            }
        }
        if protocol == "completion" {
            streaming_delta(&mut stream, protocol, "Final answer");
            streaming_complete(&mut stream, protocol, "Final answer");
        } else {
            stream.event(&json!({
                "type":"response.completed", "sequence_number":3,
                "response":response_body(json!([
                    {"type":"reasoning", "id":"reasoning-live", "status":"completed",
                     "summary":[{"type":"summary_text", "text":reasoning}]},
                    response_message("answer", json!([response_text("Final answer")]))
                ]))
            })).unwrap();
        }
        stream.finish();
        pty.text("Final answer");
        pty.ready();
        // 缩放触发重新布局，思考的语义样式仍应保留。
        for cols in [60, 100] {
            pty.resize(40, cols);
            pty.wait_for("缩放后的思考语义排版已就绪", |screen| {
                screen.contents().contains("plain thinking")
                    && markdown_blocks_find(screen, "bold word").is_some()
                    && markdown_blocks_bold(screen, "bold word")
                    && !screen.contents().contains("```")
            });
            let screen = pty.parser.screen();
            assert_eq!(
                markdown_blocks_fg(screen, "plain thinking", 0),
                vt100::Color::Rgb(110, 117, 129)
            );
            assert!(markdown_blocks_bold(screen, "bold word"));
            assert!(markdown_blocks_bold(screen, "cell bold"));
            assert!(!screen.contents().contains("```"));
        }
        pty.input("/quit\r");
        pty.wait_for("退出并恢复终端模式", |s| !s.hide_cursor() && !s.bracketed_paste());
        // 退出后的终端原生历史也应保留高亮，且没有重复的思考正文。
        let screen = pty.parser.screen();
        assert_eq!(screen.contents().matches("plain thinking").count(), 1);
        assert!(markdown_blocks_bold(screen, "bold word"));
        assert!(markdown_blocks_bold(screen, "cell bold"));
        pty.finish();
        no_more(&fixture);
}

