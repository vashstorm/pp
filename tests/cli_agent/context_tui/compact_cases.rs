// Coverage 4 (manual): while the server holds the summary SSE the screen shows Compacting; a new draft typed
// during compaction is preserved; after Esc the test waits for the real terminal state back to Ready with the draft
// untouched, the next task request carries the full old history plus the new input, and the provider summary output
// never enters the visible conversation.
// Coverage 4 (manual): a successful compaction shows no task Completed, adds no You: /compact, and clears no
// Transcript; the status bar usage drops; the next ordinary request holds exactly one summary wrapper plus the
// native recent suffix; at 60/30 column resize the context fields are kept first and Ctrl-C restores terminal flags.
#[test]
fn context_tui_compact_success_keeps_dialogue_drops_usage_and_survives_narrow_resize() {
    let protocol = "response";
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 40000, 80);
        assert!(context_tui_metrics_hidden(pty.parser.screen()));
        context_tui_fill_three_exchanges(&mut pty, &fixture, protocol);
        let before = context_tui_ctx_tokens(pty.parser.screen());
        pty.input("/compact\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        context_tui_assert_summary_body(&body, protocol, "context-old-one", "context-recent-keep");
        pty.input("/model ");
        // Models may still be completed while the summary request is in flight; confirm the busy check still applies.
        pty.model_search("");
        pty.text("fixture/primary");
        pty.input("\r");
        pty.model_search("");
        pty.text("Compacting context...");
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
        pty.wait_for("the post-compaction usage snapshot is published", |screen| {
            context_tui_ctx_tokens(screen) < before
        });
        let after = context_tui_ctx_tokens(pty.parser.screen());
        assert!(
            after < before,
            "the post-compaction estimate must drop: before {before} after {after}"
        );
        // The old conversation stays in the Transcript: paging up reveals the earliest old marker.
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
        assert!(paged, "compaction must not clear the old conversation: {}", pty.diagnostics());
        // The summary text and summary instruction never enter the visible conversation, and the command itself is not a conversation entry.
        let visible = pty.parser.screen().contents();
        assert!(!visible.contains(CONTEXT_TUI_SUMMARY), "{visible}");
        assert!(!visible.contains(CONTEXT_TUI_INSTRUCTION), "{visible}");
        assert!(!visible.contains("/compact"), "{visible}");
        for _ in 0..80 {
            if !resume_history_page(&mut pty, "\x1b[6~") {
                break;
            }
        }
        // The next ordinary request: one summary wrapper plus the native recent suffix, no large old prefix.
        pty.input("context-after-question\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let text = serde_json::to_string(&body).unwrap();
        assert_eq!(text.matches("[Context summary]").count(), 1);
        assert!(text.contains(CONTEXT_TUI_SUMMARY));
        assert!(text.contains("context-recent-keep"));
        assert!(!text.contains("context-old-one"), "the old prefix text must not come back");
        assert!(text.contains("context-after-question"));
        assert!(text.contains("noted context-recent-keep"));
        if protocol == "completion" {
            final_reply(request, "context after answer");
        } else {
            response_final(request, "context after answer");
        }
        pty.text("context after answer");
        pty.ready();
        // Narrow resize: the context fields are kept first and no width overflows or panics.
        // The first bytes after a resize may still belong to the previous frame, so require the target layout as the redraw fence.
        for width in [60, 30] {
            pty.resize(30, width);
            pty.wait_for("the narrow redraw keeps the context fields", |screen| {
                screen
                    .contents()
                    .lines()
                    .nth(26)
                    .is_some_and(|line| line.contains("\u{e62a} "))
            });
        }
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
}

