#[test]
fn tui_streaming_completion_menus_remain_usable_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = runtime_input_home(&server, protocol, "");
        install_command(home.path(), "qcmd.md", "Review $ARGUMENTS");
        std::fs::write(home.path().join("picker-file.rs"), "fn main() {}\n").unwrap();
        let mut pty = Pty::start_with_args(home.path(), home.path(), &[]);
        pty.ready();
        let mut stream = runtime_input_live(&mut pty, &server, protocol, "completion task");

        pty.input("/mo");
        pty.text("Select a model at the next safe boundary");
        streaming_delta(&mut stream, protocol, "completion-menu-stream-progress");
        pty.text("completion-menu-stream-progress");
        pty.text("Select a model at the next safe boundary");
        pty.input("\x1b");
        pty.wait_for("Esc 收起补全并保留运行中的输入", |screen| {
            !screen.contents().contains("Select a model at the next safe boundary")
                && screen.contents().contains("❯ /mo")
        });
        // 不修改草稿，直接 Tab 重新唤起并完成补全。
        pty.input("\t");
        pty.draft("/model");
        pty.input(" ");
        pty.text("");
        pty.text("fixture/primary");
        pty.input("\x1b");
        // 主输入框始终在面板下方可见；必须等搜索焦点真正归还，否则紧跟的
        // Esc 会与下一个字符合并成 Alt 序列。
        pty.draft("");

        pty.input("/think h");
        pty.text("Deep thinking");
        pty.input("\t");
        pty.draft("/effort high");
        // Enter 现在会把补全好的 `/effort high` 直接提交并清空草稿，后续 Esc 就会取消任务；
        // 这里只验证流式期间补全菜单可用，所以补全后用 Esc 关闭菜单。
        pty.input("\x1b");
        pty.wait_for("关闭思考补全且保留运行中的草稿", |screen| {
            !screen.contents().contains("Deep thinking")
                && screen.contents().contains("❯ /effort high")
        });

        pty.input("\x01\x0b/qc");
        pty.wait_for("输出期间显示模板命令补全", |screen| {
            screen.contents().lines().any(|line| line.starts_with("❯ /qc"))
                && screen.contents().contains("/qcmd")
        });
        pty.input("\t @picker");
        pty.text("picker-file.rs");
        pty.input("\t");
        pty.draft("/qcmd @picker-file.rs");
        streaming_delta(&mut stream, protocol, "completion-file-stream-progress");
        pty.text("completion-file-stream-progress");
        pty.draft("/qcmd @picker-file.rs");
        no_more(&server);

        streaming_complete(
            &mut stream,
            protocol,
            "runtime-live-markercompletion-menu-stream-progresscompletion-file-stream-progress",
        );
        stream.finish();
        pty.completed();
        pty.draft("/qcmd @picker-file.rs");
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn tui_menu_overlay_keeps_input_cursor_and_history_anchored() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("picker-file.rs"), "fn main() {}\n").unwrap();
    let mut pty = Pty::start(&server, home.path());
    pty.ready();
    pty.input("show history\r");
    let (request, _) = pty.request(&server);
    let answer = (0..40)
        .map(|index| format!("anchor-line-{index:02}\n\n"))
        .collect::<String>();
    final_reply(request, &answer);
    pty.text("anchor-line-39");
    pty.completed();
    let top = (0..8)
        .map(|row| tip_columns(pty.parser.screen(), row, 0..100))
        .collect::<Vec<_>>();

    pty.input("/");
    pty.wait_for("命令菜单在输入框上方显示且光标固定", |screen| {
        screen.contents().contains("Show available commands")
            && screen.cursor_position() == (27, 3)
            && screen.cell(18, 2).unwrap().contents() == "/"
    });
    assert_eq!(
        pty.parser.screen().cell(18, 2).unwrap().fgcolor(),
        vt100::Color::Rgb(153, 194, 232)
    );
    for (row, expected) in top.iter().enumerate() {
        assert_eq!(tip_columns(pty.parser.screen(), row as u16, 0..100), *expected);
    }
    pty.input("mo");
    pty.wait_for("筛选到单个命令时输入行保持原位", |screen| {
        screen.cursor_position() == (27, 5)
            && screen.cell(25, 2).unwrap().contents() == "/"
            && !screen.contents().contains("Show available commands")
    });
    pty.input("\x1b");
    pty.wait_for("收起菜单保留草稿和光标", |screen| {
        screen.cursor_position() == (27, 5)
            && !screen.contents().contains("List or select a configured model")
    });
    pty.draft("/mo");

    pty.function_key('p');
    pty.wait_for("模型菜单在输入框上方显示且光标进入搜索框", |screen| {
        screen.contents().contains("Ctrl + S")
            && screen.cursor_position() == (22, 2)
            && screen.cell(27, 0).unwrap().contents() == "❯"
    });
    pty.input("\x1b");
    pty.wait_for("Esc 关闭模型菜单并保留草稿", |screen| {
        !screen.contents().contains("Ctrl + S") && input_is_ready(screen)
    });
    pty.draft("/mo");
    pty.input("\x01\x0b/effort ");
    pty.wait_for("思考菜单在固定输入行上方显示", |screen| {
        screen.contents().contains("Disable thinking") && screen.cursor_position() == (27, 10)
    });
    pty.input("\x1b");
    pty.wait_for("Esc 收起思考菜单并保留草稿", |screen| {
        !screen.contents().contains("Disable thinking") && input_is_ready(screen)
    });
    pty.draft("/effort");
    pty.input("\x01\x0b@");
    pty.wait_for("文件菜单在固定输入行上方显示", |screen| {
        screen.contents().contains("picker-file.rs") && screen.cursor_position() == (27, 3)
    });
    pty.resize(12, 40);
    pty.wait_for("缩放后文件菜单仍在输入框上方", |screen| {
        screen.contents().contains("picker-file.rs") && screen.cursor_position() == (9, 3)
    });
    pty.input("\x1b");
    pty.wait_for("缩放后收起菜单不移动输入行", |screen| {
        !screen.contents().contains("picker-file.rs") && screen.cursor_position() == (9, 3)
    });
    pty.draft("@");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}
