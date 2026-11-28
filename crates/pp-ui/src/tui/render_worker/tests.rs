use super::*;
use crossterm::event::Event;
use pp_core::events::AgentEvent;
use ratatui::{Terminal, backend::TestBackend};

fn layout_text(layout: &EntryLayout) -> String {
    layout
        .iter()
        .map(|line| match &line.content {
            LayoutContent::Styled(line) => line.to_string(),
            _ => panic!("expected a styled row"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn rendered_from_markdown(source: &str, width: u16, muted: bool, streaming: bool) -> Rendered {
    let mut doc = super::super::markdown::parse_streaming(source, streaming);
    let (mut lines, tail) = doc.layout_cached_with_meta(width);
    apply_muted(&mut lines, muted);
    Rendered {
        epoch: 0,
        version: 1,
        theme: Theme::Default,
        icons: IconMode::Nerd,
        width,
        source: Arc::from(source),
        layout: Arc::new(prepared(lines, width)),
        ends_with_table: doc.ends_with_table(),
        table_tail_start: doc.table_tail_start,
        tail,
    }
}

#[test]
fn layout_worker_plain_and_initial_tail() {
    let rows = plain("first\n\n中\t🙂\n", 80, false);
    assert_eq!(
        rows.iter().map(Line::to_string).collect::<Vec<_>>(),
        ["first", "", "中  🙂", ""]
    );

    let worker = Worker::start().unwrap();
    let pause = worker.pause();
    let mut ticket = Ticket::default();
    let text = format!("{}🙂\n\nend", "中".repeat(3000));
    let preview = ticket.layout(&text, 8, false, false);
    pause.wait();
    let p_rows = preview
        .iter()
        .map(|line| match &line.content {
            LayoutContent::Styled(l) => l.to_string(),
            _ => panic!("expected a preview row"),
        })
        .collect::<Vec<_>>();
    assert!(
        p_rows[0].starts_with('中')
            && !p_rows.join("").contains('\u{fffd}')
            && p_rows.join("").contains('🙂')
    );
    assert_eq!(&p_rows[p_rows.len() - 2..], &["", "end"]);
    assert!(!ticket.exact);
    drop(pause);
}

#[test]
fn layout_worker_previews_latest_append_while_preserving_rendered_styles() {
    let worker = Worker::start().unwrap();
    let first = worker.pause();
    let mut ticket = Ticket::default();
    ticket.layout("**first**", 80, false, false);
    first.wait();
    let second = worker.pause();
    ticket.invalidate(false);
    ticket.layout("**first**\n\nsecond", 80, false, false);
    drop(first);
    // No queueing before the previous job is written back: pump until the write-back is absorbed and the latest version reaches the layout thread.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while ticket.in_flight != Some(ticket.version) {
        assert!(
            std::time::Instant::now() < deadline,
            "the latest version should be handed to the layout thread"
        );
        ticket.layout("**first**\n\nsecond", 80, false, false);
        std::thread::yield_now();
    }
    second.wait();
    let intermediate = ticket.layout("**first**\n\nsecond", 80, false, false);
    assert!(!ticket.exact);
    let LayoutContent::Styled(line) = &intermediate.get(0).expect("first preview row").content
    else {
        panic!("expected a Markdown row");
    };
    assert_eq!(line.to_string(), "first");
    assert!(line.spans.iter().any(|span| {
        span.style
            .add_modifier
            .contains(ratatui::style::Modifier::BOLD)
    }));
    assert_eq!(layout_text(&intermediate), "first\n\nsecond");
    drop(second);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        ticket.layout("**first**\n\nsecond", 80, false, false);
        if ticket.exact {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the latest layout should eventually complete"
        );
        std::thread::yield_now();
    }
    let third = worker.pause();
    ticket.invalidate(false);
    assert!(
        !ticket.exact,
        "appending revokes latest-version eligibility immediately"
    );
    let preview = ticket.layout("**first**\n\nsecond\n\nthird", 80, false, false);
    third.wait();
    assert_eq!(layout_text(&preview), "first\n\nsecond\n\nthird");
    assert!(!ticket.exact);
    ticket.invalidate(false);
    let preview = ticket.layout("**first**\n\nsecond\n\nthird 中🙂", 80, false, false);
    assert_eq!(layout_text(&preview), "first\n\nsecond\n\nthird 中🙂");
    assert!(!ticket.exact);
    drop(third);
}

#[test]
fn layout_worker_append_preview_matches_markdown_for_word_wrap_indents_and_newlines() {
    for (source, suffix, width, muted) in [
        ("**ab**", "中🙂", 4, true),
        ("**ab**", "\t中", 4, true),
        ("**ab**", "\n\n中\n", 4, true),
        ("**ab**\n", "中", 10, true),
        ("para1\n\n", "para2", 20, true),
        ("para1", "\n", 20, true),
        ("para1", "\n\n", 20, true),
        (
            "- Checking how streaming wrap behaves when words",
            " cross multiple lines without jumping",
            24,
            true,
        ),
        (
            "10. Ordered list item with long content",
            " continuing onto next wrapped rows",
            22,
            false,
        ),
        (
            "> Quoted thinking line with words",
            " wrapping across lines smoothly",
            20,
            true,
        ),
        (
            "Earlier paragraph.\n\n- item one\n- item two with **bold",
            "** and `code` wrapping cleanly",
            28,
            true,
        ),
    ] {
        let full = format!("{source}{suffix}");
        let result = rendered_from_markdown(source, width, muted, true);
        let preview = append_preview(&result, &full, width, muted, true);
        let expected = rendered_from_markdown(&full, width, muted, true);
        assert_eq!(
            layout_text(&preview),
            layout_text(&expected.layout),
            "source={source:?}, suffix={suffix:?}, width={width}"
        );
        assert_eq!(
            preview.height, expected.layout.height,
            "preview and background heights must match: source={source:?}, suffix={suffix:?}"
        );
    }

    let src_code = "```rust\nlet alpha = 1;\nlet beta = ";
    let res_code = rendered_from_markdown(src_code, 24, false, true);
    let prefix_line = res_code.layout.get(1).expect("first code line");
    for suffix in ["2;", "2;\n", "2;\nlet gamma = 3;\n", "12345678901234567890"] {
        let full = format!("{src_code}{suffix}");
        let preview = append_preview(&res_code, &full, 24, false, true);
        let expected = rendered_from_markdown(&full, 24, false, true);
        assert!(std::ptr::eq(prefix_line, preview.get(1).unwrap()));
        assert_eq!(layout_text(&preview), layout_text(&expected.layout));
        assert_eq!(preview.height, expected.layout.height);
    }

    let res_bound = rendered_from_markdown("**prefix**", 80, true, true);
    let long_sfx = format!("{}🙂\n\nlatest", "中".repeat(4000));
    let p_bound = append_preview(&res_bound, &format!("**prefix**{long_sfx}"), 80, true, true);
    let bound_txt = layout_text(&p_bound);
    assert!(
        bound_txt.starts_with("prefix\n…\n")
            && bound_txt.ends_with("🙂\n\nlatest")
            && !bound_txt.contains('\u{fffd}')
    );
    assert_eq!(
        layout_text(&append_preview(&res_bound, "**prefix**", 80, true, true)),
        "prefix"
    );

    let reuse_src = "**first**\n\nsecond";
    let res_reuse = rendered_from_markdown(reuse_src, 80, false, true);
    let p_reuse = append_preview(
        &res_reuse,
        &format!("{reuse_src}\n\nthird"),
        80,
        false,
        true,
    );
    assert!(std::ptr::eq(
        res_reuse.layout.get(0).unwrap(),
        p_reuse.get(0).unwrap()
    ));
    assert_eq!(layout_text(&p_reuse), "first\n\nsecond\n\nthird");
    assert!(Arc::ptr_eq(
        &append_preview(&res_reuse, reuse_src, 80, false, true),
        &res_reuse.layout
    ));
}

#[test]
fn layout_worker_oversized_active_block_fallback_preserves_word_wrap_indent_and_trailing_newline() {
    // Build a single top-level list item larger than PREVIEW_MARKDOWN_LIMIT and verify the fallback path still keeps word wrapping, hanging indents and no trailing blank row.
    let source = format!("- {}", "word ".repeat(1700));
    assert!(source.len() > PREVIEW_MARKDOWN_LIMIT);
    let result = rendered_from_markdown(&source, 24, true, true);
    for suffix in [
        "alpha beta gamma",
        "\n",
        "\n\n",
        "\ncontinuation words here",
    ] {
        let full = format!("{source}{suffix}");
        let preview = append_preview(&result, &full, 24, true, true);
        let expected = rendered_from_markdown(&full, 24, true, true);
        assert_eq!(
            preview.height, expected.layout.height,
            "oversized active block fallback height must match the background: suffix={suffix:?}"
        );
        let tail_rows = |layout: &EntryLayout| -> Vec<String> {
            let start = layout.len().saturating_sub(3);
            (start..layout.len())
                .map(
                    |index| match &layout.get(index).expect("tail row").content {
                        LayoutContent::Styled(line) => line.to_string(),
                        _ => panic!("expected a styled row"),
                    },
                )
                .collect()
        };
        assert_eq!(
            tail_rows(&preview),
            tail_rows(&expected.layout),
            "oversized active block fallback tail rows must match the background: suffix={suffix:?}"
        );
    }
}

#[test]
fn layout_worker_blocked_unchanged_and_rejection() {
    let worker = Worker::start().unwrap();
    let pause = worker.pause();
    let mut app = crate::tui::App::new(Err(anyhow::anyhow!("fixture"))).unwrap();
    app.conversation = app.conversation.fresh();
    app.state = crate::tui::State::Running;
    app.show_welcome = false;
    app.native_render = true;
    app.conversation.progress(AgentEvent::AssistantStarted);
    app.conversation.progress(AgentEvent::AssistantDelta {
        text: "```rust\nlet value = 1;\n```".into(),
        refusal: false,
    });
    app.conversation.progress(AgentEvent::AssistantFinished {
        text: "```rust\nlet value = 1;\n```".into(),
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal
        .draw(|frame| app.draw(frame, &Default::default()))
        .unwrap();
    assert!(
        app.conversation.render_pending()
            && app.conversation.next_native_batch(100, 1, true).is_none()
    );
    app.handle(Event::Paste("可继续输入🙂".into()), &mut false);
    assert_eq!(app.input.lines(), &["可继续输入🙂"]);

    let mut ticket = Ticket::default();
    let first = ticket.layout("正文🙂", 80, false, false);
    let source = ticket.source.clone();
    let version = ticket.version;
    let second = ticket.layout("正文🙂", 80, false, false);
    assert!(
        Arc::ptr_eq(&first, &second)
            && Arc::ptr_eq(&source, &ticket.source)
            && version == ticket.version
    );
    ticket.invalidate(false);
    ticket.layout("正文🙂新增", 80, false, false);
    assert_eq!(ticket.version, version + 1);

    let mut t2 = Ticket::default();
    t2.layout("old", 80, false, false);
    pause.wait();
    assert!(t2.slot.lock().unwrap().is_none());
    let old_epoch = t2.epoch;
    t2.invalidate(true);
    t2.layout("new", 80, false, false);
    *t2.slot.lock().unwrap() = Some(Rendered {
        epoch: old_epoch,
        version: u64::MAX,
        theme: Theme::Default,
        icons: IconMode::Nerd,
        width: 80,
        source: Arc::from("old"),
        layout: Arc::new(prepared(vec![Line::raw("stale")], 80)),
        ends_with_table: false,
        table_tail_start: None,
        tail: TailMeta::default(),
    });
    let rows = t2.layout("new", 80, false, false);
    assert!(!t2.exact);
    let LayoutContent::Styled(l0) = &rows.get(0).unwrap().content else {
        panic!()
    };
    assert_eq!(l0.to_string(), "new");
    *t2.slot.lock().unwrap() = Some(Rendered {
        epoch: t2.epoch,
        version: t2.version,
        theme: Theme::Default,
        icons: IconMode::Nerd,
        width: 80,
        source: Arc::from("new"),
        layout: Arc::new(prepared(vec![Line::raw("new")], 80)),
        ends_with_table: false,
        table_tail_start: None,
        tail: TailMeta::default(),
    });
    t2.layout("new", 80, false, false);
    assert!(t2.exact);
    t2.layout("new", 40, false, false);
    assert!(!t2.exact);
    drop(pause);
}

// ---------------------------------------------------------------------------
// Streaming frames: the background document is updated incrementally, so every frame it commits and
// every preview the main thread derives from it must equal a frame built from a fresh parse.
// ---------------------------------------------------------------------------

struct Background {
    source: Arc<str>,
    document: super::super::markdown::MarkdownDocument,
    width: u16,
}

impl Background {
    fn start(source: &str, width: u16) -> Self {
        Self {
            source: Arc::from(source),
            document: super::super::markdown::parse_streaming(source, true),
            width,
        }
    }

    /// Mirror the layout worker: update the cached document from the previous snapshot, then lay out.
    fn render(&mut self, text: &str, muted: bool) -> Rendered {
        if self.source.as_ref() != text {
            self.document.update(&self.source, text, true);
            self.source = Arc::from(text);
        }
        let (mut lines, tail) = self.document.layout_cached_with_meta(self.width);
        apply_muted(&mut lines, muted);
        Rendered {
            epoch: 0,
            version: 1,
            theme: Theme::Default,
            icons: IconMode::Nerd,
            width: self.width,
            source: self.source.clone(),
            layout: Arc::new(prepared(lines, self.width)),
            ends_with_table: self.document.ends_with_table(),
            table_tail_start: self.document.table_tail_start,
            tail,
        }
    }
}

#[test]
fn layout_worker_streaming_frames_match_full_parse_for_each_append() {
    let width = 32u16;
    let base = "# Title\n\nIntro with **bold** and a [link](https://example.test/a).\n\n\
- item one\n- item two\n\n\
```rust\nlet alpha = 1;\n";
    let suffix = "let beta = 2;\n```\n\nappended paragraph\n\n- appended item\n\n\
| A | B |\n| --- | --- |\n| 1 | 2 |";
    let boundaries: Vec<usize> = suffix
        .char_indices()
        .map(|(index, _)| index)
        .chain(std::iter::once(suffix.len()))
        .collect();
    let steps: Vec<String> = boundaries
        .iter()
        .skip(1)
        .step_by(3)
        .map(|&end| format!("{base}{}", &suffix[..end]))
        .collect();
    assert!(steps.len() >= 8, "the walk must cover several appends");
    for muted in [false, true] {
        let mut background = Background::start(base, width);
        let mut frame = background.render(base, muted);
        let mut previous = base.to_string();
        for next in &steps {
            let reference = rendered_from_markdown(&previous, width, muted, true);
            let preview = append_preview(&frame, next, width, muted, true);
            let expected = append_preview(&reference, next, width, muted, true);
            assert_eq!(
                layout_text(&preview),
                layout_text(&expected),
                "streaming preview differs from a fresh parse: muted={muted}, next={next:?}"
            );
            assert_eq!(
                preview.height, expected.height,
                "streaming preview height differs: muted={muted}"
            );
            frame = background.render(next, muted);
            let committed = rendered_from_markdown(next, width, muted, true);
            assert_eq!(
                layout_text(&frame.layout),
                layout_text(&committed.layout),
                "committed rows differ from a fresh parse: muted={muted}, next={next:?}"
            );
            assert_eq!(frame.layout.height, committed.layout.height);
            assert_eq!(frame.table_tail_start, committed.table_tail_start);
            assert_eq!(frame.ends_with_table, committed.ends_with_table);
            previous = next.clone();
        }
    }
}

/// Wait for the background write-back of `text`, which is the frame the terminal actually shows.
fn wait_for_exact(
    ticket: &mut Ticket,
    text: &str,
    width: u16,
    streaming: bool,
) -> Arc<EntryLayout> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let layout = ticket.layout(text, width, false, streaming);
        if ticket.exact {
            return layout;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the background layout should settle"
        );
        std::thread::yield_now();
    }
}

/// Foreground of the Rust `let` keyword in the committed code row, i.e. the theme's syntax color.
fn code_keyword_colors(layout: &EntryLayout) -> Vec<Option<ratatui::style::Color>> {
    layout
        .iter()
        .filter_map(|row| match &row.content {
            LayoutContent::Styled(line) => line
                .spans
                .iter()
                .find(|span| span.content.contains("let"))
                .map(|span| span.style.fg),
            _ => None,
        })
        .collect()
}

#[test]
fn layout_worker_rehighlights_streaming_code_after_a_theme_change() {
    // The worker must outlive the ticket: it owns the layout thread the ticket dispatches to.
    let _worker = Worker::start().unwrap();
    let mut ticket = Ticket::default();
    let theme_keyword = |theme: Theme| {
        crate::tui::themes::set_active(theme, IconMode::Nerd);
        crate::tui::transcript_style::syntax_keyword()
    };
    let source = "```rust\nlet alpha = 1;\n```\n\nstreaming tail";
    let dark = theme_keyword(Theme::Default);
    let night = theme_keyword(Theme::TokyoNight);
    assert_ne!(dark, night, "the fixture needs two distinct palettes");

    crate::tui::themes::set_active(Theme::Default, IconMode::Nerd);
    let first = wait_for_exact(&mut ticket, source, 32, true);
    assert_eq!(code_keyword_colors(&first), vec![Some(dark)]);

    // Swapping the theme mid-stream must not keep the settled code block in the old palette.
    crate::tui::themes::set_active(Theme::TokyoNight, IconMode::Nerd);
    let extended = format!("{source} and more");
    let second = wait_for_exact(&mut ticket, &extended, 32, true);
    assert_eq!(code_keyword_colors(&second), vec![Some(night)]);
    crate::tui::themes::set_active(Theme::Default, IconMode::Nerd);
}
