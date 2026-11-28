use super::*;
use crossterm::event::Event;
use pp_core::events::AgentEvent;
use ratatui::{Terminal, backend::TestBackend};

fn layout_text(layout: &EntryLayout) -> String {
    layout
        .iter()
        .map(|line| match &line.content {
            LayoutContent::Styled(line) => line.to_string(),
            _ => panic!("应为样式行"),
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
            _ => panic!("应为预览行"),
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
    // 上一任务未回写前不排队：泵到回写被吸收、最新版本交给排版线程为止。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while ticket.in_flight != Some(ticket.version) {
        assert!(
            std::time::Instant::now() < deadline,
            "最新版本应交给排版线程"
        );
        ticket.layout("**first**\n\nsecond", 80, false, false);
        std::thread::yield_now();
    }
    second.wait();
    let intermediate = ticket.layout("**first**\n\nsecond", 80, false, false);
    assert!(!ticket.exact);
    let LayoutContent::Styled(line) = &intermediate.get(0).expect("预览首行").content else {
        panic!("应为 Markdown 行");
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
        assert!(std::time::Instant::now() < deadline, "应最终完成最新排版");
        std::thread::yield_now();
    }
    let third = worker.pause();
    ticket.invalidate(false);
    assert!(!ticket.exact, "追加后立即撤销最新版本资格");
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
            "预览与后台高度必须一致：source={source:?}, suffix={suffix:?}"
        );
    }

    let src_code = "```rust\nlet alpha = 1;\nlet beta = ";
    let res_code = rendered_from_markdown(src_code, 24, false, true);
    let prefix_line = res_code.layout.get(1).expect("第一行代码");
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
    // 构造超过 PREVIEW_MARKDOWN_LIMIT 的单个顶层列表项，验证回退路径仍保持词级换行、悬挂缩进与无尾空行。
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
            "超大活跃块回退高度应与后台一致：suffix={suffix:?}"
        );
        let tail_rows = |layout: &EntryLayout| -> Vec<String> {
            let start = layout.len().saturating_sub(3);
            (start..layout.len())
                .map(|index| match &layout.get(index).expect("尾行").content {
                    LayoutContent::Styled(line) => line.to_string(),
                    _ => panic!("应为样式行"),
                })
                .collect()
        };
        assert_eq!(
            tail_rows(&preview),
            tail_rows(&expected.layout),
            "超大活跃块回退尾行应与后台一致：suffix={suffix:?}"
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
