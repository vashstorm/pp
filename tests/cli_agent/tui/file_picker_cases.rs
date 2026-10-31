use super::*;

#[test]
fn tui_file_picker_inserts_path_without_sending() {
    let server = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
    std::fs::write(
        config_path(home.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();
    std::fs::create_dir_all(project.path().join("src")).unwrap();
    std::fs::write(
        project.path().join("src/main.rs"),
        "attached-resource-content",
    )
    .unwrap();
    std::fs::write(project.path().join("src/menu.rs"), "menu source").unwrap();
    std::fs::create_dir(project.path().join("src/nested")).unwrap();
    std::fs::write(project.path().join("src/nested/child.rs"), "nested source").unwrap();
    std::fs::write(project.path().join("README.md"), "project root").unwrap();
    let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
    pty.ready_model("fixture/primary");

    // 根菜单只显示直接子项；进入目录同步补全路径，但不发送草稿。
    pty.input("分析 @");
    pty.wait_for(
        "输入框上方列出根目录，不平铺后代文件",
        |screen| {
            let text = screen.contents();
            text.find("❯ 分析 @").is_some_and(|draft| {
                text.find("src/").is_some_and(|directory| directory < draft)
                    && text.find("README.md").is_some_and(|file| file < draft)
            }) && text.contains("Enter/Tab open/insert")
                && !text.contains("src/main.rs")
                && !text.contains("src/nested/")
        },
    );
    pty.input("\r");
    pty.text("Parent of src/");
    pty.text("src/nested/");
    pty.draft("分析 @src/");
    no_more(&server);
    pty.input("\t");
    pty.wait_for(
        "进入第二级目录，只显示当前目录文件",
        |screen| {
            let text = screen.contents();
            text.contains("Parent of src/nested/")
                && text.contains("src/nested/child.rs")
                && !text.contains("src/main.rs")
                && !text.contains("src/menu.rs")
        },
    );
    pty.draft("分析 @src/nested/");
    no_more(&server);
    pty.input("\x1b[H\r");
    pty.text("src/main.rs");
    pty.draft("分析 @src/");
    pty.input("\x1b[H\t");
    pty.wait_for("返回项目根目录", |screen| {
        let text = screen.contents();
        text.contains("README.md") && !text.contains("Parent of src/")
    });
    pty.draft("分析 @");
    pty.input("\r");
    pty.text("Parent of src/");

    // 继续输入只筛选当前子树；不平铺下层目录。
    pty.input("main");
    pty.wait_for("筛选后只剩 src/main.rs", |screen| {
        let text = screen.contents();
        text.contains("src/main.rs") && !text.contains("src/menu.rs")
    });

    // Enter 只插入引用，不提交草稿，也不读取文件内容。
    pty.input("\r");
    pty.draft("分析 @src/main.rs");
    pty.wait_for("插入后菜单关闭", |screen| {
        !screen.contents().contains("Enter/Tab open/insert")
    });
    no_more(&server);

    // 补全时只编辑草稿，提交时携带实际文件正文。
    pty.input("说明\r");
    let (request, body) = pty.request(&server);
    let serialized = body.to_string();
    assert!(
        serialized.contains("attached-resource-content"),
        "{serialized}"
    );
    let messages = body["messages"].as_array().unwrap();
    let user = messages.last().unwrap();
    assert_eq!(user["role"], "user");
    assert!(
        user["content"]
            .as_str()
            .unwrap()
            .contains("attached-resource-content")
    );
    assert!(user["content"].as_str().unwrap().contains("说明"));
    final_reply(request, "file picker answer");
    pty.text("file picker answer");
    pty.ready_model("fixture/primary");

    // 回复后可继续编辑；Tab 与 Enter 执行同一种插入，不发送请求。
    pty.input("\x01\x0btab @");
    pty.wait_for("新的 @ 激活重新列出候选", |screen| {
        let text = screen.contents();
        text.contains("Enter/Tab open/insert") && !text.contains("Parent of src/")
    });
    pty.input("\t");
    pty.text("Parent of src/");
    pty.input("main");
    pty.wait_for("Tab 选择筛选后的文件", |screen| {
        let text = screen.contents();
        text.contains("❯ tab @src/main") && !text.contains("src/nested/")
    });
    pty.input("\t");
    pty.draft("tab @src/main.rs");
    no_more(&server);

    // Esc 取消选择但保留已补全的目录路径。
    pty.input("\x01\x0besc @");
    pty.wait_for("Esc 前菜单打开", |screen| {
        let text = screen.contents();
        text.contains("Enter/Tab open/insert") && !text.contains("Parent of src/")
    });
    pty.input("\t");
    pty.text("Parent of src/");
    pty.input("\t");
    pty.text("Parent of src/nested/");
    pty.input("\x1b");
    pty.wait_for("Esc 关闭菜单并保留 @ 草稿", |screen| {
        let text = screen.contents();
        !text.contains("Enter/Tab open/insert") && text.contains("❯ esc @")
    });
    pty.draft("esc @src/nested/");
    no_more(&server);

    // 菜单打开时缩放终端，恢复后仍可用键盘选择文件。
    pty.input("\x01\x0bresize @");
    pty.wait_for("缩放前根菜单打开", |screen| {
        let text = screen.contents();
        text.contains("Enter/Tab open/insert") && !text.contains("Parent of src/")
    });
    pty.input("\r");
    pty.text("Parent of src/");
    pty.resize(4, 20);
    pty.resize(30, 100);
    pty.wait_for("恢复尺寸后菜单仍可选", |screen| {
        let text = screen.contents();
        text.contains("Enter/Tab open/insert")
            && text.contains("src/main.rs")
            && text.contains("src/menu.rs")
    });
    pty.input("\x1b[F\x1b[A\r");
    pty.draft("resize @src/menu.rs");
    no_more(&server);

    // 菜单打开时 Ctrl-C 仍立即退出并恢复终端。
    pty.input("\x01\x0bquit @");
    pty.text("Enter/Tab open/insert");
    pty.input("\x03");
    pty.finish();
    no_more(&server);
}

#[test]
fn tui_image_path_paste_and_persistent_recall_send_real_images_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), fixture_settings()).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let path = project.path().join("my screenshot.png");
        image::DynamicImage::new_rgb8(2, 3).save(&path).unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready_model("fixture/primary");
        pty.input(&format!("\x1b[200~{}\x1b[201~", path.display()));
        pty.wait_for("图片路径转为编号和摘要", |screen| {
            screen.contents().contains("[图片 1]") && screen.contents().contains("2 x 3")
        });
        no_more(&server);
        pty.input("解释\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        let history = if protocol == "completion" {
            body["messages"].as_array().unwrap()
        } else {
            body["input"].as_array().unwrap()
        };
        let user = history.iter().find(|m| m["role"] == "user").unwrap();
        let content = user["content"].as_array().unwrap();
        assert_eq!(
            content
                .iter()
                .filter(|part| part["type"] == "image_url" || part["type"] == "input_image")
                .count(),
            1
        );
        assert!(user.to_string().contains("data:image/png;base64,"));
        assert!(user.to_string().contains("解释"));
        if protocol == "completion" {
            final_reply(request, "image-first-done");
        } else {
            response_final(request, "image-first-done");
        }
        pty.text("image-first-done");
        pty.completed();
        pty.input("\x03");
        pty.finish();
        std::fs::remove_file(path).unwrap();
        let id = saved_session_id(home.path());
        let mut pty = Pty::start_with_args(project.path(), home.path(), &["-r", &id]);
        pty.ready_model("fixture/primary");
        pty.input("\x1b[A");
        pty.wait_for("重启后召回带图输入", |screen| {
            screen.contents().contains("[图片 1]") && screen.contents().contains("2 x 3")
        });
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        let history = if protocol == "completion" {
            body["messages"].as_array().unwrap()
        } else {
            body["input"].as_array().unwrap()
        };
        let users: Vec<_> = history.iter().filter(|m| m["role"] == "user").collect();
        assert_eq!(users.len(), 2);
        assert!(
            users
                .iter()
                .all(|user| user.to_string().contains("data:image/png;base64,"))
        );
        if protocol == "completion" {
            final_reply(request, "image-recall-done");
        } else {
            response_final(request, "image-recall-done");
        }
        pty.text("image-recall-done");
        pty.completed();
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}
