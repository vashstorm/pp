// 临时管道阶段面板的 TUI 回归：真实 `pp` + portable-pty + 内嵌 shell。
// 本地 `!` 命令不产生模型请求；断言只使用可见屏幕、门闩文件与真实终态。
// 辅助项使用 `pipes_` 前缀，测试使用 `pipeline_` 前缀，避免与兄弟 include 冲突。

const PIPES_F8: &str = "\x1b[19~";
const PIPES_PGUP: &str = "\x1b[5~";
const PIPES_END: &str = "\x1b[4~";

#[test]
fn pipeline_llm_bash_card_streams_without_newlines_both_protocols() {
    for protocol in ["completion", "response"] {
        let provider = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("typing-pipeline.sh"), r#"
printf '\033[31m中'
while [ ! -f first ]; do sleep 0.01; done
printf '文\033[0m'
while [ ! -f second ]; do sleep 0.01; done
printf '\n'
for i in {0..34}; do printf 'final-%02d\n' "$i"; done
"#).unwrap();
        let mut pty = ask_tui_start(&provider, home.path(), project.path(), protocol);
        pty.input("stream pipeline\r");
        let (request, _) = pty.request_at(&provider, api_path(protocol));
        let args = json!({"command":"bash typing-pipeline.sh 2>&1 | cat | tail -30"});
        let tool_call = if protocol == "completion" {
            call("typing", "bash", args)
        } else {
            response_call("typing", "bash", args)
        };
        ask_reply_calls(request, protocol, json!([tool_call]));
        pty.text("中");
        assert!(!project.path().join("first").exists());
        assert!(!pty.contents().contains("中文"));
        std::fs::write(project.path().join("first"), "").unwrap();
        pty.text("中文");
        let live = pty.contents();
        assert_eq!(live.matches("中文").count(), 1, "{protocol}: {live}");
        assert!(!live.contains('�'), "{protocol}: {live}");
        assert!(!project.path().join("second").exists());
        std::fs::write(project.path().join("second"), "").unwrap();
        let (request, body) = pty.request_at(&provider, api_path(protocol));
        let outputs = cancel_tool_outputs(&body, protocol);
        let result = &outputs.last().unwrap().1;
        assert_eq!(result["ok"], true, "{result}");
        let output = result["output"].as_str().unwrap();
        assert!(output.starts_with("final-05\n") && output.ends_with("final-34\n"), "{result}");
        assert!(!output.contains("中文"), "阶段预览不能进入工具结果");
        pty.text("final-34");
        assert!(!pty.contents().contains("中文"));
        ask_final_reply(request, protocol, "pipeline done");
        pty.text("pipeline done");
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&provider);
    }
}

fn pipes_open(pty: &mut Pty) {
    pty.input(PIPES_F8);
    pty.wait_for("管道阶段面板打开", |screen| {
        screen.alternate_screen() && screen.contents().contains("Pipe streams")
    });
}

fn pipes_close(pty: &mut Pty) {
    pty.input("\x1b");
    pty.wait_for("管道阶段面板关闭并恢复主屏", |screen| {
        !screen.alternate_screen()
    });
}

/// 可见正文中的 `live-NN` 行号；命令回显只包含 `live-%02d`，不会匹配数字。
fn pipes_live_numbers(screen: &vt100::Screen) -> Vec<u32> {
    pipes_numbers(screen, "live-")
}

fn pipes_numbers(screen: &vt100::Screen, prefix: &str) -> Vec<u32> {
    let text = screen.contents();
    let mut numbers = Vec::new();
    for line in text.lines() {
        let mut rest = line;
        while let Some(index) = rest.find(prefix) {
            rest = &rest[index + prefix.len()..];
            let digits: String = rest
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if !digits.is_empty()
                && let Ok(value) = digits.parse()
            {
                numbers.push(value);
            }
        }
    }
    numbers
}

fn pipes_max_live(screen: &vt100::Screen) -> Option<u32> {
    pipes_live_numbers(screen).into_iter().max()
}

/// 面板 header 中 32 位小写十六进制的 run ID。
fn pipes_run_id(pty: &mut Pty) -> String {
    let text = pty.contents();
    text.split(|character: char| !character.is_ascii_hexdigit())
        .find(|token| token.len() == 32 && token.chars().all(|c| c.is_ascii_hexdigit()))
        .unwrap_or_else(|| panic!("阶段面板未显示 run ID：{text}"))
        .to_owned()
}

#[test]
fn pipeline_bang_shell_footer_warning_and_f8_stage_body() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = bang_shell_start(&provider, home.path(), project.path(), "completion", "");

    // 命令回显用变量拼接，确保 hidden/visible 只可能来自真实阶段输出。
    pty.input("!{ A=hid; B=den; C=vis; D=ible; printf '%s%s\\n%s%s\\n' \"$A\" \"$B\" \"$C\" \"$D\"; exit 7; } | tail -n 1\r");
    pty.wait_for("主输出只有末阶段 visible 且保留非零阶段警示", |screen| {
        let text = screen.contents();
        text.contains("visible")
            && !text.contains("hidden")
            && text.contains("Non-zero stage results: 1")
            && text.contains("Pipes: /pipes (F8)")
    });
    bang_shell_wait_footer(&mut pty, "shell 原始状态仍为成功", "Timeout:");
    // 最终 0 不得掩盖上游 7：卡片 footer 仍带警示。
    pty.wait_for("折叠卡片保留阶段警示", |screen| {
        screen
            .contents()
            .lines()
            .rfind(|line| line.contains("╰") && line.contains("Wall:"))
            .is_some_and(|line| line.contains("Non-zero stage results: 1"))
    });

    // F8 打开第一条边，看到被 tail 截掉的隐藏正文，主输出不被替换。
    pipes_open(&mut pty);
    pty.text("hidden");
    pty.wait_for("面板显示保留区间与已关闭状态", |screen| {
        let text = screen.contents();
        text.contains("output · kept 0..15 / 15 bytes") && text.contains("closed")
    });
    pipes_close(&mut pty);
    pty.wait_for("关闭面板后主卡片仍在", |screen| {
        let text = screen.contents();
        text.contains("visible") && text.contains("Non-zero stage results: 1")
    });
    // Esc 只关闭面板，输入区恢复且可继续编辑。
    pty.input("pipes-draft");
    pty.draft("pipes-draft");

    cancel_assert_no_request(&provider);
    assert!(
        !bang_shell_sessions_dir(home.path()).exists(),
        "本地 shell 不得创建会话"
    );
    pty.input("\x03");
    pty.finish();
    no_more(&provider);
}

#[test]
fn pipeline_bang_shell_live_stage_before_final_tail_output() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let gate = home.path().join("pipe-gate");
    let mut pty = bang_shell_start(&provider, home.path(), project.path(), "completion", "");

    let command = format!(
        "!{{ A=AL; B=PHA; printf '%s%s\\n' \"$A\" \"$B\"; while [ ! -f '{}' ]; do sleep 0.05; done; C=OME; D=GA; printf '%s%s\\n' \"$C\" \"$D\"; }} | tail -n 1\r",
        gate.display()
    );
    pty.input(&command);
    pty.text("Elapsed:");
    pty.wait_for("门闩未释放时卡片显示上游实时输出", |screen| {
        let text = screen.contents();
        text.contains("Elapsed:") && text.contains("ALPHA") && !text.contains("OMEGA")
    });

    // /pipes 在 RunningShell 中仅本地面板：不进入 steering、不发送模型请求。
    pty.input("/pipes\r");
    pty.wait_for("/pipes 在运行中显示已流过的阶段正文", |screen| {
        screen.alternate_screen() && screen.contents().contains("ALPHA")
    });
    pipes_close(&mut pty);
    // F8 与命令入口等价，也允许运行中打开。
    pipes_open(&mut pty);
    pty.text("ALPHA");
    pipes_close(&mut pty);
    pty.wait_for("关闭面板后卡片仍保留实时预览", |screen| {
        let text = screen.contents();
        !screen.alternate_screen()
            && text.contains("Elapsed:")
            && text.contains("ALPHA")
            && !text.contains("OMEGA")
    });
    cancel_assert_no_request(&provider);
    assert!(!gate.exists(), "门闩只能由测试显式释放");

    std::fs::write(&gate, "").unwrap();
    pty.wait_for("释放门闩后 tail 输出最终行", |screen| {
        let text = screen.contents();
        text.contains("OMEGA") && text.contains("Pipes: /pipes (F8)")
    });
    bang_shell_wait_footer(&mut pty, "门闩命令以成功终态结束", "Timeout:");

    // 已完成 run 仍可浏览完整阶段正文（ready 与 done）。
    pipes_open(&mut pty);
    pty.text("ALPHA");
    pty.text("OMEGA");
    pty.wait_for("已关闭流仍显示 closed", |screen| {
        screen.contents().contains("closed")
    });
    pipes_close(&mut pty);
    cancel_assert_no_request(&provider);
    assert!(
        !bang_shell_sessions_dir(home.path()).exists(),
        "本地 shell 不得创建会话"
    );
    pty.input("\x03");
    pty.finish();
    no_more(&provider);
}

#[test]
fn pipeline_pipes_panel_switches_run_stream_freezes_and_restores_draft() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = bang_shell_start(&provider, home.path(), project.path(), "completion", "");

    // 较早完成的 3 阶段 run：两条边正文不同，用于左右/上下切换。
    pty.input("!printf 'first-%s\n' edge | tr 'a-z' 'A-Z' | cat\r");
    pty.wait_for("较早的三阶段管道 run 完成", |screen| {
        let text = screen.contents();
        text.contains("FIRST-EDGE") && text.contains("Pipes: /pipes (F8)")
    });
    assert!(
        !pty.contents().contains("Non-zero stage results"),
        "全部阶段成功时不得出现警示"
    );

    // 运行中的三阶段管道：复合阶段与下游并发，两条边都能在运行中看到字节。
    pty.input("!i=0; while :; do i=$((i+1)); printf 'live-%02d\\n' \"$i\"; sleep 0.02; done | cat | tail -n 1\r");
    pty.text("Elapsed:");
    pty.input("pipes-draft");
    pty.draft("pipes-draft");
    pipes_open(&mut pty);

    // 默认选择最近的运行中 run（1/2）与它唯一的边。
    pty.wait_for("默认显示最近的管道 run 与第一条边", |screen| {
        let text = screen.contents();
        text.contains("run 1/2")
            && text.contains("follow")
            && !pipes_live_numbers(screen).is_empty()
    });
    assert!(pty.contents().contains("› s1"), "选中流必须有可见标记");
    // 复合阶段后面的边也必须在运行中被镜像，而不是等到 EOF。
    pty.wait_for("第二条边在运行中收到字节", |screen| {
        screen
            .contents()
            .lines()
            .any(|line| line.contains("s2 ") && !line.contains("kept 0..0 / 0 "))
    });
    pty.input("\x1b[B");
    pty.wait_for("Down 选中运行中 run 的第二条边", |screen| {
        let text = screen.contents();
        text.lines().any(|line| line.contains("› s2"))
            && !pipes_live_numbers(screen).is_empty()
    });
    pty.input("\x1b[A");
    pty.wait_for("Up 回到运行中 run 的第一条边", |screen| {
        let text = screen.contents();
        text.lines().any(|line| line.contains("› s1"))
            && !pipes_live_numbers(screen).is_empty()
    });

    // Right/Left 在两个 run 间切换。
    pty.input("\x1b[C");
    pty.wait_for("Right 切到较早完成的三阶段 run", |screen| {
        let text = screen.contents();
        text.contains("run 2/2") && text.contains("first-edge")
    });
    // Down/Up 在该 run 的相邻边间切换：第二条边是 tr 转换后的字节。
    pty.input("\x1b[B");
    pty.wait_for("Down 选中第二条边并显示转换后的字节", |screen| {
        let text = screen.contents();
        text.lines().any(|line| line.contains("› s2"))
            && text.contains("FIRST-EDGE")
            && !text.lines().any(|line| line.trim() == "first-edge")
    });
    pty.input("\x1b[A");
    pty.wait_for("Up 回到第一条边", |screen| {
        let text = screen.contents();
        text.lines().any(|line| line.contains("› s1"))
            && text.lines().any(|line| line.trim() == "first-edge")
    });
    pty.input("\x1b[D");
    pty.wait_for("Left 回到最近的运行中 run", |screen| {
        screen.contents().contains("run 1/2")
    });

    // PgUp 冻结：新字节继续到达，但 viewport 不回到尾部。
    pty.wait_for("第一条边已产生足够行以冻结滚动", |screen| {
        pipes_max_live(screen).is_some_and(|last| last >= 30)
    });
    pty.input(PIPES_PGUP);
    pty.wait_for("PgUp 冻结正文", |screen| screen.contents().contains("frozen"));
    let frozen = pipes_live_numbers(pty.parser.screen());
    assert!(!frozen.is_empty());
    // 规范允许两种冻结提示：后台有新字节，或旧缓存已被覆盖。
    pty.wait_for("冻结期间提示后台新字节或旧缓存已覆盖", |screen| {
        let text = screen.contents();
        text.contains("new +") || text.contains("cached bytes overwritten")
    });
    assert_eq!(
        pipes_live_numbers(pty.parser.screen()),
        frozen,
        "冻结后新输出不得把 viewport 拉回底部"
    );

    // resize 保留选中流与冻结锚点。
    pty.resize(30, 80);
    pty.wait_for("resize 后保持冻结与 run 选择", |screen| {
        screen.alternate_screen()
            && screen.contents().contains("run 1/2")
            && screen.contents().contains("frozen")
    });
    assert_eq!(
        pipes_live_numbers(pty.parser.screen()),
        frozen,
        "resize 不得改变冻结锚点"
    );

    // End 恢复跟随，并显示包含新字节的尾部。
    let frozen_last = *frozen.last().unwrap();
    pty.input(PIPES_END);
    pty.wait_for("End 恢复跟随并前进到新输出", |screen| {
        let text = screen.contents();
        text.contains("follow")
            && pipes_live_numbers(screen)
                .last()
                .is_some_and(|last| *last > frozen_last)
    });

    // Esc 只关闭面板：shell 仍在运行、草稿保留，不触发取消。
    pipes_close(&mut pty);
    pty.wait_for("Esc 关闭面板但不取消 shell", |screen| {
        let text = screen.contents();
        !screen.alternate_screen()
            && text.contains("Elapsed:")
            && !text.contains("Cancelling shell")
    });
    pty.draft("pipes-draft");

    // 第二次 Esc 才取消 shell，作为收尾。
    pty.input("\x1b");
    pty.text("Cancelling shell...");
    bang_shell_wait_done(&mut pty);
    cancel_assert_no_scratch(project.path());
    cancel_assert_no_request(&provider);
    assert!(
        !bang_shell_sessions_dir(home.path()).exists(),
        "本地 shell 不得创建会话"
    );
    pty.input("\x03");
    pty.finish();
    no_more(&provider);
}

#[test]
fn pipeline_pipes_root_change_invalidates_old_run_id() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = bang_shell_start(&provider, home.path(), project.path(), "completion", "");

    pty.input("!printf 'old-%s\\n' edge | cat\r");
    pty.wait_for("旧根上的管道 run 完成", |screen| {
        let text = screen.contents();
        text.contains("old-edge") && text.contains("Pipes: /pipes (F8)")
    });
    pipes_open(&mut pty);
    let old_run = pipes_run_id(&mut pty);
    pipes_close(&mut pty);

    // /new 换根：TUI 重绑新 store，旧 run 的 URI 不再存在。
    pty.input("/new\r");
    pty.wait_for("换根后回到欢迎页", |screen| {
        let text = screen.contents();
        !text.contains("old-edge") && text.contains("Keyboard")
    });
    pty.input(&format!("/pipes {old_run}\r"));
    pty.wait_for("旧根的 run 明确不可用而不是空正文", |screen| {
        let text = screen.contents();
        text.contains("Pipe streams")
            && text.contains(&old_run)
            && text.contains("unavailable in this session")
    });
    pipes_close(&mut pty);

    // 新根没有任何管道历史；F8 显示空态而不是旧内容。
    pipes_open(&mut pty);
    pty.wait_for("新根显示空管道列表", |screen| {
        screen.contents().contains("No pipe runs in this session.")
    });
    pipes_close(&mut pty);
    assert!(!pty.contents().contains("old-edge"));

    cancel_assert_no_request(&provider);
    pty.input("\x03");
    pty.finish();
    no_more(&provider);
}

#[test]
fn pipeline_f8_does_not_override_ask_modal() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let mut pty = ask_tui_start(&provider, home.path(), project.path(), "completion");
    pty.ready();
    pty.input("ask pipes precedence\r");
    let (request, _) = pty.request_at(&provider, api_path("completion"));
    ask_reply_calls(
        request,
        "completion",
        json!([ask_call_json(
            "completion",
            "ask-pipes",
            ask_single_arguments()
        )]),
    );
    ask_wait_panel(&mut pty, "Which database should the project use?");

    pty.input(PIPES_F8);
    pty.wait_for("F8 不得覆盖 Ask 模态", |screen| {
        let text = screen.contents();
        text.contains("╭ Ask")
            && text.contains("Which database should the project use?")
            && text.contains("Space select · ↵ confirm")
            && !text.contains("Pipe streams")
    });
    pty.input("\x03");
    pty.finish();
}

