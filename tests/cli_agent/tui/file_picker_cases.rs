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

    let has_item = |screen: &vt100::Screen, label: &str| {
        screen
            .contents()
            .lines()
            .any(|line| line.trim().trim_start_matches("❯ ").trim() == label)
    };

    // 根目录单列左缩进四列，选中项为黄色，其余文字亮白。
    pty.input("分析 @");
    pty.wait_for(
        "输入框上方列出根目录，不平铺后代文件",
        |screen| {
            let text = screen.contents();
            has_item(screen, "src/")
                && has_item(screen, "README.md")
                && text.matches("README.md").count() == 1
                && !text.contains("Enter/Tab")
                && !text.contains("src/main.rs")
                && !text.contains("src/nested/")
        },
    );
    for label in ["src/", "README.md"] {
        let screen = pty.parser.screen();
        let (row, col) = screen
            .rows(0, screen.size().1)
            .enumerate()
            .find_map(|(row, line)| {
                line.find(label)
                    .map(|byte| (row, line[..byte].chars().count()))
            })
            .unwrap();
        assert_eq!(col, 6);
        assert!((0..4).all(|x| screen.cell(row as u16, x).unwrap().contents() == " "));
        let color = if label == "src/" {
            assert_eq!(screen.cell(row as u16, 4).unwrap().contents(), "❯");
            assert_eq!(
                screen.cell(row as u16, 4).unwrap().fgcolor(),
                vt100::Color::Rgb(255, 190, 55)
            );
            vt100::Color::Rgb(255, 190, 55)
        } else {
            vt100::Color::Idx(15)
        };
        assert_eq!(
            screen.cell(row as u16, col as u16).unwrap().fgcolor(),
            color
        );
    }
    pty.input("\r");
    pty.wait_for("Enter 进入目录", |screen| {
        has_item(screen, "../") && has_item(screen, "nested/") && has_item(screen, "main.rs")
    });
    pty.draft("分析 @src/");
    no_more(&server);
    pty.input("\t");
    pty.wait_for("Tab 进入第二级目录", |screen| {
        has_item(screen, "../") && has_item(screen, "child.rs") && !has_item(screen, "main.rs")
    });
    pty.draft("分析 @src/nested/");
    no_more(&server);
    pty.input("\x1b[D");
    pty.wait_for("左方向键返回一级目录", |screen| {
        has_item(screen, "main.rs")
    });
    pty.draft("分析 @src/");
    pty.input("\x1b[D");
    pty.wait_for("左方向键返回项目根目录", |screen| {
        has_item(screen, "README.md") && !has_item(screen, "../")
    });
    pty.draft("分析 @");
    pty.input("\x1b[C");
    pty.wait_for("右方向键进入目录", |screen| {
        has_item(screen, "main.rs")
    });
    pty.draft("分析 @src/");

    // 继续输入只筛选当前子树；确认只插入引用，不提交草稿。
    pty.input("main");
    pty.wait_for("筛选后只剩 main.rs", |screen| {
        has_item(screen, "main.rs") && !has_item(screen, "menu.rs")
    });
    pty.input("\x1b[C");
    pty.draft("分析 @src/main.rs");
    pty.wait_for("插入后菜单关闭", |screen| !has_item(screen, "../"));
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

    // 回复后仍可用 Tab 插入文件，不发送请求。
    pty.input("\x01\x0btab @");
    pty.wait_for("新的 @ 激活重新列出候选", |screen| {
        has_item(screen, "README.md") && !has_item(screen, "../")
    });
    pty.input("\t");
    pty.wait_for("Tab 打开目录", |screen| has_item(screen, "main.rs"));
    pty.input("main");
    pty.wait_for("Tab 选择筛选后的文件", |screen| {
        has_item(screen, "main.rs") && !has_item(screen, "nested/")
    });
    pty.input("\t");
    pty.draft("tab @src/main.rs");
    no_more(&server);

    // Esc 取消选择但保留已补全的目录路径。
    pty.input("\x01\x0besc @");
    pty.wait_for("Esc 前菜单打开", |screen| {
        has_item(screen, "README.md")
    });
    pty.input("\t");
    pty.wait_for("进入 src", |screen| has_item(screen, "nested/"));
    pty.input("\t");
    pty.wait_for("进入 nested", |screen| has_item(screen, "child.rs"));
    pty.input("\x1b");
    pty.wait_for("Esc 关闭菜单并保留草稿", |screen| {
        !has_item(screen, "../") && !has_item(screen, "child.rs")
    });
    pty.draft("esc @src/nested/");
    no_more(&server);

    // 输入空格即可结束目录引用并关闭菜单，不发送草稿。
    pty.input("\x01\x0bdirectory @");
    pty.wait_for("目录引用前根菜单打开", |screen| {
        has_item(screen, "README.md")
    });
    pty.input("\x1b[C");
    pty.wait_for("目录引用只显示子项和返回上级", |screen| {
        has_item(screen, "main.rs") && !screen.contents().contains("引用当前目录")
    });
    pty.input(" ");
    pty.draft("directory @src/");
    pty.wait_for("空格结束目录引用并关闭菜单", |screen| {
        !has_item(screen, "../") && !has_item(screen, "main.rs")
    });
    no_more(&server);

    // 菜单打开时缩放终端，恢复后仍可用 Enter 选择文件。
    pty.input("\x01\x0bresize @");
    pty.wait_for("缩放前根菜单打开", |screen| {
        has_item(screen, "README.md")
    });
    pty.input("\r");
    pty.wait_for("缩放前进入 src", |screen| has_item(screen, "main.rs"));
    pty.resize(4, 20);
    pty.resize(30, 100);
    pty.wait_for("恢复尺寸后菜单仍可选", |screen| {
        has_item(screen, "main.rs")
            && has_item(screen, "menu.rs")
            && !screen.contents().contains("Enter/Tab")
    });
    pty.input("\x1b[F\r");
    pty.draft("resize @src/menu.rs");
    no_more(&server);

    // 菜单打开时 Ctrl-C 仍立即退出并恢复终端。
    pty.input("\x01\x0bquit @");
    pty.wait_for("退出前菜单打开", |screen| {
        has_item(screen, "README.md")
    });
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
            screen.contents().contains("\u{f03e} #1") && screen.contents().contains("2x3")
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
            screen.contents().contains("\u{f03e} #1") && screen.contents().contains("2x3")
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

#[test]
fn tui_image_references_hover_atomic_delete_and_frozen_snapshots_both_protocols() {
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
        let first = project.path().join("one.png");
        let second = project.path().join("two.png");
        image::RgbImage::from_pixel(884, 368, image::Rgb([230, 40, 70]))
            .save(&first)
            .unwrap();
        image::DynamicImage::new_rgb8(4, 6).save(&second).unwrap();
        let mut pty = Pty::start_with_args(project.path(), home.path(), &[]);
        pty.ready_model("fixture/primary");
        pty.input("\x1b[200~说明 @one.png @two.png \x1b[201~");
        pty.wait_for("两张图片引用转换为缩略图", |screen| {
            let text = screen.contents();
            text.contains("#1")
                && text.contains("#2")
                && text.contains("884x368")
                && text.contains("4x6")
        });
        let top = pty
            .parser
            .screen()
            .rows(0, 100)
            .position(|row| row.contains("#1") && row.contains('╭'))
            .unwrap();
        pty.input(&format!("\x1b[<35;4;{}M", top + 3));
        let preview = |screen: &vt100::Screen| {
            screen
                .rows(0, 100)
                .any(|row| row.contains("#1") && row.chars().filter(|c| *c == '─').count() > 70)
        };
        pty.wait_for("缩略图悬停显示大图", preview);
        pty.wait_for("大图内容在缩略图以外的区域绘出", |screen| {
            (0..top as u16).any(|row| {
                (25..94).any(|col| {
                    let cell = screen.cell(row, col).unwrap();
                    cell.fgcolor() == vt100::Color::Rgb(230, 40, 70)
                        || cell.bgcolor() == vt100::Color::Rgb(230, 40, 70)
                })
            })
        });
        pty.input("\x1b[<35;100;30M");
        pty.wait_for("移出后关闭大图", |screen| !preview(screen));
        pty.input("\x7f\x7f");
        pty.wait_for("删除完整的第二张图片", |screen| {
            let text = screen.contents();
            text.contains("#1") && !text.contains("#2") && !text.contains("4x6")
        });
        pty.input("\x15");
        pty.wait_for("撤销同时恢复标记和缩略图", |screen| {
            screen.contents().contains("#2") && screen.contents().contains("4x6")
        });
        std::fs::remove_file(first).unwrap();
        std::fs::remove_file(second).unwrap();
        no_more(&server);
        pty.input("\r");
        let (request, body) = pty.request_at(&server, api_path(protocol));
        let history = if protocol == "completion" {
            body["messages"].as_array().unwrap()
        } else {
            body["input"].as_array().unwrap()
        };
        let user = history
            .iter()
            .find(|message| message["role"] == "user")
            .unwrap();
        assert_eq!(
            user["content"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|part| part["type"] == "image_url" || part["type"] == "input_image")
                .count(),
            2
        );
        assert!(user.to_string().contains("说明"));
        if protocol == "completion" {
            final_reply(request, "images-snapshot-done");
        } else {
            response_final(request, "images-snapshot-done");
        }
        pty.text("images-snapshot-done");
        pty.completed();
        pty.input("\x03");
        pty.finish();
        no_more(&server);
    }
}
