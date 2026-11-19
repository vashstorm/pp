#[test]
fn context_tui_shows_current_and_window_on_input_top_right() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 1_280_000, 80);
        // 初始无任何数据时整行隐藏（含 ctx）。
        pty.wait_for("初始无数据时隐藏输入栏指标行", context_tui_metrics_hidden);

        context_tui_exchange(
            &mut pty,
            &fixture,
            protocol,
            "context-position-seed",
            "context-position-ok",
        );
        let contents = pty.parser.screen().contents();
        let lines: Vec<_> = contents.lines().collect();
        assert!(lines[26].contains("\u{e62a} ") && lines[26].contains("/1.3M · "));
        // 右对齐位置用真实 screen cell 验证；新指标占据左侧，字面长度不再固定。
        let (row, column) =
            context_tui_locate(pty.parser.screen(), "\u{e62a} ")
                .expect("状态栏必须在输入栏上沿显示 Ctx 容量");
        assert_eq!(row, 26);
        let (cache_row, cache_column) =
            context_tui_locate(pty.parser.screen(), "\u{f01bc} —/—").unwrap();
        assert_eq!(cache_row, row);
        assert!(column < cache_column);
        let (timer_row, timer_column) =
            context_tui_locate(pty.parser.screen(), "\u{f051b} ").unwrap();
        assert_eq!(timer_row, row);
        assert!(cache_column < timer_column);
        context_tui_assert_ctx_white(&pty);
        assert!(!lines[29].contains("\u{e62a} "));
        let before_draft = context_tui_ctx_tokens(pty.parser.screen());
        assert!(before_draft > 0.0);
        pty.input("context-position-draft");
        pty.draft("context-position-draft");
        assert_eq!(context_tui_ctx_tokens(pty.parser.screen()), before_draft);
        pty.resize(12, 40);
        pty.wait_for("缩放后用量仍在输入栏上沿右端", |screen| {
            screen
                .contents()
                .lines()
                .nth(8)
                .is_some_and(|line| line.contains(" \u{e62a} ") && line.contains("/1.3M"))
        });
        pty.draft("context-position-draft");
        assert_eq!(context_tui_ctx_tokens(pty.parser.screen()), before_draft);
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
        pty.wait_for("新会话清空指标并整行隐藏", context_tui_metrics_hidden);
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

// 关闭自动压缩，分别让用量越过压缩阈值和容量，验证上下文字段始终为白色。
#[test]
fn context_tui_context_field_stays_white_above_threshold_and_capacity() {
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
    context_tui_assert_ctx_white(&pty);
    context_tui_exchange(
        &mut pty,
        &fixture,
        "completion",
        &context_tui_filler("context-over-capacity", 70000),
        "over capacity observed",
    );
    assert!(context_tui_ctx_percent(pty.parser.screen()) >= 100);
    context_tui_assert_ctx_white(&pty);
    pty.input("\x03");
    pty.finish();
}


// 验证项 4（token 指标，双协议）：流式生成期间在 Ctx 左侧显示不带前缀的非零估算
// 速度；服务端权威 usage 到达后更新平均速度，In/CRead/CWrite 使用最近一次已完成
// 普通请求，Out 累计整个会话（input 仍含缓存命中总数，CRead/CWrite 只是其中的明细）；
// 下一轮缺失 usage 时最近请求指标退回未知而累计 out 保留，估算速度冻结；/new 清空全部新指标。
#[test]
fn context_tui_token_metrics_live_then_authoritative_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 40000, 80);
        // 初始 100×30 无任何数据时整行隐藏（含 Ctx）。
        pty.wait_for("初始无数据时整行隐藏", context_tui_metrics_hidden);

        pty.input("context-metrics-live\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = request.start_sse().unwrap();
        let first = format!("context-metrics-live-one {}", "x".repeat(600));
        streaming_delta(&mut stream, protocol, &first);
        pty.text("context-metrics-live-one");
        // SSE 仍被服务端握住：速度只能是估算；in 与缓存尚无完成的请求，out 按已接收字节增长。
        pty.wait_for("流式生成中显示非零估算速度", context_tui_positive_speed);
        let first_out = first.len().div_ceil(3);
        pty.wait_for("生成中的 out 按已接收字节估算", |screen| {
            let live = screen.contents();
            live.contains(&format!("\u{f04e2} —/{first_out}")) && live.contains("\u{f01bc} —/—")
        });
        let live = pty.parser.screen().contents();
        assert!(
            !live.contains("\u{f04e2} —/—"),
            "生成中的 out 不能继续显示未知：{live}"
        );
        let (speed_row, speed_column) =
            context_tui_locate(pty.parser.screen(), "tok/s").expect("流式生成中必须显示速率字段");
        let (ctx_row, ctx_column) =
            context_tui_locate(pty.parser.screen(), "\u{e62a} ").expect("Ctx 必须可见");
        assert_eq!(speed_row, ctx_row, "指标必须与 Ctx 同在输入栏上沿");
        assert!(speed_column < ctx_column, "速度必须位于 Ctx 左侧");

        let second = " context-metrics-live-two";
        streaming_delta(&mut stream, protocol, second);
        pty.text("context-metrics-live-two");
        assert!(
            context_tui_speed(pty.parser.screen()).is_some_and(|(_, marked)| !marked),
            "流式速度不得带 ~ 前缀"
        );
        // 第二个 delta 到达后估算必须增长，而不是停在首个分片的值。
        let grown_out = (first.len() + second.len()).div_ceil(3);
        assert!(grown_out > first_out);
        pty.wait_for("生成中的 out 随分片增长", |screen| {
            screen
                .contents()
                .contains(&format!("\u{f04e2} —/{grown_out}"))
        });

        context_tui_stream_usage(
            &mut stream,
            protocol,
            &format!("{first} context-metrics-live-two"),
        );
        stream.finish();
        pty.wait_for("权威 usage 更新完成态指标", |screen| {
            let contents = screen.contents();
            contents.contains("12K/300") && contents.contains("40/120")
        });
        pty.ready();
        let screen = pty.parser.screen();
        let (rate, marked) = context_tui_speed(screen).expect("完成后必须显示精确速度");
        assert!(!marked, "权威 usage 到达后不得保留 ~");
        assert!(rate > 0.0, "完成速度必须为正：{rate}");
        let (row, ctx_column) = context_tui_locate(screen, "\u{e62a} ").expect("Ctx 必须可见");
        let speed_column = context_tui_locate(screen, "tok/s").unwrap().1;
        assert!(speed_column < ctx_column);
        let mut previous = ctx_column;
        for (label, color) in [
            ("12K", vt100::Color::Rgb(125, 207, 255)),
            ("300", vt100::Color::Rgb(158, 206, 106)),
            ("40/120", vt100::Color::Rgb(187, 154, 247)),
            ("120", vt100::Color::Rgb(255, 158, 100)),
        ] {
            let (field_row, column) = context_tui_locate(screen, label).unwrap();
            assert_eq!(field_row, row);
            assert!(column > previous);
            previous = column;
            assert_eq!(screen.cell(row, column).unwrap().fgcolor(), color);
        }
        // 速度图标按 tok/s 分三档（映射由 pp-ui 单元测试覆盖），此处只验证它出现在状态栏且为粉色。
        let (speed_row, speed_icon_column) = ["\u{f0f86}", "\u{f0f85}", "\u{f04c5}"]
            .into_iter()
            .find_map(|icon| context_tui_locate(screen, icon))
            .expect("状态栏必须显示速度档位图标");
        assert_eq!(speed_row, row);
        assert_eq!(
            screen.cell(row, speed_icon_column).unwrap().fgcolor(),
            vt100::Color::Rgb(255, 95, 162)
        );
        for (icon, color) in [
            ("\u{f04e2}", vt100::Color::Rgb(125, 207, 255)),
            ("\u{f01bc}", vt100::Color::Rgb(187, 154, 247)),
            ("\u{e62a}", vt100::Color::Idx(15)),
            ("\u{f051b}", vt100::Color::Idx(15)),
        ] {
            let (icon_row, column) = context_tui_locate(screen, icon).unwrap();
            assert_eq!(icon_row, row);
            assert_eq!(screen.cell(row, column).unwrap().fgcolor(), color);
        }
        let (_, timer_column) = context_tui_locate(screen, "\u{f051b} ").unwrap();
        assert_eq!(
            screen.cell(row, timer_column + 2).unwrap().fgcolor(),
            vt100::Color::Idx(15)
        );

        // 下一轮：成功但没有 usage。计数退回未知，流式估算不带前缀并冻结。
        // 加宽以容纳请求临时状态，同时验证上一轮读写计数保留。
        pty.resize(30, 140);
        pty.input("context-metrics-missing\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = request.start_sse().unwrap();
        // 服务端暂不返回首个 delta：新请求等待期间必须保留上一笔完成速度。
        pty.wait_for(
            "下一请求等待首字节时速度不闪回未知",
            |screen| {
                screen.contents().contains("Requesting model")
                    && screen.contents().contains("40/120")
                    && context_tui_speed(screen)
                        .is_some_and(|(current, marked)| !marked && current == rate)
            },
        );
        streaming_delta(&mut stream, protocol, "context-metrics-missing-answer");
        pty.text("context-metrics-missing-answer");
        pty.wait_for(
            "缺失 usage 的请求也显示估算速度",
            context_tui_positive_speed,
        );
        streaming_complete(&mut stream, protocol, "context-metrics-missing-answer");
        stream.finish();
        pty.wait_for("缺失 usage 后最近请求指标退回未知而累计 out 保留", |screen| {
            let contents = screen.contents();
            contents.contains("\u{f04e2} —/300") && contents.contains("\u{f01bc} —/—")
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
        pty.wait_for("/new 清空全部新指标并整行隐藏", context_tui_metrics_hidden);
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

// 验证项 4（token 指标取消与重计）：流中 Esc 后速度停止，下一轮请求重新计数；
// 成功压缩清空最近请求指标但保留会话累计 out，摘要等待期间临时状态与既有指标
// 同处输入栏上沿且互不侵占。
#[test]
fn context_tui_token_metrics_cancel_stops_and_recounts() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 40000, 80);
        pty.wait_for("初始无数据时整行隐藏", context_tui_metrics_hidden);

        // 流式生成中 Esc：速度计时停止并从状态栏清除。
        pty.input("context-metrics-cancel\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = request.start_sse().unwrap();
        streaming_delta(&mut stream, protocol, "context-metrics-cancel-partial");
        pty.text("context-metrics-cancel-partial");
        pty.wait_for("取消前显示估算速度", context_tui_positive_speed);
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        let cancelled = pty.parser.screen().contents();
        assert!(
            cancelled.contains("— tok/s"),
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
        let mut stream = request.start_sse().unwrap();
        streaming_delta(&mut stream, protocol, "context-metrics-recount-partial");
        pty.text("context-metrics-recount-partial");
        pty.wait_for("新请求重新显示估算速度", context_tui_positive_speed);
        pty.input("\x1b");
        pty.wait_for("second task cancellation committed", |screen| {
            input_is_ready(screen)
                && screen.contents().contains("✻ Cancelled")
                && cancel_count_normalized(screen, CANCEL_DONE) == 1
        });
        drop(stream);
        pty.wait_for(
            "本轮取消清空速度而非上一轮取消提示",
            |screen| screen.contents().contains("— tok/s") && context_tui_speed(screen).is_none(),
        );

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
        let mut stream = request.start_sse().unwrap();
        streaming_delta(
            &mut stream,
            protocol,
            "context-metrics-authoritative-answer",
        );
        context_tui_stream_usage(
            &mut stream,
            protocol,
            "context-metrics-authoritative-answer",
        );
        stream.finish();
        pty.wait_for("普通请求完成并显示权威计数", |screen| {
            screen.contents().contains("12K/300") && screen.contents().contains("40/120")
        });
        pty.ready();

        // 已有权威计数时取消下一轮：最近成功的缓存读写量继续保留。
        pty.resize(30, 140);
        pty.input("context-metrics-retain-on-cancel\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut cancelled_stream = request.start_sse().unwrap();
        streaming_delta(
            &mut cancelled_stream,
            protocol,
            "context-metrics-retain-partial",
        );
        pty.text("context-metrics-retain-partial");
        pty.input("\x1b");
        pty.wait_for(
            "取消下一轮后保留最近成功的缓存读写量",
            |screen| {
                let contents = screen.contents();
                input_is_ready(screen)
                    && contents.contains("context-metrics-retain-partial")
                    && contents.contains("✻ Cancelled")
                    && contents.contains("— tok/s")
                    && contents.contains("40/120")
            },
        );
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
        let mut stream = request.start_sse().unwrap();
        pty.text("Compacting context...");
        pty.wait_for(
            "压缩等待期间保留既有指标与临时状态",
            |screen| {
                let contents = screen.contents();
                contents.contains("Compacting context...") && contents.contains("12K/300")
            },
        );

        // 加宽后读写量仍在，证明临时状态仅触发布局降级，没有清除计数。
        pty.resize(30, 140);
        pty.wait_for("宽屏压缩等待期间保留缓存读写量", |screen| {
            let contents = screen.contents();
            contents.contains("Compacting context...") && contents.contains("40/120")
        });
        let screen = pty.parser.screen();
        let (row, extra_column) = context_tui_locate(screen, "Compacting context...")
            .expect("压缩等待时必须显示临时状态");
        let (ctx_row, ctx_column) = context_tui_locate(screen, "\u{e62a} ").expect("Ctx 必须可见");
        let (metrics_row, metrics_column) =
            context_tui_locate(screen, "tok/s").expect("已有完成速率必须可见");
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
        pty.wait_for("成功压缩清空最近请求指标但保留累计 out", |screen| {
            let contents = screen.contents();
            contents.contains("— tok/s")
                && contents.contains("\u{f04e2} —/300")
                && contents.contains("\u{f01bc} —/—")
        });
        assert!(context_tui_locate(pty.parser.screen(), "\u{e62a} ").is_some());
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}
