use super::*;

fn live_activity_label(transcript: &Transcript) -> String {
    let line = transcript.live_activity_line().unwrap().to_string();
    line.strip_prefix(palette::WAITING_CLOCK_FRAMES)
        .unwrap()
        .strip_prefix(' ')
        .unwrap()
        .to_owned()
}

fn publish_pending_summary(transcript: &mut Transcript) {
    let activity = transcript.activity.as_ref().unwrap();
    if let Some((_, ready_at)) = activity.pending_summary.as_ref() {
        let now = activity
            .summary_updated_at
            .map(|updated| updated + Duration::from_secs(5))
            .unwrap_or(*ready_at)
            .max(*ready_at);
        transcript.tick(now);
    }
}

#[test]
fn activity_reasoning_summary_selection_headings_and_caps() {
    let mut transcript = Transcript::new();
    transcript.begin_run();
    assert_eq!(live_activity_label(&transcript), "Requesting model");
    for (text, expected) in [
        (" \n", "Thinking"),
        ("**检查", "Thinking"),
        ("状态**\n\n这里是摘要正文。", "检查状态"),
        ("\n正文继续更新。\n\n**修复", "检查状态"),
        ("状态**\n\n新的正文。", "修复状态"),
    ] {
        transcript.progress(AgentEvent::ReasoningSummaryDelta { text: text.into() });
        publish_pending_summary(&mut transcript);
        assert_eq!(live_activity_label(&transcript), expected);
    }
    transcript.progress(AgentEvent::ToolStarted {
        id: "read-summary".into(),
        name: "read".into(),
        arguments: r#"{"path":"test.txt"}"#.into(),
    });
    assert_eq!(live_activity_label(&transcript), "修复状态");
    transcript.progress(AgentEvent::ToolFinished {
        id: "read-summary".into(),
        result: json!({"ok":true,"content":"file content"}),
    });
    transcript.progress(AgentEvent::AssistantStarted);
    assert_eq!(live_activity_label(&transcript), "修复状态");
    for event in [
        AgentEvent::ThinkingDelta {
            text: "本次请求只有思考正文，没有摘要。".into(),
        },
        AgentEvent::ReasoningSummaryDelta { text: " \n".into() },
        AgentEvent::AssistantDelta {
            text: "继续检查".into(),
            refusal: false,
        },
        AgentEvent::AssistantFinished {
            text: "继续检查".into(),
        },
        AgentEvent::ToolStarted {
            id: "read-without-summary".into(),
            name: "read".into(),
            arguments: r#"{"path":"test.txt"}"#.into(),
        },
        AgentEvent::ToolFinished {
            id: "read-without-summary".into(),
            result: json!({"ok":true,"content":"file content"}),
        },
        AgentEvent::AssistantStarted,
    ] {
        transcript.progress(event);
        assert_eq!(live_activity_label(&transcript), "修复状态");
    }
    transcript.progress(AgentEvent::ReasoningSummaryDelta {
        text: "确认修复结果".into(),
    });
    publish_pending_summary(&mut transcript);
    assert_eq!(live_activity_label(&transcript), "确认修复结果");
    transcript.progress(AgentEvent::AssistantDelta {
        text: "修复完成".into(),
        refusal: false,
    });
    assert_eq!(live_activity_label(&transcript), "确认修复结果");
    transcript.finish_run_with_timing("Completed", None, None);
    assert!(transcript.live_activity_line().is_none());
    assert!(screen(&render(&mut transcript, 100, 30)).contains("✻ Completed · —"));
    transcript.begin_run();
    assert_eq!(live_activity_label(&transcript), "Requesting model");

    let mut headings = Transcript::new();
    headings.begin_run();
    for (text, expected) in [
        ("检查", "检查"),
        ("输入🙂\n\n", "检查输入🙂"),
        ("## 验证结果\n正文不替换标题。", "验证结果"),
    ] {
        headings.progress(AgentEvent::ReasoningSummaryDelta { text: text.into() });
        publish_pending_summary(&mut headings);
        assert_eq!(live_activity_label(&headings), expected);
    }
    headings.progress(AgentEvent::ReasoningSummaryDelta {
        text: format!("{}\n\n**完成验证**\n", "中🙂".repeat(4096)),
    });
    publish_pending_summary(&mut headings);
    assert_eq!(live_activity_label(&headings), "完成验证");
    assert!(headings.activity.as_ref().unwrap().summary_line.len() <= 1024);
    headings.fail();
    assert!(headings.live_activity_line().is_none());
    assert!(screen(&render(&mut headings, 100, 30)).contains("✻ Failed ·"));

    for text in [
        "a".repeat(100),
        "检查".repeat(50),
        "e\u{301}".repeat(100),
        "👩‍💻".repeat(50),
    ] {
        let mut capped = Transcript::new();
        capped.begin_run();
        let body = format!("**{text}**\n正文仍完整保留。\n");
        capped.progress(AgentEvent::ReasoningSummaryDelta { text: body.clone() });
        let label = live_activity_label(&capped);
        assert!(Line::raw(&label).width() <= 60, "{label}");
        let prefix = label.strip_suffix('…').unwrap();
        assert!(!prefix.is_empty());
        assert!(text.starts_with(prefix));
        assert!(!prefix.ends_with('\u{200d}'));
        if text.starts_with('e') {
            assert!(prefix.ends_with('\u{301}'));
        }
        assert_eq!(capped.entries.last().unwrap().text, body);
    }
    for text in ["a".repeat(60), "检".repeat(30)] {
        let mut exact = Transcript::new();
        exact.begin_run();
        exact.progress(AgentEvent::ReasoningSummaryDelta {
            text: format!("**{text}**\n"),
        });
        assert_eq!(live_activity_label(&exact), text);
    }
}

#[test]
fn activity_summary_throttling_settling_and_stop_dropping() {
    let started = Instant::now();
    let mut transcript = Transcript::new();
    transcript.activity = Some(Activity::new(started));
    transcript
        .activity
        .as_mut()
        .unwrap()
        .push_reasoning_summary("**检查状态**\n", started);
    assert_eq!(live_activity_label(&transcript), "检查状态");
    for second in 1..5 {
        let now = started + Duration::from_secs(second);
        transcript
            .activity
            .as_mut()
            .unwrap()
            .push_reasoning_summary(&format!("**分析步骤 {second}**\n"), now);
        transcript.tick(now);
        assert_eq!(live_activity_label(&transcript), "检查状态");
    }
    transcript.tick(started + Duration::from_millis(4999));
    assert_eq!(live_activity_label(&transcript), "检查状态");
    assert!(transcript.tick(started + Duration::from_secs(5)));
    assert_eq!(live_activity_label(&transcript), "分析步骤 4");
    transcript.tick(started + Duration::from_secs(20));
    assert_eq!(live_activity_label(&transcript), "分析步骤 4");

    for prefix in ["", "## "] {
        let mut partial = Activity::new(started);
        partial.push_reasoning_summary(&format!("{prefix}检查"), started);
        partial.tick(started + Duration::from_millis(400));
        assert!(partial.summary_label.is_none());
        partial.push_reasoning_summary("输入🙂", started + Duration::from_millis(400));
        partial.tick(started + Duration::from_millis(899));
        assert!(partial.summary_label.is_none());
        assert!(partial.tick(started + Duration::from_millis(900)));
        assert_eq!(partial.summary_label.as_deref(), Some("检查输入🙂"));

        let mut completed = Activity::new(started);
        completed.push_reasoning_summary(&format!("{prefix}检查输入🙂\n"), started);
        assert_eq!(completed.summary_label.as_deref(), Some("检查输入🙂"));
    }

    let mut stopped = Activity::new(started);
    stopped.push_reasoning_summary("**检查状态**\n", started);
    stopped.push_reasoning_summary("**检查状态**\n", started + Duration::from_secs(4));
    stopped.push_reasoning_summary("**完成检查**\n", started + Duration::from_secs(5));
    assert_eq!(stopped.summary_label.as_deref(), Some("完成检查"));
    stopped.push_reasoning_summary("**延迟摘要**\n", started + Duration::from_secs(6));
    stopped.stop("Completed");
    assert!(!stopped.tick(started + Duration::from_secs(20)));
    assert_eq!(stopped.summary_label.as_deref(), Some("完成检查"));

    let mut cancelled = Activity::new(started);
    cancelled.push_reasoning_summary("**检查状态**\n", started);
    cancelled.push_reasoning_summary("**延迟摘要**\n", started + Duration::from_secs(1));
    cancelled.phase("Cancelling");
    cancelled.tick(started + Duration::from_secs(5));
    cancelled.push_reasoning_summary("**取消后摘要**\n", started + Duration::from_secs(6));
    assert_eq!(cancelled.summary_label.as_deref(), Some("检查状态"));
}

#[test]
fn activity_waiting_duration_and_failure_timers() {
    let mut transcript = Transcript::new();
    let started = Instant::now() - Duration::from_secs(85);
    transcript.activity = Some(Activity::new(started));
    transcript.tick(started + Duration::from_secs(85));
    assert_eq!(live_activity_label(&transcript), "Requesting model");
    assert_eq!(transcript.task_duration().as_deref(), Some("1m 25s"));

    for id in ["ask-1", "ask-2"] {
        transcript.progress(AgentEvent::ToolStarted {
            id: id.into(),
            name: "ask".into(),
            arguments: r#"{"questions":[{"id":"q","question":"Question","options":[]}]}"#.into(),
        });
        transcript.waiting_for_input();
        let waiting = transcript
            .activity
            .as_ref()
            .unwrap()
            .waiting_started
            .unwrap();
        assert_eq!(live_activity_label(&transcript), "Waiting for input · 0s");
        transcript.tick(waiting + Duration::from_millis(999));
        assert_eq!(live_activity_label(&transcript), "Waiting for input · 0s");
        transcript.tick(waiting + Duration::from_secs(85));
        assert_eq!(
            live_activity_label(&transcript),
            "Waiting for input · 1m 25s"
        );
        transcript.progress(AgentEvent::ToolFinished {
            id: id.into(),
            result: json!({"ok":true,"answers":[{"id":"q","selected_options":[],"custom_input":"yes"}]}),
        });
        assert!(
            transcript
                .activity
                .as_ref()
                .unwrap()
                .waiting_started
                .is_none()
        );
        transcript.tick(started + Duration::from_secs(85));
        assert_eq!(live_activity_label(&transcript), "Processing tool result");
        assert_eq!(transcript.task_duration().as_deref(), Some("1m 25s"));
        assert_eq!(transcript.activity.as_ref().unwrap().started, Some(started));
    }
    transcript.waiting_for_input();
    transcript.cancelling();
    transcript.waiting_for_input();
    transcript.tick(started + Duration::from_secs(85));
    assert_eq!(live_activity_label(&transcript), "Cancelling");
    assert_eq!(transcript.task_duration().as_deref(), Some("1m 25s"));
    transcript.cancel();
    assert!(!transcript.tick(Instant::now() + Duration::from_secs(3600)));

    let started = Instant::now();
    let mut transcript = Transcript::new();
    transcript.activity = Some(Activity::new(started));
    for (seconds, expected) in [
        (0, "0s"),
        (59, "59s"),
        (60, "1m 0s"),
        (85, "1m 25s"),
        (3599, "59m 59s"),
        (3600, "1h 0m 0s"),
        (3725, "1h 2m 5s"),
        (86400, "24h 0m 0s"),
    ] {
        transcript.tick(started + Duration::from_secs(seconds));
        assert_eq!(transcript.task_duration().as_deref(), Some(expected));
    }
    transcript.finish_run_with_timing("Completed", None, None);
    assert!(transcript.task_duration().is_none());

    let mut transcript = Transcript::new();
    transcript.append("You", "waiting task");
    transcript.begin_run();
    let started = transcript.activity.as_ref().unwrap().started.unwrap();
    assert!(!transcript.tick(started + Duration::from_millis(79)));
    assert!(transcript.tick(started + Duration::from_millis(80)));
    assert!(transcript.tick(started + Duration::from_secs(1)));
    assert!(!transcript.tick(started + Duration::from_secs(1)));
    assert_eq!(live_activity_label(&transcript), "Requesting model");
    assert_eq!(transcript.task_duration().as_deref(), Some("1s"));
    assert_eq!(transcript.entries.len(), 1);
    transcript.fail();
    let stopped_duration = transcript.task_duration();
    assert!(stopped_duration.is_some());
    assert!(!transcript.tick(started + Duration::from_secs(90)));
    assert_eq!(transcript.task_duration(), stopped_duration);
    assert!(transcript.live_activity_line().is_none());
    assert!(screen(&render(&mut transcript, 100, 30)).contains("✻ Failed ·"));
}

#[test]
fn live_activity_phases_and_sweep_rendering() {
    let mut transcript = Transcript::new();
    transcript.append("You", "run task");
    transcript.begin_run();
    let phases = [
        (AgentEvent::AssistantStarted, "Requesting model"),
        (
            AgentEvent::ThinkingDelta {
                text: "thinking content".into(),
            },
            "Thinking",
        ),
        (
            AgentEvent::AssistantDelta {
                text: "answer content".into(),
                refusal: false,
            },
            "Responding",
        ),
        (
            AgentEvent::AssistantFinished {
                text: "answer content".into(),
            },
            "Processing response",
        ),
        (
            AgentEvent::ToolStarted {
                id: "read-1".into(),
                name: "read".into(),
                arguments: r#"{"path":"test.txt"}"#.into(),
            },
            "Running Read",
        ),
        (
            AgentEvent::ToolFinished {
                id: "read-1".into(),
                result: json!({"ok":true,"content":"tool content"}),
            },
            "Processing tool result",
        ),
        (AgentEvent::AssistantStarted, "Requesting model"),
    ];
    for (event, label) in phases {
        transcript.progress(event);
        let started = transcript.activity.as_ref().unwrap().started.unwrap();
        transcript.tick(started + Duration::from_secs(5));
        let text = screen(&render(&mut transcript, 100, 30));
        assert_eq!(live_activity_label(&transcript), label);
        assert_eq!(transcript.task_duration().as_deref(), Some("5s"));
        assert!(!text.contains(label), "{text}");
    }
    transcript.cancelling();
    transcript.progress(AgentEvent::ThinkingDelta {
        text: "late delta".into(),
    });
    assert_eq!(live_activity_label(&transcript), "Cancelling");
    transcript.cancel();
    assert!(transcript.live_activity_line().is_none());
    assert!(screen(&render(&mut transcript, 100, 30)).contains("✻ Cancelled ·"));
    transcript.begin_run();
    assert_eq!(live_activity_label(&transcript), "Requesting model");
    assert_eq!(transcript.task_duration().as_deref(), Some("0s"));

    let started = Instant::now();
    let mut transcript = Transcript::new();
    transcript.activity = Some(Activity::new(started));
    let render_activity = |transcript: &Transcript, width| {
        let area = Rect::new(2, 0, width, 1);
        let mut buffer = Buffer::empty(area);
        transcript
            .live_activity_line()
            .unwrap()
            .render(area, &mut buffer);
        buffer
    };
    let initial = render_activity(&transcript, 100);
    assert_eq!(initial[(2, 0)].symbol(), "\u{f1456}");
    assert_eq!(initial[(4, 0)].fg, palette::accent());
    assert_eq!(initial[(19, 0)].fg, palette::dim());
    assert!(initial[(2, 0)].modifier.contains(Modifier::BOLD));
    for index in 0..16_u16 {
        transcript.tick(started + Duration::from_millis(u64::from(index) * 80));
        let buffer = render_activity(&transcript, 100);
        assert_eq!(buffer[(4, 0)].symbol(), "R");
        assert_eq!(buffer[(4 + index, 0)].fg, palette::accent());
    }
    transcript.tick(started + Duration::from_millis(24 * 80));
    assert_eq!(
        render_activity(&transcript, 100)[(4, 0)].fg,
        palette::accent()
    );
    assert_eq!(
        screen(&render_activity(&transcript, 12)),
        "\u{f144e} Requesting"
    );
    for (ms, clock) in [
        (0, "\u{f1456}"),
        (120, "\u{f144b}"),
        (360, "\u{f144d}"),
        (720, "\u{f1450}"),
        (1080, "\u{f1453}"),
        (1320, "\u{f1455}"),
        (1440, "\u{f1456}"),
    ] {
        transcript.tick(started + Duration::from_millis(ms));
        let buffer = render_activity(&transcript, 100);
        assert_eq!(buffer[(2, 0)].symbol(), clock);
        assert_eq!(buffer[(3, 0)].symbol(), " ");
        assert_eq!(buffer[(4, 0)].symbol(), "R");
    }
    let activity = transcript.activity.as_mut().unwrap();
    activity.phase("读取 e\u{301} 中😀");
    assert_eq!(live_activity_label(&transcript), "读取 e\u{301} 中😀");
    transcript.cancel();
    assert!(!transcript.tick(started + Duration::from_secs(90)));
}
