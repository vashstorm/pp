// 自动加载卡片（AGENTS 与技能 read）的消息区行为：折叠预览、Ctrl+O 展开、
// resize、去重、/resume 冻结正文、刷新/删除/空白/重建、失败请求、树回退与
// 原生历史收尾。全部使用真实 PTY、临时 HOME、回环 mock provider 与文件
// marker 作同步栅栏；辅助函数一律使用 `instruction_cards_` 前缀，不新增
// use 声明，不改动其他 include 文件。

fn instruction_cards_settings() -> &'static str {
    "{\"default_model\":\"fixture/primary\",\"notification_timeout_seconds\":1,\"read\":{\"toolResultPreview\":false}}"
}

// 历史只能包含供应商实际返回的技能 read，自动 AGENTS 加载不是模型工具调用。
fn instruction_cards_assert_plain_request(
    body: &serde_json::Value,
    protocol: &str,
    expected_reads: usize,
) {
    let calls: Vec<_> = if protocol == "completion" {
        body["messages"].as_array().unwrap().iter()
            .filter_map(|message| message["tool_calls"].as_array())
            .flatten()
            .map(|call| &call["function"])
            .collect()
    } else {
        body["input"].as_array().unwrap().iter()
            .filter(|item| item["type"] == "function_call")
            .collect()
    };
    assert_eq!(calls.len(), expected_reads, "{body}");
    for call in calls {
        assert_eq!(call["name"], "read", "{body}");
        let args: serde_json::Value =
            serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "skill://rust", "{body}");
    }
}

// 在主屏幕与滚动视图里分页走完整消息区（备用屏幕渲染当前内部布局），
// 返回拼接文本；到顶/到底由"画面不再变化"判断，不用 sleep 猜测。
fn instruction_cards_paged_text(pty: &mut Pty) -> String {
    for _ in 0..200 {
        if !resume_history_page(pty, "\x1b[5~") {
            break;
        }
    }
    let mut pages = Vec::new();
    for _ in 0..200 {
        // 主屏幕是提交时的冻结历史；只采集备用屏幕里的当前折叠投影。
        if !pty.parser.screen().alternate_screen() {
            break;
        }
        pages.push(pty.parser.screen().contents());
        if !resume_history_page(pty, "\x1b[<65;10;5M") {
            break;
        }
    }
    pages.join("\n")
}

// 失败请求的错误文案经过通知弹窗，长路径会按词折行；按空格合并后再匹配。
fn instruction_cards_wait_error(pty: &mut Pty, needle: &str) {
    pty.wait_for("请求失败提示可见且输入恢复", |screen| {
        input_is_ready(screen) && screen.contents().replace('\n', " ").contains(needle)
    });
}

// 复用既有同步栅栏：模型面板只在 worker 终结事件后出现。
fn instruction_cards_barrier(pty: &mut Pty) {
    pty.input("/model ");
    pty.model_search("");
    pty.text("fixture/primary");
    pty.input("\x1b");
    pty.draft("");
}

fn instruction_cards_exit(pty: &mut Pty) {
    pty.input("\x03");
    pty.wait_for("退出恢复终端模式", |screen| !screen.bracketed_paste());
}

#[test]
fn instruction_loading_cards_preview_expand_and_resume_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), instruction_cards_settings()).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let agents = home.path().join("AGENTS.md");
        let markers: Vec<String> = (0..15).map(|index| format!("agents-rule-line-{index:02}")).collect();
        std::fs::write(&agents, markers.join("\n")).unwrap();
        let skill = skills::install(home.path(), "rust", "rust");
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready();
        let path = api_path(protocol);

        pty.input("cards-task-one\r");
        let (request, body) = pty.request_at(&server, path);
        let system = project_instructions::system(&body, protocol).to_owned();
        assert!(system.contains("agents-rule-line-00"), "{system}");
        assert!(system.contains("agents-rule-line-14"), "{system}");
        assert!(system.contains("Rust 测试指导"), "{system}");
        assert!(!body.to_string().contains("secret-skill-line"), "{body}");
        instruction_cards_assert_plain_request(&body, protocol, 0);
        skills::send_skill_read(request, protocol, "rust");
        let (request, body) = pty.request_at(&server, path);
        assert!(body.to_string().contains("secret-skill-line-19"), "{body}");
        project_instructions::finish_request(request, protocol, "cards-skill-ready");
        pty.text("Read skill://rust");
        pty.text("cards-skill-ready");
        pty.ready();

        // sent 在首个建连结果之后执行；先返回 SSE，再检查真实加载卡片。
        let agents_footer = format!("Resolved path: {}", agents.canonicalize().unwrap().display());
        let paged = instruction_cards_paged_text(&mut pty);
        assert!(paged.contains("Read AGENTS.md"), "{paged}");
        assert!(paged.contains("agents-rule-line-00"), "{paged}");
        assert!(paged.contains("agents-rule-line-09"), "{paged}");
        assert!(!paged.contains("agents-rule-line-14"), "{paged}");
        assert!(paged.contains("5 more lines (Ctrl + O to expand)"), "{paged}");
        assert!(paged.contains(&agents_footer), "{paged}");
        {
            let text = pty.parser.screen().contents();
            assert!(text.contains("secret-skill-line-00"), "{text}");
            assert!(!text.contains("secret-skill-line-19"), "{text}");
            assert!(text.contains("more lines (Ctrl + O to expand)"), "{text}");
            // 真实路径可跨多条屏幕行；去掉卡片边框和换行后核对完整来源。
            let unwrapped: String = text
                .chars()
                .filter(|c| !c.is_whitespace() && *c != '│')
                .collect();
            assert!(
                unwrapped.contains(&format!(
                    "Resolvedpath:{}",
                    skill.canonicalize().unwrap().display()
                )),
                "{text}"
            );
        }

        // Ctrl+O 展开：两份正文末尾都可见，折叠提示消失。
        pty.function_key('o');
        pty.text("secret-skill-line-19");
        let paged = instruction_cards_paged_text(&mut pty);
        assert!(paged.contains("agents-rule-line-14"), "{paged}");
        assert!(paged.contains("secret-skill-line-19"), "{paged}");
        assert!(!paged.contains("more lines (Ctrl + O to expand)"), "{paged}");

        // resize 后内容仍可读。
        pty.resize(4, 20);
        pty.resize(30, 100);
        pty.draft("");
        let paged = instruction_cards_paged_text(&mut pty);
        assert!(paged.contains("agents-rule-line-14"), "{paged}");
        assert!(paged.contains("secret-skill-line-19"), "{paged}");

        // 收起后回到折叠预览。
        pty.function_key('o');
        pty.text("more lines (Ctrl + O to expand)");
        let paged = instruction_cards_paged_text(&mut pty);
        assert!(!paged.contains("agents-rule-line-14"), "{paged}");
        assert!(!paged.contains("secret-skill-line-19"), "{paged}");

        // 文档未变化：继续任务不新增自动加载卡片。
        pty.input("cards-task-two\r");
        let (request, body) = pty.request_at(&server, path);
        assert!(project_instructions::system(&body, protocol).contains("agents-rule-line-00"));
        instruction_cards_assert_plain_request(&body, protocol, 1);
        project_instructions::finish_request(request, protocol, "cards-second-done");
        pty.text("cards-second-done");
        pty.ready();
        instruction_cards_barrier(&mut pty);
        {
            let (history, _) = pty.native_history();
            assert_eq!(history.matches("Read AGENTS.md").count(), 1, "{history}");
            assert_eq!(history.matches("Read skill://rust").count(), 1, "{history}");
        }

        instruction_cards_exit(&mut pty);
        pty.finish();
        no_more(&server);

        // 磁盘技能正文轮换：恢复的卡片必须展示日志冻结正文而非当前磁盘内容。
        std::fs::write(
            &skill,
            format!(
                "---\nname: rust\ndescription: Rust 测试指导\n---\n{}",
                (0..20).map(|index| format!("disk-rotated-skill-line-{index:02}\n")).collect::<String>()
            ),
        )
        .unwrap();
        let id = saved_session_id(home.path());
        let mut pty = Pty::start_with_args(home.path(), home.path(), &["--resume", &id]);
        resume_history_wait_resumed(&mut pty, &id, &["cards-skill-ready"]);
        pty.ready();
        pty.function_key('o');
        pty.text("secret-skill-line-19");
        let paged = instruction_cards_paged_text(&mut pty);
        assert!(paged.contains("agents-rule-line-14"), "{paged}");
        assert!(paged.contains("agents-rule-line-00"), "{paged}");
        assert!(paged.contains("secret-skill-line-00"), "{paged}");
        assert!(!paged.contains("disk-rotated-skill-line"), "{paged}");

        // 恢复后的会话再发任务：AGENTS 未变化，不重复展示加载卡片。
        pty.input("cards-task-three\r");
        let (request, body) = pty.request_at(&server, path);
        assert!(project_instructions::system(&body, protocol).contains("agents-rule-line-00"));
        instruction_cards_assert_plain_request(&body, protocol, 1);
        project_instructions::finish_request(request, protocol, "cards-resume-done");
        pty.text("cards-resume-done");
        pty.ready();
        instruction_cards_barrier(&mut pty);
        {
            let (history, _) = pty.native_history();
            assert_eq!(history.matches("Read AGENTS.md").count(), 1, "{history}");
        }
        instruction_cards_exit(&mut pty);
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn instruction_loading_cards_refresh_new_and_failures_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), instruction_cards_settings()).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let agents = home.path().join("AGENTS.md");
        std::fs::write(&agents, "cards-refresh-first-rule").unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready();
        let path = api_path(protocol);

        pty.input("cards-refresh-one\r");
        let (request, body) = pty.request_at(&server, path);
        assert!(
            project_instructions::system(&body, protocol).contains("cards-refresh-first-rule"),
            "{body}"
        );
        instruction_cards_assert_plain_request(&body, protocol, 0);
        project_instructions::finish_request(request, protocol, "cards-refresh-one-done");
        pty.text("Read AGENTS.md");
        pty.text("cards-refresh-one-done");
        pty.ready();

        // 内容变化后下一次加载展示新卡片，旧卡片正文冻结。
        std::fs::write(&agents, "cards-refresh-second-rule").unwrap();
        pty.input("cards-refresh-two\r");
        let (request, body) = pty.request_at(&server, path);
        let system = project_instructions::system(&body, protocol).to_owned();
        assert!(system.contains("cards-refresh-second-rule"), "{system}");
        assert!(!system.contains("cards-refresh-first-rule"), "{system}");
        project_instructions::finish_request(request, protocol, "cards-refresh-two-done");
        pty.text("cards-refresh-two-done");
        pty.ready();
        let paged = instruction_cards_paged_text(&mut pty);
        assert!(paged.matches("Read AGENTS.md").count() >= 2, "{paged}");
        assert!(paged.contains("cards-refresh-first-rule"), "{paged}");
        assert!(paged.contains("cards-refresh-second-rule"), "{paged}");

        // 删除：请求照常发送，不产生成功卡片。
        std::fs::remove_file(&agents).unwrap();
        pty.input("cards-refresh-three\r");
        let (request, body) = pty.request_at(&server, path);
        assert!(
            !project_instructions::system(&body, protocol).contains("[Project instructions:"),
            "{body}"
        );
        project_instructions::finish_request(request, protocol, "cards-refresh-three-done");
        pty.text("cards-refresh-three-done");
        pty.ready();

        // 空白正文：同样不产生卡片。
        std::fs::write(&agents, " \n\t \n").unwrap();
        pty.input("cards-refresh-four\r");
        let (request, body) = pty.request_at(&server, path);
        assert!(
            !project_instructions::system(&body, protocol).contains("[Project instructions:"),
            "{body}"
        );
        project_instructions::finish_request(request, protocol, "cards-refresh-four-done");
        pty.text("cards-refresh-four-done");
        pty.ready();

        // 重新建立正文：再次展示。
        std::fs::write(&agents, "cards-refresh-third-rule").unwrap();
        pty.input("cards-refresh-five\r");
        let (request, body) = pty.request_at(&server, path);
        assert!(
            project_instructions::system(&body, protocol).contains("cards-refresh-third-rule"),
            "{body}"
        );
        project_instructions::finish_request(request, protocol, "cards-refresh-five-done");
        pty.text("cards-refresh-five-done");
        pty.ready();
        instruction_cards_barrier(&mut pty);
        {
            let (history, _) = pty.native_history();
            assert_eq!(history.matches("Read AGENTS.md").count(), 3, "{history}");
            assert!(history.contains("cards-refresh-first-rule"), "{history}");
            assert!(history.contains("cards-refresh-second-rule"), "{history}");
            assert!(history.contains("cards-refresh-third-rule"), "{history}");
        }

        // /new 清空会话后，同一 AGENTS 在新会话首个请求重新展示。
        pty.input("/new\r");
        pty.wait_for_raw(b"\x1b[3J");
        pty.text("Session & workflow");
        pty.ready();
        pty.input("cards-refresh-six\r");
        let (request, body) = pty.request_at(&server, path);
        assert!(
            project_instructions::system(&body, protocol).contains("cards-refresh-third-rule"),
            "{body}"
        );
        instruction_cards_assert_plain_request(&body, protocol, 0);
        project_instructions::finish_request(request, protocol, "cards-refresh-six-done");
        pty.text("Read AGENTS.md");
        pty.text("cards-refresh-six-done");
        pty.ready();
        instruction_cards_barrier(&mut pty);
        {
            let (history, _) = pty.native_history();
            assert_eq!(history.matches("Read AGENTS.md").count(), 1, "{history}");
        }

        // 非 UTF-8 正文阻止请求：真实错误、无模型请求、无成功卡片。
        std::fs::write(&agents, b"cards-secret-document\xff".as_slice()).unwrap();
        pty.input("cards-refresh-broken\r");
        instruction_cards_wait_error(&mut pty, "as UTF-8");
        assert!(
            !pty.parser.screen().contents().contains("cards-secret-document"),
            "{}",
            pty.parser.screen().contents()
        );
        no_more(&server);

        // symlink 同样阻止请求。
        std::fs::remove_file(&agents).unwrap();
        std::os::unix::fs::symlink(home.path().join("cards-symlink-target"), &agents).unwrap();
        pty.input("cards-refresh-linked\r");
        instruction_cards_wait_error(&mut pty, "symlinks are forbidden");
        no_more(&server);
        {
            let (history, _) = pty.native_history();
            assert_eq!(history.matches("Read AGENTS.md").count(), 1, "{history}");
        }

        // 修复后重新可加载。
        std::fs::remove_file(&agents).unwrap();
        std::fs::write(&agents, "cards-refresh-fourth-rule").unwrap();
        pty.input("cards-refresh-seven\r");
        let (request, body) = pty.request_at(&server, path);
        assert!(
            project_instructions::system(&body, protocol).contains("cards-refresh-fourth-rule"),
            "{body}"
        );
        project_instructions::finish_request(request, protocol, "cards-refresh-seven-done");
        pty.text("cards-refresh-seven-done");
        pty.ready();
        instruction_cards_barrier(&mut pty);
        {
            let (history, _) = pty.native_history();
            assert_eq!(history.matches("Read AGENTS.md").count(), 2, "{history}");
        }
        instruction_cards_exit(&mut pty);
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn instruction_loading_cards_tree_and_native_history_both_protocols() {
    for protocol in ["completion", "response"] {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(config_path(home.path(), "settings.yml"), instruction_cards_settings()).unwrap();
        std::fs::write(
            config_path(home.path(), "models.yml"),
            response_models(&server, Some(protocol)),
        )
        .unwrap();
        let agents = home.path().join("AGENTS.md");
        std::fs::write(&agents, "cards-tree-first-rule").unwrap();
        let mut pty = Pty::start_with_existing_files(home.path());
        pty.ready();
        let path = api_path(protocol);

        pty.input("cards-tree-one\r");
        let (request, body) = pty.request_at(&server, path);
        assert!(
            project_instructions::system(&body, protocol).contains("cards-tree-first-rule"),
            "{body}"
        );
        project_instructions::finish_request(request, protocol, "cards-tree-one-answer");
        pty.text("Read AGENTS.md");
        pty.text("cards-tree-one-answer");
        pty.ready();

        // 第二轮使用不同的 AGENTS 正文，产生第二张加载卡片。
        std::fs::write(&agents, "cards-tree-second-rule").unwrap();
        pty.input("cards-tree-two\r");
        let (request, body) = pty.request_at(&server, path);
        assert!(
            project_instructions::system(&body, protocol).contains("cards-tree-second-rule"),
            "{body}"
        );
        project_instructions::finish_request(request, protocol, "cards-tree-two-answer");
        pty.text("cards-tree-two-answer");
        pty.ready();

        // 树回退到第二轮之前：只保留有效路径上的第一张加载卡片。
        tree_tui_open(&mut pty, "cards-tree-two-answer");
        tree_tui_rewind(&mut pty, &[TREE_TUI_UP], "cards-tree-two");
        pty.ready();
        {
            let text = pty.parser.screen().contents();
            assert!(text.contains("cards-tree-first-rule"), "{text}");
            assert!(text.contains("Read AGENTS.md"), "{text}");
        }
        tree_tui_assert_abandoned_invisible(
            pty.parser.screen(),
            &["cards-tree-second-rule", "cards-tree-two-answer"],
        );
        no_more(&server);

        instruction_cards_exit(&mut pty);
        pty.finish();
        let id = saved_session_id(home.path());

        // 重启恢复：废弃分支的加载卡片不再出现。
        let mut pty = Pty::start_with_args(home.path(), home.path(), &["--resume", &id]);
        pty.ready();
        {
            let text = pty.parser.screen().contents();
            assert!(text.contains("cards-tree-first-rule"), "{text}");
            assert!(text.contains("Read AGENTS.md"), "{text}");
        }
        tree_tui_assert_abandoned_invisible(
            pty.parser.screen(),
            &["cards-tree-second-rule", "cards-tree-two-answer"],
        );

        // 树回退只裁剪历史，不回滚磁盘；恢复原文后才能验证未变化的去重。
        assert_eq!(std::fs::read_to_string(&agents).unwrap(), "cards-tree-second-rule");
        std::fs::write(&agents, "cards-tree-first-rule").unwrap();
        // 长回答把重建后的对话推进原生历史；文档未变化，不再新增加载卡片。
        pty.input("cards-tree-three\r");
        let (request, body) = pty.request_at(&server, path);
        assert!(
            project_instructions::system(&body, protocol).contains("cards-tree-first-rule"),
            "{body}"
        );
        assert!(!body.to_string().contains("cards-tree-second-rule"), "{body}");
        let answer = (0..140)
            .map(|index| format!("cards-tree-long-{index:03}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        project_instructions::finish_request(request, protocol, &answer);
        pty.text("cards-tree-long-139");
        pty.ready();
        instruction_cards_barrier(&mut pty);
        {
            let (history, _) = pty.native_history();
            assert_eq!(history.matches("Read AGENTS.md").count(), 1, "{history}");
            assert_eq!(history.matches("cards-tree-first-rule").count(), 1, "{history}");
            assert!(!history.contains("cards-tree-second-rule"), "{history}");
        }

        // 退出只补交未提交内容，不重复输出已提交的卡片。
        let before = pty.raw.len();
        instruction_cards_exit(&mut pty);
        let exit = String::from_utf8_lossy(&pty.raw[before..]);
        assert!(
            !exit.contains("cards-tree-first-rule") && !exit.contains("Read AGENTS.md"),
            "退出重放了已提交卡片：{exit:?}"
        );
        let (history, _) = pty.native_history();
        assert_eq!(history.matches("Read AGENTS.md").count(), 1, "{history}");
        pty.finish();
        no_more(&server);
    }
}

#[test]
fn agents_tui_init_submit_cancel_then_run_with_latest_rules_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(config_path(cwd.path(), "settings.yml"), fixture_settings()).unwrap();
        std::fs::write(config_path(cwd.path(), "models.yml"), response_models(&fixture, Some(protocol))).unwrap();
        let mut pty = Pty::start_with_existing_files(cwd.path());
        pty.ready();
        pty.input("/ini");
        pty.text("Create or update project AGENTS.md");
        pty.input("\t");
        pty.draft("/init");
        no_more(&fixture);
        pty.input("\r");
        let (request, body) = project_instructions::receive_for(&fixture, protocol);
        assert!(body.to_string().contains("Create or update AGENTS.md"));
        pty.text("/init");
        pty.input("\x1b");
        cancel_wait_cancelled_ready(&mut pty);
        drop(request);
        no_more(&fixture);
        std::fs::write(cwd.path().join("AGENTS.md"), "tui-latest-project-rule").unwrap();
        pty.input("after-init-cancellation\r");
        let (request, body) = project_instructions::receive_for(&fixture, protocol);
        assert!(project_instructions::system(&body, protocol).contains("tui-latest-project-rule"));
        project_instructions::finish_request(request, protocol, "agents-tui-done");
        pty.text("agents-tui-done");
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}

#[test]
fn agents_tui_resume_history_keeps_current_directory_rules_both_protocols() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let current = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("AGENTS.md"), "saved-directory-rule").unwrap();
        system_prompt::write(home.path(), ".pp/SYSTEM.md", "saved-source-only");
        system_prompt::write(current.path(), ".pp/SYSTEM_TEMPLATE.md", system_prompt::LIVE_TEMPLATE);
        system_prompt::write(current.path(), ".pp/APPEND_SYSTEM.md", "current-append-only");
        let cli = Cli::start_with(home.path(), Some("agents-saved-task"), fixture_settings(), &response_models(&fixture, Some(protocol)));
        let (request, _) = project_instructions::receive_for(&fixture, protocol);
        project_instructions::finish_request(request, protocol, "agents-saved-answer");
        assert!(cli.finish().status.success());
        let files = resume_history_session_files(home.path());
        let id = files[0].file_stem().unwrap().to_str().unwrap();
        std::fs::write(current.path().join("AGENTS.md"), "current-directory-rule").unwrap();
        let mut pty = Pty::start_with_args(current.path(), home.path(), &[]);
        pty.ready();
        system_prompt::write(current.path(), ".pp/SYSTEM_TEMPLATE.md", "{{#if broken}}");
        pty.input("/resume\r");
        // 等待后台扫描结束，避免在 Loading 状态发送会被忽略的方向键。
        pty.text("No sessions in current folder.");
        pty.input("\x1b[C");
        pty.text("agents-saved-task");
        pty.input("\r");
        resume_history_wait_resumed(&mut pty, id, &["agents-saved-task", "agents-saved-answer"]);
        pty.ready();
        std::fs::write(current.path().join("AGENTS.md"), "current-directory-rule-refreshed").unwrap();
        pty.input("agents-resumed-task\r");
        let (request, body) = project_instructions::receive_for(&fixture, protocol);
        system_prompt::assert_live(&body, protocol, "execute");
        let system = project_instructions::system(&body, protocol);
        assert!(system.contains("current-append-only"));
        assert!(!system.contains("saved-source-only"));
        assert_eq!(system.matches("current-directory-rule-refreshed").count(), 1);
        assert!(!system.contains("saved-directory-rule"));
        assert!(body.to_string().contains("agents-saved-task"));
        project_instructions::finish_request(request, protocol, "agents-resume-done");
        pty.text("agents-resume-done");
        pty.ready();
        pty.input("\x03");
        pty.finish();
        no_more(&fixture);
    }
}
