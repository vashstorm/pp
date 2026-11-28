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
    std::fs::write(project.path().join("src/map.rs"), "map source").unwrap();
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

    // The root directory is a single column indented four columns; the selected entry is yellow and the rest bright white.
    pty.input("分析 @");
    pty.wait_for(
        "the root directory is listed above the input box without flattening descendants",
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
    pty.wait_for("Enter enters the directory", |screen| {
        has_item(screen, "../") && has_item(screen, "nested/") && has_item(screen, "main.rs")
    });
    pty.draft("分析 @src/");
    no_more(&server);
    pty.input("\x1b[C");
    pty.wait_for(
        "the right arrow enters the second directory level",
        |screen| {
            has_item(screen, "../") && has_item(screen, "child.rs") && !has_item(screen, "main.rs")
        },
    );
    pty.draft("分析 @src/nested/");
    no_more(&server);
    pty.input("\x1b[D");
    pty.wait_for("the left arrow returns one directory level", |screen| {
        has_item(screen, "main.rs")
    });
    pty.draft("分析 @src/");
    pty.input("\x1b[D");
    pty.wait_for("the left arrow returns to the project root", |screen| {
        has_item(screen, "README.md") && !has_item(screen, "../")
    });
    pty.draft("分析 @");
    pty.input("\x1b[C");
    pty.wait_for("the right arrow enters the directory", |screen| {
        has_item(screen, "main.rs")
    });
    pty.draft("分析 @src/");

    // Further typing filters the current subtree only; confirming inserts the reference without submitting the draft.
    pty.input("main");
    pty.wait_for("filtering leaves only main.rs", |screen| {
        has_item(screen, "main.rs") && !has_item(screen, "map.rs")
    });
    pty.input("\x1b[C");
    pty.draft("分析 @src/main.rs");
    pty.wait_for("the menu closes after insertion", |screen| {
        !has_item(screen, "../")
    });
    no_more(&server);

    // Completion only edits the draft; submitting carries the real file body.
    pty.input("describe\r");
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
    assert!(user["content"].as_str().unwrap().contains("describe"));
    final_reply(request, "file picker answer");
    pty.text("file picker answer");
    pty.ready_model("fixture/primary");

    // With several candidates Tab completes to the common prefix only; the file is completed after filtering to one candidate.
    pty.input("\x01\x0btab @");
    pty.wait_for("a new @ reactivates the candidate list", |screen| {
        has_item(screen, "README.md") && !has_item(screen, "../")
    });
    pty.input("\x1b[C");
    pty.wait_for("open the directory", |screen| has_item(screen, "main.rs"));
    pty.input("m\t");
    pty.draft("tab @src/ma");
    pty.wait_for("two candidates remain after Tab", |screen| {
        has_item(screen, "main.rs") && has_item(screen, "map.rs")
    });
    pty.input("in");
    pty.wait_for("Tab selects the filtered file", |screen| {
        has_item(screen, "main.rs") && !has_item(screen, "nested/")
    });
    pty.input("\t");
    pty.draft("tab @src/main.rs");
    no_more(&server);

    // Esc cancels the selection but keeps the already completed directory path.
    pty.input("\x01\x0besc @");
    pty.wait_for("the menu is open before Esc", |screen| {
        has_item(screen, "README.md")
    });
    pty.input("\x1b[C");
    pty.wait_for("enter src", |screen| has_item(screen, "nested/"));
    pty.input("\x1b[C");
    pty.wait_for("enter nested", |screen| has_item(screen, "child.rs"));
    pty.input("\x1b");
    pty.wait_for("Esc closes the menu and keeps the draft", |screen| {
        !has_item(screen, "../") && !has_item(screen, "child.rs")
    });
    pty.draft("esc @src/nested/");
    no_more(&server);

    // Typing a space ends the directory reference and closes the menu without sending the draft.
    pty.input("\x01\x0bdirectory @");
    pty.wait_for(
        "the root menu opens before the directory reference",
        |screen| has_item(screen, "README.md"),
    );
    pty.input("\x1b[C");
    pty.wait_for(
        "a directory reference shows only children and the parent entry",
        |screen| {
            has_item(screen, "main.rs")
                && !screen.contents().contains("Reference current directory")
        },
    );
    pty.input(" ");
    pty.draft("directory @src/");
    pty.wait_for(
        "a space ends the directory reference and closes the menu",
        |screen| !has_item(screen, "../") && !has_item(screen, "main.rs"),
    );
    no_more(&server);

    // Resizing the terminal while the menu is open still allows picking a file with Enter afterwards.
    pty.input("\x01\x0bresize @");
    pty.wait_for("the root menu opens before the resize", |screen| {
        has_item(screen, "README.md")
    });
    pty.input("\r");
    pty.wait_for("enter src before the resize", |screen| {
        has_item(screen, "main.rs")
    });
    pty.resize(4, 20);
    pty.resize(30, 100);
    pty.wait_for(
        "the menu is still selectable after the size is restored",
        |screen| {
            has_item(screen, "main.rs")
                && has_item(screen, "map.rs")
                && !screen.contents().contains("Enter/Tab")
        },
    );
    pty.input("\x1b[F\r");
    pty.draft("resize @src/map.rs");
    no_more(&server);

    // Ctrl-C still exits immediately and restores the terminal while the menu is open.
    pty.input("\x01\x0bquit @");
    pty.wait_for("the menu is open before quitting", |screen| {
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
        pty.wait_for("the image path becomes a numbered summary", |screen| {
            screen.contents().contains("\u{f03e} #1") && screen.contents().contains("2x3")
        });
        no_more(&server);
        pty.input("explain\r");
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
        assert!(user.to_string().contains("explain"));
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
        pty.wait_for(
            "the recalled input carries the image after a restart",
            |screen| screen.contents().contains("\u{f03e} #1") && screen.contents().contains("2x3"),
        );
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
