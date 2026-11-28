// 临时管道阶段面板的 TUI 回归：真实 `pp` + portable-pty + 内嵌 shell。
// 本地 `!` 命令不产生模型请求；断言只使用可见屏幕、门闩文件与真实终态。
// 辅助项使用 `pipes_` 前缀，测试使用 `pipeline_` 前缀，避免与兄弟 include 冲突。

const PIPES_F8: &str = "\x1b[19~";
const PIPES_PGUP: &str = "\x1b[5~";
const PIPES_END: &str = "\x1b[4~";

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

    // 第二次 Esc 才取消 shell，作为收尾。取消中的提示可能在两次轮询间消失，
    // 以输入恢复和取消终态为栅栏，不依赖瞬时文案。
    pty.input("\x1b");
    bang_shell_wait_done(&mut pty);
    pty.text("Command cancelled; side effects were not rolled back.");
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

