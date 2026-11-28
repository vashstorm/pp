// Coverage 4 (token metrics, both protocols): while streaming, show a non-zero estimated speed without a prefix to the
// left of Ctx; once the server's authoritative usage arrives, update the average speed; In/CRead/CWrite use the latest
// completed ordinary request while Out accumulates the whole session (input still carries the cache-hit total, of which
// CRead/CWrite are the breakdown); when the next round has no usage, the latest-request metrics fall back to unknown with
// the cumulative out preserved and the estimated speed frozen; /new clears every new metric.
#[test]
fn context_tui_token_metrics_live_then_authoritative_both_protocols() {
    let protocol = "response";
        let fixture = server();
        let (_home, mut pty) = context_tui_start(&fixture, protocol, 40000, 80);
        // Initially at 100×30 with no data at all the whole row is hidden, Ctx included.
        pty.wait_for("the whole row is hidden while there is no data", context_tui_metrics_hidden);

        pty.input("context-metrics-live\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = request.start_sse().unwrap();
        let first = format!("context-metrics-live-one {}", "x".repeat(600));
        streaming_delta(&mut stream, protocol, &first);
        pty.text("context-metrics-live-one");
        // The SSE is still held by the server: the speed can only be an estimate; in and the cache counters have no completed
        // request yet, so out grows with the received bytes.
        pty.wait_for("streaming shows a non-zero estimated speed", context_tui_positive_speed);
        let first_out = first.len().div_ceil(3);
        pty.wait_for("live out is estimated from the received bytes", |screen| {
            let live = screen.contents();
            live.contains(&format!("\u{f04e2} —/{first_out}")) && live.contains("\u{f01bc} —/—")
        });
        let live = pty.parser.screen().contents();
        assert!(
            !live.contains("\u{f04e2} —/—"),
            "live out must stop showing unknown: {live}"
        );
        let (speed_row, speed_column) =
            context_tui_locate(pty.parser.screen(), "tok/s").expect("the rate field must be visible while streaming");
        let (ctx_row, ctx_column) =
            context_tui_locate(pty.parser.screen(), "\u{e62a} ").expect("Ctx must be visible");
        assert_eq!(speed_row, ctx_row, "the metrics must share the top row of the input bar with Ctx");
        assert!(speed_column < ctx_column, "the speed must sit to the left of Ctx");

        let second = " context-metrics-live-two";
        streaming_delta(&mut stream, protocol, second);
        pty.text("context-metrics-live-two");
        assert!(
            context_tui_speed(pty.parser.screen()).is_some_and(|(_, marked)| !marked),
            "a streaming speed must not carry the ~ prefix"
        );
        // Once the second delta arrives the estimate must grow instead of sticking to the first chunk's value.
        let grown_out = (first.len() + second.len()).div_ceil(3);
        assert!(grown_out > first_out);
        pty.wait_for("live out grows with the chunks", |screen| {
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
        pty.wait_for("authoritative usage updates the completed metrics", |screen| {
            let contents = screen.contents();
            contents.contains("12K/300") && contents.contains("40/120")
        });
        pty.ready();
        let screen = pty.parser.screen();
        let (rate, marked) = context_tui_speed(screen).expect("an exact speed must be shown after completion");
        assert!(!marked, "the ~ must not survive once authoritative usage arrives");
        assert!(rate > 0.0, "the completed speed must be positive: {rate}");
        let (row, ctx_column) = context_tui_locate(screen, "\u{e62a} ").expect("Ctx must be visible");
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
        // The speed icon has three tiers by tok/s (the mapping is covered by pp-ui unit tests); here only verify it shows in the status bar in pink.
        let (speed_row, speed_icon_column) = ["\u{f0f86}", "\u{f0f85}", "\u{f04c5}"]
            .into_iter()
            .find_map(|icon| context_tui_locate(screen, icon))
            .expect("the status bar must show a speed tier icon");
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

        // Next round: successful but without usage. The counters fall back to unknown and the streaming estimate is unprefixed and frozen.
        // Widen to fit the request's transient state and confirm the read/write counters of the previous round are kept.
        pty.resize(30, 140);
        pty.input("context-metrics-missing\r");
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        let mut stream = request.start_sse().unwrap();
        // The server delays the first delta: the previous completed speed must survive while the new request waits.
        pty.wait_for(
            "the speed does not flash back to unknown while waiting for the first byte",
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
            "a request without usage still shows an estimated speed",
            context_tui_positive_speed,
        );
        streaming_complete(&mut stream, protocol, "context-metrics-missing-answer");
        stream.finish();
        pty.wait_for("after missing usage the latest-request metrics fall back to unknown while cumulative out is kept", |screen| {
            let contents = screen.contents();
            contents.contains("\u{f04e2} —/300") && contents.contains("\u{f01bc} —/—")
        });
        let (rate, marked) =
            context_tui_speed(pty.parser.screen()).expect("the streamed bytes must keep the final estimated speed");
        assert!(
            !marked && rate > 0.0,
            "without usage the speed also carries no ~ prefix: {rate}"
        );

        pty.ready();
        pty.resize(30, 100);
        pty.input("/new\r");
        pty.wait_for("/new clears every new metric and hides the whole row", context_tui_metrics_hidden);
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
}

