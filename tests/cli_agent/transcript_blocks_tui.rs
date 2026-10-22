// Markdown 块级渲染 PTY 回归：真实 PTY + 本地 SSE fixture 覆盖计划中 Markdown
// 验证行 —— 两协议分段流式的中间态屏幕与 vt100 样式断言（先观察屏幕再发下一段）、
// 未闭合 delimiter 与最终权威正文不重复、refusal 替换/refusal-only/空正文边界、
// bash/read 字面输出不被 Markdown 解释、Unicode 表格与 >8192 grapheme 长行在
// 100↔24 列缩放下的内容不丢失、运行中 PgUp 保持视口并在回到底部后恢复跟随、
// 以及退出 scrollback 的完整样式回放。辅助函数以 `markdown_blocks_` 前缀命名，
// 测试函数以 `transcript_blocks_markdown_` 前缀命名，不新增 use 声明以免与
// 兄弟 include 文件冲突。

const MARKDOWN_BLOCKS_ERROR: vt100::Color = vt100::Color::Rgb(224, 108, 117);

fn markdown_blocks_start(fixture: &Server, protocol: &str, home: &Path, project: &Path) -> Pty {
    std::fs::write(config_path(home, "settings.yml"), r#"{"default_model":"fixture/primary","notification_timeout_seconds":1}"#).unwrap();
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
fn markdown_blocks_refusal_delta(stream: &mut SseReply, protocol: &str, text: &str) {
    if protocol == "completion" {
        stream.event(chat_chunk(json!({"role":"assistant","refusal":text}), None));
    } else {
        stream.event(json!({
            "type":"response.refusal.delta", "sequence_number":0,
            "item_id":"msg-live", "output_index":0, "content_index":0,
            "delta":text, "logprobs":[]
        }));
        stream.event(json!({
            "type":"response.content_part.done", "sequence_number":1,
            "item_id":"msg-live", "output_index":0, "content_index":0,
            "part":{"type":"refusal","refusal":text}
        }));
    }
}

// response 协议的正文与 refusal 属于不同 content part，正文 delta 必须落在
// (0,1)；completion 仍复用既有 streaming_delta。
fn markdown_blocks_content_delta(stream: &mut SseReply, protocol: &str, text: &str) {
    if protocol == "completion" {
        streaming_delta(stream, protocol, text);
    } else {
        stream.event(json!({
            "type":"response.output_text.delta", "sequence_number":2,
            "item_id":"msg-live", "output_index":0, "content_index":1,
            "delta":text, "logprobs":[]
        }));
    }
}

fn markdown_blocks_refusal_content_complete(stream: &mut SseReply, refusal: &str, text: &str) {
    stream.event(json!({
        "type":"response.completed", "sequence_number":3,
        "response": response_body(json!([response_message("msg-live", json!([
            {"type":"refusal","refusal":refusal}, response_text(text)
        ]))]))
    }));
}

fn markdown_blocks_reply_calls(request: Request, protocol: &str, batch: &[(&str, &str, Value)]) {
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

fn markdown_blocks_final(request: Request, protocol: &str, text: &str) {
    if protocol == "completion" {
        final_reply(request, text);
    } else {
        response_final(request, text);
    }
}

#[test]
fn transcript_blocks_markdown_streams_staged_blocks_and_styles() {
    let stage1 = "# Fixture Title\n\n**bold";
    let stage2 = " word** and `inline code`";
    let stage3 = " with [docs](https://example.com/x).\n\n- first item\n  - nested item\n- [x] done task\n- [ ] open task\n\n> quoted line\n\n```rust\nlet x = 1;\n";
    let stage4 = "```";
    let full = format!("{stage1}{stage2}{stage3}{stage4}");
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = markdown_blocks_start(&fixture, protocol, home.path(), project.path());
        pty.ready();
        pty.input("stream markdown please\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = SseReply::new(request);

        // 阶段 1：标题加粗渲染，未闭合的 ** 保持字面。
        streaming_delta(&mut stream, protocol, stage1);
        pty.wait_for("标题与未闭合加粗的中间态", |s| {
            s.contents().contains("Fixture Title")
                && s.contents().contains("**bold")
                && !s.contents().contains(" word**")
                && s.contents().contains("✻ Responding")
        });
        {
            let screen = pty.parser.screen();
            assert!(markdown_blocks_bold(screen, "Fixture Title"));
            // 未闭合的 ** 按字面呈现，不加粗。
            assert!(!markdown_blocks_bold(screen, "**bold"));
        }

        // 阶段 2：加粗闭合后 delimiter 消失，inline code 上屏。
        streaming_delta(&mut stream, protocol, stage2);
        pty.wait_for("闭合加粗与 inline code", |s| {
            s.contents().contains("bold word")
                && s.contents().contains("inline code")
                && !s.contents().contains("**")
        });
        {
            let screen = pty.parser.screen();
            assert!(markdown_blocks_bold(screen, "bold word"));
            assert_eq!(screen.contents().matches("Fixture Title").count(), 1);
        }

        // 阶段 3：列表/任务/引用/链接/代码框全部就位。
        streaming_delta(&mut stream, protocol, stage3);
        pty.wait_for("列表、任务、引用、链接与代码框", |s| {
            s.contents().contains("• first item")
                && s.contents().contains("nested item")
                && s.contents().contains("done task")
                && s.contents().contains("open task")
                && s.contents().contains("quoted line")
                && s.contents().contains(" rust ")
                && s.contents().contains("let x = 1;")
                && s.contents().contains("docs")
        });

        // 结束前先补上未闭合的 fence，再以最终权威正文收尾。
        streaming_delta(&mut stream, protocol, stage4);
        pty.wait_for("闭合 fence 出现底线且无遗留反引号", |s| {
            let text = s.contents();
            // 未闭合时只有顶线；闭合后出现仅由框线组成的底线（顶线带语言标签）。
            text.lines().any(|line| {
                line.starts_with("  ─") && line.chars().all(|ch| ch == '─' || ch == ' ')
            }) && !text.contains("```")
        });

        // 结束：最终权威正文只渲染一次，没有重复块或遗留 delimiter。
        streaming_complete(&mut stream, protocol, &full);
        stream.finish();
        pty.ready();
        {
            let screen = pty.parser.screen();
            let text = screen.contents();
            assert_eq!(text.matches("Fixture Title").count(), 1, "{text}");
            assert_eq!(text.matches("let x = 1;").count(), 1, "{text}");
            assert_eq!(text.matches("• first item").count(), 1, "{text}");
            assert!(!text.contains("**"), "{text}");
            assert!(!text.contains("```"), "{text}");
            assert!(markdown_blocks_bold(screen, "bold word"));
        }
        no_more(&fixture);
    }
}

#[test]
fn transcript_blocks_markdown_refusal_swap_refusal_only_and_empty_turn() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = markdown_blocks_start(&fixture, protocol, home.path(), project.path());
        pty.ready();
        pty.input("refusal swap\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = SseReply::new(request);
        markdown_blocks_refusal_delta(&mut stream, protocol, "provisional refusal mark");
        pty.text("provisional refusal mark");
        markdown_blocks_content_delta(&mut stream, protocol, "content wins final mark");
        pty.wait_for("正文替换 provisional refusal", |s| {
            s.contents().contains("content wins final mark")
                && !s.contents().contains("provisional refusal mark")
        });
        if protocol == "completion" {
            streaming_complete(&mut stream, protocol, "content wins final mark");
        } else {
            markdown_blocks_refusal_content_complete(
                &mut stream,
                "provisional refusal mark",
                "content wins final mark",
            );
        }
        stream.finish();
        pty.wait_for("最终正文恰好出现一次", |s| {
            s.contents().matches("content wins final mark").count() == 1 && input_is_ready(s)
        });
        no_more(&fixture);

        // refusal-only 只在 completion 协议下成为最终权威正文（response 协议的
        // 纯 refusal 轮不产生可见正文，由既有协议语义决定）。
        if protocol == "completion" {
            pty.input("refusal only\r");
            let (request, _) = pty.request_at(&fixture, api_path(protocol));
            let mut stream = SseReply::new(request);
            markdown_blocks_refusal_delta(&mut stream, protocol, "only refusal final text");
            streaming_complete(&mut stream, protocol, "");
            stream.finish();
            pty.wait_for("refusal-only 成为可见正文且只出现一次", |s| {
                s.contents().matches("only refusal final text").count() == 1
                    && input_is_ready(s)
            });
            no_more(&fixture);

            // 空正文：报错弹窗，不产生空 Markdown 卡片，旧内容不被复制。
            pty.input("empty final\r");
            let (request, _) = pty.request_at(&fixture, api_path(protocol));
            let mut stream = SseReply::new(request);
            stream.event(chat_chunk(json!({"role":"assistant","content":""}), None));
            streaming_complete(&mut stream, protocol, "");
            stream.finish();
            pty.wait_for("空正文报错弹窗", |s| {
                notification_bounds(s, "Error", MARKDOWN_BLOCKS_ERROR).is_some()
                    && s.contents().contains("neither text nor")
            });
            assert_eq!(
                pty.parser.screen().contents().matches("content wins final mark").count(),
                1
            );
            pty.ready();
            no_more(&fixture);
        }
    }
}

#[test]
fn transcript_blocks_markdown_tool_output_stays_literal() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join("literal.md"),
            "# From File Heading\n**file bold** stays\n",
        )
        .unwrap();
        let mut pty = markdown_blocks_start(&fixture, protocol, home.path(), project.path());
        pty.ready();
        pty.input("show literal tool output\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        markdown_blocks_reply_calls(
            request,
            protocol,
            &[
                (
                    "mb-bash",
                    "bash",
                    json!({"command":"echo '**literal**' '# not a heading'"}),
                ),
                ("mb-read", "read", json!({"path":"literal.md"})),
            ],
        );
        pty.wait_for("bash 与 read 的字面输出上屏", |s| {
            s.contents().contains("**literal** # not a heading")
                && s.contents().contains("# From File Heading")
        });
        {
            let screen = pty.parser.screen();
            let text = screen.contents();
            // stdout 行（非标题行）保持字面：星号与 # 都在，无标题/列表/代码框样式。
            assert!(!markdown_blocks_bold(screen, "**literal** # not a heading"));
            assert_eq!(
                markdown_blocks_fg(screen, "# From File Heading", 0),
                vt100::Color::Default,
                "{text}"
            );
            assert!(!markdown_blocks_bold(screen, "# From File Heading"));
            assert!(!text.contains(" rust "), "{text}");
            assert!(!text.contains("• "), "{text}");
        }
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        markdown_blocks_final(request, protocol, "Final **styled** answer");
        pty.wait_for("字面工具输出后助手 Markdown 仍正常渲染", |s| {
            s.contents().contains("Final") && input_is_ready(s)
        });
        {
            let screen = pty.parser.screen();
            assert!(markdown_blocks_bold(screen, "styled"));
            // bash 标题与 stdout 各出现一次，字面输出未被重复解释。
            assert_eq!(screen.contents().matches("**literal**").count(), 2);
            assert_eq!(screen.contents().matches("**file bold**").count(), 1);
        }
        no_more(&fixture);
    }
}

#[test]
fn transcript_blocks_markdown_unicode_and_long_line_survive_resize() {
    // 8624 个 x 保证换行尾巴在 98 列与 22 列两个宽度下都恰好从行首开始，
    // 断言不依赖断行位置；总 grapheme 数远超 8192。
    let long_line = format!("{}long-line-tail中😀", "x".repeat(8624));
    let document = format!(
        "# 中文标题 😀\n\n- 顶层\n  - 嵌套\n- [x] 已完成\n- [ ] 待办\n\n> 引用行 中文\n\n```text\n宽度保持 中文 😀\n```\n\n{long_line}\n\n| 名称 | 😀 | Note |\n| --- | :-: | ---: |\n| 中文名 | cafe\u{301} | ✅ |\n| 第二行 | e\u{301}x | 😀 |\n"
    );
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = markdown_blocks_start(&fixture, "completion", home.path(), project.path());
    pty.ready();
    pty.input("render unicode stress\r");
    let (request, _) = pty.request_at(&fixture, api_path("completion"));
    markdown_blocks_final(request, "completion", &document);
    let bottom = |screen: &vt100::Screen| {
        let text = screen.contents();
        text.contains("long-line-tail")
            && text.contains("名称")
            && text.contains("中文名")
            && text.contains("第二行")
            && text.contains("cafe\u{301}")
            && text.contains("✅")
            && text.contains("😀")
    };
    pty.wait_for("100 列底部显示表格与长行尾巴", |s| bottom(s));
    // The session toast can cover the entire narrow table after resize.
    pty.wait_for("会话通知消失后检查窄屏正文", |s| !s.contents().contains("Session:"));
    let top = |screen: &vt100::Screen| {
        let text = screen.contents();
        text.contains("宽度保持 中文 😀")
            && text.contains("中文标题")
            && text.contains("• 顶层")
            && text.contains("嵌套")
            && text.contains("已完成")
            && text.contains("待办")
            && text.contains("引用行")
    };
    // Fence each page with an observed draft update. Repeated x-only pages can look
    // identical, and a resize redraw alone does not acknowledge queued paging keys.
    let page_until = |pty: &mut Pty, key: &str, visible: fn(&vt100::Screen) -> bool| {
        for step in 0..64 {
            pty.input(key);
            let marker = format!("page-fence-{step}");
            pty.input("\x01\x0b");
            context_tui_paste(pty, &marker);
            pty.draft(&marker);
            if visible(pty.parser.screen()) {
                pty.input("\x01\x0b");
                pty.draft("");
                return;
            }
        }
        panic!("paging did not reach the expected content: {}", pty.diagnostics());
    };
    for (rows, cols) in [(30u16, 24u16), (30u16, 100u16)] {
        pty.resize(rows, cols);
        page_until(&mut pty, "\x1b[6~", bottom);
        page_until(&mut pty, "\x1b[5~", top);
    }
    page_until(&mut pty, "\x1b[6~", bottom);
    pty.ready();
    no_more(&fixture);
    pty.input("/quit\r");
    pty.finish();
}

#[test]
fn transcript_blocks_markdown_scroll_offset_follow_and_exit_scrollback() {
    let paragraphs = |range: std::ops::Range<usize>| {
        range
            .map(|index| format!("para-{index:02} 段落中😀"))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let stage1 = format!(
        "# Scrollback Heading\n\n```text\nscrollback code line\n```\n\n{}",
        paragraphs(0..10)
    );
    let stage2 = paragraphs(10..30);
    let full = format!("{stage1}\n\n{stage2}");
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = markdown_blocks_start(&fixture, "completion", home.path(), project.path());
    pty.ready();
    pty.input("scroll marker task\r");
    let (request, _) = pty.request_at(&fixture, api_path("completion"));
    let mut stream = SseReply::new(request);
    streaming_delta(&mut stream, "completion", &stage1);
    pty.wait_for("流式中间态已显示代码块与早期段落", |s| {
        s.contents().contains("scrollback code line")
            && s.contents().contains("para-09")
            && s.contents().contains("✻ Responding")
    });
    pty.input("\x1b[5~");
    pty.wait_for("运行中 PgUp 离开底部", |s| {
        !s.contents().contains("para-29")
    });
    let old_view: Vec<String> = pty
        .parser
        .screen()
        .contents()
        .lines()
        .take(25)
        .map(str::to_owned)
        .collect();

    // 追加内容并结束：用户上翻时视口保持原位，不因新 chunk 或完成而拉回底部。
    streaming_delta(&mut stream, "completion", &format!("\n\n{stage2}"));
    streaming_complete(&mut stream, "completion", &full);
    stream.finish();
    pty.ready();
    assert_eq!(
        pty.parser
            .screen()
            .contents()
            .lines()
            .take(25)
            .map(str::to_owned)
            .collect::<Vec<_>>(),
        old_view
    );

    // 回到底部后恢复跟随：后续流式内容无需翻页即自动出现。
    for _ in 0..4 {
        pty.input("\x1b[6~");
    }
    pty.text("para-29");
    pty.input("follow bottom task\r");
    let (request, _) = pty.request_at(&fixture, api_path("completion"));
    let mut stream = SseReply::new(request);
    streaming_delta(&mut stream, "completion", "follow-bottom-restored 中");
    pty.text("follow-bottom-restored 中");
    streaming_complete(&mut stream, "completion", "follow-bottom-restored 中");
    stream.finish();
    pty.ready();
    no_more(&fixture);

    pty.input("/quit\r");
    pty.wait_for("退出后滚动历史写入主屏", |s| {
        !s.alternate_screen() && s.contents().contains("follow-bottom-restored")
    });
    let marker = b"\x1b[?1049l";
    let start = pty
        .raw
        .windows(marker.len())
        .rposition(|bytes| bytes == marker)
        .unwrap();
    let mut parser = vt100::Parser::new(150, 100, 0);
    parser.process(&pty.raw[start + marker.len()..]);
    let screen = parser.screen();
    let text = screen.contents();
    assert_eq!(text.matches("Scrollback Heading").count(), 1, "{text}");
    assert_eq!(text.matches("scrollback code line").count(), 1, "{text}");
    for index in 0..30 {
        let line = format!("para-{index:02}");
        assert_eq!(text.matches(&line).count(), 1, "{line}\n{text}");
    }
    assert_eq!(text.matches("follow-bottom-restored").count(), 1, "{text}");
    pty.finish();
}
