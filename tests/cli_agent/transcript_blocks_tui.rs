// PTY regression for Markdown block rendering: a real PTY plus the local SSE fixture cover the Markdown
// verification rows of the plan — the intermediate screens and vt100 style assertions of fragmented
// streaming for both protocols (observe the screen before sending the next chunk), unclosed delimiters
// not duplicating the final authoritative body, refusal replacement/refusal-only/empty-body boundaries,
// bash/read literal output not being interpreted as Markdown, the content of Unicode tables and >8192
// grapheme long lines surviving a resize between 100 and 24 columns, PgUp keeping the viewport while
// running and following resuming after returning to the bottom, and full style replay in the exit
// scrollback. Helpers are named with the `markdown_blocks_` prefix and test functions with the
// `transcript_blocks_markdown_` prefix, and no use declaration is added to avoid clashing with sibling
// include files.

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

// Locate needle on the screen: returns (row, col). Every needle used for color sampling has only
// single-column characters before it (ASCII/box drawing/tab), so the character index equals the column.
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

// completion: the refusal delta goes through the chat chunk's refusal field; response: the refusal
// occupies the (0,0) part and content_part.done follows immediately so the stream position advances to
// the next column.
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

// On the response protocol the body and the refusal belong to different content parts, so a body delta
// must land on (0,1); completion still reuses the existing streaming_delta.
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
                    pty.wait_for("thinking title style is ready", |screen| screen.contents().contains("**bold") && markdown_blocks_find(screen, "Reasoning title").is_some() && markdown_blocks_bold(screen, "Reasoning title"));
                    assert!(markdown_blocks_bold(pty.parser.screen(), "Reasoning title"));
                    assert!(!markdown_blocks_bold(pty.parser.screen(), "**bold"));
                }
                1 => {
                    let keyword_color = vt100::Color::Rgb(197, 134, 192);
                    // The preview shows the code text first and background typesetting fills in the syntax
                    // highlighting.
                    pty.wait_for("thinking stream fills in bold and the code block", |s| {
                        markdown_blocks_find(s, "let x = 1;")
                            .and_then(|(row, col)| s.cell(row, col))
                            .map(vt100::Cell::fgcolor)
                            == Some(keyword_color)
                            && !s.contents().contains("**bold")
                    });
                    let screen = pty.parser.screen();
                    assert!(markdown_blocks_bold(screen, "bold word"));
                    assert_eq!(
                        markdown_blocks_fg(screen, "bold word", 0),
                        vt100::Color::Rgb(174, 72, 182)
                    );
                    // The default theme distinguishes inline code in green from bold in purple.
                    assert_eq!(
                        markdown_blocks_fg(screen, "inline code", 0),
                        vt100::Color::Rgb(55, 165, 98)
                    );
                }
                _ => {
                    pty.wait_for("thinking stream shows the table, list and quote", |s| {
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
        // A resize triggers a new layout, and the semantic styles of thinking must survive it.
        for cols in [60, 100] {
            pty.resize(40, cols);
            pty.wait_for("semantic thinking typesetting is ready after resize", |screen| {
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
        pty.wait_for("quit restores the terminal mode", |s| !s.hide_cursor() && !s.bracketed_paste());
        // The terminal's native scrollback after exiting must also keep the highlighting, without duplicating
        // the thinking body.
        let screen = pty.parser.screen();
        assert_eq!(screen.contents().matches("plain thinking").count(), 1);
        assert!(markdown_blocks_bold(screen, "bold word"));
        assert!(markdown_blocks_bold(screen, "cell bold"));
        pty.finish();
        no_more(&fixture);
}

