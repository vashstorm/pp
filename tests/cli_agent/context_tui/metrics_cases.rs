// 验证项 4（token 指标，双协议）：流式生成期间在 Ctx 左侧显示不带前缀的非零估算
// 速度；服务端权威 usage 到达后更新平均速度，In/CRead/CWrite 使用最近一次已完成
// 普通请求，Out 累计整个会话（input 仍含缓存命中总数，CRead/CWrite 只是其中的明细）；
// 下一轮缺失 usage 时最近请求指标退回未知而累计 out 保留，估算速度冻结；/new 清空全部新指标。
#[test]
fn context_tui_token_metrics_live_then_authoritative_both_protocols() {
    let protocol = "response";
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

