#[test]
fn tui_streaming_completion_menus_remain_usable_both_protocols() {
    let protocol = "response";
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
            !screen
                .contents()
                .contains("Select a model at the next safe boundary")
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
            screen
                .contents()
                .lines()
                .any(|line| line.starts_with("❯ /qc"))
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

