// /plan-list 管理页真实 TUI 覆盖：列表/分类/搜索/预览/零请求，以及冲突安全的重命名与删除。
// 复用 plan_tui.rs 的真实二进制 + PTY + loopback fixture（同一 include 模块共享 helpers）。

fn plan_list_seed(project: &Path) {
    for dir in ["drafts", "plans", "done"] {
        std::fs::create_dir_all(project.join(".pp").join(dir)).unwrap();
    }
    std::fs::write(
        project.join(".pp/drafts/fix-config.md"),
        "# 修复配置\n\n- [ ] 更新配置加载\n- [ ] 读取回验证\n",
    )
    .unwrap();
    std::fs::write(
        project.join(".pp/plans/approved.md"),
        "# Approved plan\n\n- [ ] approved step\n",
    )
    .unwrap();
    std::fs::write(
        project.join(".pp/done/completed.md"),
        "# Completed plan\n\nAll steps verified.\n",
    )
    .unwrap();
}

fn plan_list_open(pty: &mut Pty) {
    pty.input("/plan-list\r");
    pty.wait_for("计划管理页打开", |screen| {
        cancel_normalize(screen).contains("Planlist")
    });
}

fn plan_list_wait_closed(pty: &mut Pty) {
    pty.wait_for("管理页关闭并恢复主输入", |screen| {
        !screen.alternate_screen()
    });
}

#[test]
fn plan_list_filters_previews_and_preserves_input() {
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    plan_list_seed(project.path());
    let mut pty = plan_start(&fixture, home.path(), project.path(), "response");
    pty.ready();
    plan_wait_execute(&mut pty);

    // 打开列表：三个目录的条目、标题、分类、进度与空任务占位。
    plan_list_open(&mut pty);
    pty.wait_for("三目录条目齐全", |screen| {
        let text = cancel_normalize(screen);
        text.contains("fix-config.md")
            && text.contains("修复配置")
            && text.contains("0/2")
            && text.contains("approved.md")
            && text.contains("completed.md")
            && text.contains("—")
            && text.contains("草稿")
            && text.contains("待执行")
            && text.contains("已完成")
            && !text.contains("Current")
    });

    // 分类过滤：Right 切到 Drafts，只显示草稿。
    pty.input("\x1b[C");
    pty.wait_for("Drafts 分类只显示草稿", |screen| {
        let text = cancel_normalize(screen);
        text.contains("fix-config.md")
            && !text.contains("approved.md")
            && !text.contains("completed.md")
    });
    pty.input("\x1b[D");
    pty.wait_for("回到 All 显示全部", |screen| {
        let text = cancel_normalize(screen);
        text.contains("fix-config.md")
            && text.contains("approved.md")
            && text.contains("completed.md")
    });

    // 搜索：多词 AND、大小写无关；Enter 回列表后 query 继续过滤。
    pty.input("/Completed");
    pty.wait_for("搜索命中唯一条目", |screen| {
        let text = cancel_normalize(screen);
        text.contains("completed.md")
            && !text.contains("fix-config.md")
            && !text.contains("approved.md")
    });
    pty.input("\r");
    pty.wait_for("搜索退出但过滤保留", |screen| {
        let text = cancel_normalize(screen);
        text.contains("completed.md") && !text.contains("fix-config.md")
    });
    // 重新进入搜索后 Ctrl+U 才清空 query。
    pty.input("/\x15");
    pty.wait_for("Ctrl+U 清空恢复全部", |screen| {
        let text = cancel_normalize(screen);
        text.contains("fix-config.md")
            && text.contains("approved.md")
            && text.contains("completed.md")
            && !text.contains("/Completed")
    });

    // Enter 打开完整操作菜单；打开本身不调模型，Esc 返回列表。
    pty.input("fix");
    pty.wait_for("选中 fix-config", |screen| {
        let text = cancel_normalize(screen);
        text.contains("fix-config.md") && !text.contains("approved.md")
    });
    pty.input("\r");
    pty.input("\r");
    pty.wait_for("操作菜单打开且显示正文", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Executeinanewsession") && text.contains("SavedraftandleavePlanmode")
            && text.contains(".pp/drafts/fix-config.md")
            && text.contains("更新配置加载")
    });
    pty.input("\x1b");
    pty.wait_for("预览返回列表且保留状态", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Planlist") && text.contains("fix-config.md")
    });

    // F5 刷新与 resize 后列表仍正常。
    pty.input("\x1b[15~");
    pty.wait_for("F5 刷新完成", |screen| {
        cancel_normalize(screen).contains("fix-config.md")
    });
    pty.resize(50, 120);
    pty.wait_for("resize 后列表仍渲染", |screen| {
        cancel_normalize(screen).contains("fix-config.md")
    });

    // 关闭恢复主输入；打开菜单已保存载入会话，但整个浏览过程零模型请求。
    pty.input("\x1b");
    plan_list_wait_closed(&mut pty);
    pty.input("kept draft");
    pty.wait_for("主输入可用且草稿保留", |screen| {
        cancel_normalize(screen).contains("keptdraft")
    });
    no_more(&fixture);
    let sessions = home.path().join(".pp/agent/sessions");
    assert!(std::fs::read_dir(&sessions).unwrap().next().is_some(), "opening the menu saves the selected plan session");
    pty.input("\x03");
    pty.finish();
}

#[test]
fn plan_list_rename_and_delete_are_conflict_safe() {
    let fixture = server();
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    plan_list_seed(project.path());
    std::fs::write(
        project.path().join(".pp/drafts/old-name.md"),
        "# Old plan\n\n- [ ] keep checkbox\n",
    )
    .unwrap();
    std::fs::write(project.path().join(".pp/drafts/taken.md"), "taken bytes").unwrap();
    // 悬空 symlink：无法纳管，操作必须被拒绝且目标不受影响。
    std::os::unix::fs::symlink(
        project.path().join("missing-target.md"),
        project.path().join(".pp/drafts/link.md"),
    )
    .unwrap();
    let mut pty = plan_start(&fixture, home.path(), project.path(), "response");
    pty.ready();
    plan_wait_execute(&mut pty);
    plan_list_open(&mut pty);
    pty.wait_for("草稿条目齐全", |screen| {
        let text = cancel_normalize(screen);
        text.contains("old-name.md") && text.contains("taken.md") && text.contains("link.md")
    });

    // 重命名：预填文件名，Ctrl+U 清空后输入新名；保持正文与 checkbox。
    pty.input("/old-name");
    pty.wait_for("选中 old-name", |screen| {
        cancel_normalize(screen).contains("old-name.md")
    });
    pty.input("\r");
    pty.input("r");
    pty.wait_for("重命名编辑器预填", |screen| {
        cancel_normalize(screen).contains("rename>old-name.md")
    });
    pty.input("\x15renamed-plan.md\r");
    // 重命名后旧 query 不再命中新名：先等 worker 回复落定（空过滤提示），
    // 再清空搜索等待列表更新。
    pty.wait_for("重命名完成且编辑器关闭", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Nomatchingplans") && !text.contains("rename>")
    });
    pty.input("/\x15");
    pty.wait_for("重命名后列表更新", |screen| {
        let text = cancel_normalize(screen);
        text.contains("renamed-plan.md") && !text.contains("old-name.md")
    });
    assert!(!project.path().join(".pp/drafts/old-name.md").exists());
    assert_eq!(
        std::fs::read_to_string(project.path().join(".pp/drafts/renamed-plan.md")).unwrap(),
        "# Old plan\n\n- [ ] keep checkbox\n"
    );

    // 同名无操作成功：编辑器直接关闭，列表与磁盘不变。
    pty.input("r\r");
    pty.wait_for("同名重命名无操作", |screen| {
        !cancel_normalize(screen).contains("rename>")
    });
    assert!(project.path().join(".pp/drafts/renamed-plan.md").exists());

    // 重名冲突：worker 拒绝，磁盘字节不变，旧列表保留。
    pty.input("r\x15taken.md\r");
    pty.wait_for("重名冲突后回到列表", |screen| {
        let text = cancel_normalize(screen);
        !text.contains("rename>") && text.contains("renamed-plan.md")
    });
    assert_eq!(
        std::fs::read_to_string(project.path().join(".pp/drafts/taken.md")).unwrap(),
        "taken bytes"
    );
    assert!(project.path().join(".pp/drafts/renamed-plan.md").exists());

    // 悬空 symlink 是错误条目：确认框不打开，删除不会作用于外部目标。
    pty.input("/link");
    pty.wait_for("选中 symlink 条目", |screen| {
        cancel_normalize(screen).contains("link.md")
    });
    pty.input("\r");
    pty.input("d\x1b[D\r");
    pty.wait_for("symlink 删除被拒绝", |screen| {
        let text = cancel_normalize(screen);
        !text.contains("Deletetheplanfileonly") && text.contains("link.md")
    });
    assert!(
        project
            .path()
            .join(".pp/drafts/link.md")
            .symlink_metadata()
            .is_ok()
    );

    // 关闭列表后真实创建并绑定一份计划（阶段二）。
    pty.input("\x1b");
    plan_list_wait_closed(&mut pty);
    plan_enter(&mut pty, &fixture);
    plan_drive_ready(
        &mut pty,
        &fixture,
        "response",
        "Finalize the canary plan",
        PLAN_TUI_BODY,
    );
    pty.input("\x1b");
    pty.wait_for("关闭审阅面板", |screen| !screen.alternate_screen());
    plan_list_open(&mut pty);
    pty.wait_for("当前绑定标记 Current", |screen| {
        let text = cancel_normalize(screen);
        text.contains("create-canary.md") && text.contains("Current")
    });

    // 确认删除：完整相对路径与提示文案；默认 Cancel。
    pty.input("/create-canary");
    pty.wait_for("选中当前绑定", |screen| {
        cancel_normalize(screen).contains("create-canary.md")
    });
    pty.input("\r");
    pty.input("d");
    pty.wait_for("删除确认框展示路径与文案", |screen| {
        let text = cancel_normalize(screen);
        text.contains(".pp/drafts/create-canary.md")
            && text.contains("Deletetheplanfileonly;codeandsessionsarenotrolledback.")
    });
    pty.input("\r");
    pty.wait_for("默认 Cancel 关闭确认框", |screen| {
        let text = cancel_normalize(screen);
        !text.contains("Deletetheplanfileonly") && text.contains("create-canary.md")
    });
    assert!(project.path().join(".pp/drafts/create-canary.md").exists());

    // 显式选择 Delete 后仅删除计划文件；删除后旧 query 不再命中，
    // 先等 worker 回复落定再清空搜索。
    pty.input("d\x1b[D\r");
    pty.wait_for("删除完成且过滤为空", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Nomatchingplans") && !text.contains("Deletetheplanfileonly")
    });
    pty.input("/\x15");
    pty.wait_for("删除后列表移除条目", |screen| {
        let text = cancel_normalize(screen);
        text.contains("renamed-plan.md") && !text.contains("create-canary.md")
    });
    assert!(!project.path().join(".pp/drafts/create-canary.md").exists());

    // 删除当前绑定只解除绑定：模式保持 Plan，可正常 toggle 回 Execute。
    plan_wait_mode(&mut pty, "Plan");
    no_more(&fixture);
    // Ctrl+U 后仍处于搜索模式：先退出搜索再关闭列表。
    pty.input("\r");
    pty.input("\x1b");
    plan_list_wait_closed(&mut pty);
    pty.input("/plan\r");
    plan_wait_execute(&mut pty);
    no_more(&fixture);
    pty.input("\x03");
    pty.finish();
}

/// 候选会话原生请求中的用户文本（两种协议统一提取）。
fn plan_list_user_texts(protocol: &str, body: &Value) -> Vec<String> {
    let entries = if protocol == "completion" {
        body["messages"].as_array().unwrap()
    } else {
        body["input"].as_array().unwrap()
    };
    entries
        .iter()
        .filter(|entry| entry["role"] == "user")
        .map(|entry| match &entry["content"] {
            Value::String(text) => text.clone(),
            parts => parts
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part["text"].as_str())
                        .collect::<String>()
                })
                .unwrap_or_default(),
        })
        .collect()
}

/// 候选会话的系统提示文本（completion 的 system 消息 / responses 的 instructions）。
fn plan_list_system_text(protocol: &str, body: &Value) -> String {
    if protocol == "completion" {
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["role"] == "system")
            .map(|entry| entry["content"].as_str().unwrap_or_default().to_owned())
            .unwrap_or_default()
    } else {
        body["instructions"].as_str().unwrap_or_default().to_owned()
    }
}

fn plan_list_tool_names(protocol: &str, body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| {
            tool[if protocol == "completion" {
                "function"
            } else {
                "unused"
            }]["name"]
                .as_str()
                .or_else(|| tool["name"].as_str())
                .map(str::to_owned)
        })
        .collect()
}

#[test]
fn plan_list_continues_in_fresh_context() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        pty.ready();
        plan_wait_execute(&mut pty);

        // 另一份草稿先落盘（较早修改时间，列表排序在 sentinel 草稿之后）。
        let other = "# Other draft\n\n- [ ] Other task.\n";
        std::fs::create_dir_all(project.path().join(".pp/drafts")).unwrap();
        std::fs::write(project.path().join(".pp/drafts/other-draft.md"), other).unwrap();

        // 原会话：Plan 模式保存带独有 sentinel 的草稿，然后退出。
        plan_enter(&mut pty, &fixture);
        let draft = "# Continue listed\n\n- [ ] Draft task one.\n- [ ] Draft task two.\n";
        pty.input("SENTINEL-CANARY alpha\r");
        let (request, plan_body) = pty.request_at(&fixture, api_path(protocol));
        let names = plan_list_tool_names(protocol, &plan_body);
        assert!(names.contains(&"plan_write".to_owned()), "{plan_body}");
        assert!(!names.contains(&"bash".to_owned()), "{plan_body}");
        plan_calls(
            request,
            protocol,
            json!([plan_call(
                protocol,
                "listed-write",
                "plan_write",
                json!({"title":"Continue listed","file_name":"continue-listed.md","content":draft,"expected_revision":0})
            )]),
        );
        let (request, _) = pty.request_at(&fixture, api_path(protocol));
        plan_final(request, protocol, "Draft saved.");
        pty.text("Draft saved.");
        pty.completed();
        pty.input("/plan\r");
        plan_wait_execute(&mut pty);
        no_more(&fixture);

        let sessions = home.path().join(".pp/agent/sessions");
        let before = std::fs::read_dir(&sessions).unwrap().count();
        assert!(before >= 1, "source session must be committed");

        // 打开列表：两份草稿都被纳管显示。
        plan_list_open(&mut pty);
        pty.wait_for("两份草稿条目齐全", |screen| {
            let text = cancel_normalize(screen);
            text.contains("continue-listed.md") && text.contains("other-draft.md")
        });
        no_more(&fixture);

        // c：以仅携带计划的新会话继续；未输入前零模型请求。
        pty.input("c");
        pty.text("Opened plan draft");
        plan_wait_mode(&mut pty, "Plan");
        no_more(&fixture);
        let after = std::fs::read_dir(&sessions).unwrap().count();
        assert_eq!(after, before + 1, "candidate session forked");
        // 当前另一份草稿文件保持。
        assert_eq!(
            std::fs::read_to_string(project.path().join(".pp/drafts/other-draft.md")).unwrap(),
            other
        );

        // 下一条输入才发模型请求：唯一前置用户内容是选择动作 + ID + 实际
        // 路径 + 完整快照，不含原聊天 sentinel；系统提示含当前 Plan 规则。
        pty.input("SENTINEL-NOTE continue\r");
        let (request, body) = pty.request_at(&fixture, api_path(protocol));
        let users = plan_list_user_texts(protocol, &body);
        assert_eq!(users.len(), 2, "{body}");
        assert!(users[0].contains("Continue planning the selected Markdown plan below."));
        assert!(users[0].contains("continue-listed.md"));
        assert!(users[0].contains("[Selected Markdown snapshot]"));
        assert!(users[0].contains(draft), "{}", users[0]);
        assert_eq!(users[1], "SENTINEL-NOTE continue");
        for user in &users {
            assert!(!user.contains("SENTINEL-CANARY"), "{user}");
        }
        let system = plan_list_system_text(protocol, &body);
        assert!(system.contains("PLAN MODE"), "{system}");
        assert!(!system.contains("SENTINEL-CANARY"), "{system}");
        let names = plan_list_tool_names(protocol, &body);
        assert!(names.contains(&"plan_write".to_owned()), "{body}");
        assert!(!names.contains(&"bash".to_owned()), "{body}");
        plan_final(request, protocol, "Continued planning.");
        pty.text("Continued planning.");

        // 不另建计划：drafts 仍是两份草稿文件。
        assert_eq!(
            std::fs::read_dir(project.path().join(".pp/drafts"))
                .unwrap()
                .count(),
            2
        );
        no_more(&fixture);
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn plan_list_execution_requires_new_local_approval() {
    for protocol in ["completion", "response"] {
        let fixture = server();
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
        pty.ready();
        plan_wait_execute(&mut pty);

        // done 先写（较早修改时间），drafts 的 listed-exec.md 排在首位。
        std::fs::create_dir_all(project.path().join(".pp/done")).unwrap();
        std::fs::write(
            project.path().join(".pp/done/finished.md"),
            "# Finished\n\n- [x] All done.\n",
        )
        .unwrap();
        std::fs::create_dir_all(project.path().join(".pp/drafts")).unwrap();
        let body = "# Listed exec\n\n- [x] Already verified step.\n- [ ] Remaining canary step.\n";
        std::fs::write(project.path().join(".pp/drafts/listed-exec.md"), body).unwrap();

        plan_list_open(&mut pty);
        pty.wait_for("未登记文件纳管显示", |screen| {
            let text = cancel_normalize(screen);
            text.contains("listed-exec.md") && text.contains("finished.md")
        });
        no_more(&fixture);

        // Done 条目只读：e 没有执行入口，零请求且列表保持。
        pty.input("\x1b[B");
        pty.wait_for("选中 done 条目", |screen| {
            cancel_normalize(screen).contains("finished.md")
        });
        pty.input("e");
        no_more(&fixture);
        pty.wait_for("Done 无执行动作", |screen| {
            let text = cancel_normalize(screen);
            text.contains("Planlist") && text.contains("finished.md")
        });
        pty.input("\x1b[A");
        pty.wait_for("回到 drafts 条目", |screen| {
            cancel_normalize(screen).contains("listed-exec.md")
        });

        // e 打开完整六项菜单；已勾选进度原样展示，打开菜单零模型请求。
        pty.input("e");
        pty.wait_for("完整操作菜单打开", |screen| {
            let text = cancel_normalize(screen);
            text.contains("Executeinanewsession")
                && text.contains("ReviewPlan")
                && text.contains("Alreadyverifiedstep.")
                && text.contains("Remainingcanarystep.")
                && text.contains(".pp/drafts/listed-exec.md")
        });
        no_more(&fixture);

        // 返回列表再进入：取消无代码副作用。
        pty.input("\x1b");
        pty.wait_for("返回列表", |screen| {
            cancel_normalize(screen).contains("Planlist")
        });
        no_more(&fixture);
        assert!(project.path().join(".pp/drafts/listed-exec.md").exists());
        pty.input("e");
        pty.wait_for("再次进入执行审阅", |screen| {
            cancel_normalize(screen).contains("Executeinanewsession")
        });

        // 明确 Execute（固定 Fresh）：新执行会话只带选中计划，已勾选状态
        // 保留；模型请求在本地批准之后才发出。
        plan_choose(&mut pty, 0);
        let (request, exec_body) = pty.request_at(&fixture, api_path(protocol));
        let users = plan_list_user_texts(protocol, &exec_body);
        assert_eq!(users.len(), 1, "{exec_body}");
        assert!(users[0].contains("APPROVED PLAN EXECUTION"), "{}", users[0]);
        assert!(
            users[0].contains("Path: .pp/plans/listed-exec.md"),
            "{}",
            users[0]
        );
        assert!(
            users[0].contains("- [x] Already verified step."),
            "{}",
            users[0]
        );
        assert!(
            users[0].contains("- [ ] Remaining canary step."),
            "{}",
            users[0]
        );
        plan_final(request, protocol, "Executed.");
        pty.text("Executed.");

        // 本地批准把 drafts 搬到 plans；未全部完成不会归档到 done。
        assert!(!project.path().join(".pp/drafts/listed-exec.md").exists());
        assert_eq!(
            std::fs::read_to_string(project.path().join(".pp/plans/listed-exec.md")).unwrap(),
            body
        );
        assert!(!project.path().join(".pp/done/listed-exec.md").exists());
        no_more(&fixture);
        pty.input("\x03");
        pty.finish();
    }
}

#[test]
fn plan_list_enter_opens_six_actions_for_drafts_and_plans_and_only_displays_done() {
    for protocol in ["completion", "response"] {
        for location in ["drafts", "plans", "done", "prose"] {
            let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let directory = if location == "prose" {
                "drafts"
            } else {
                location
            };
            let content = if location == "prose" {
                "# Selected plan\n\nStill discussing the approach.\n"
            } else {
                "# Selected plan\n\n- [x] Already verified.\n- [ ] Remaining work.\n"
            };
            let path = project.path().join(format!(".pp/{directory}/selected.md"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
            pty.ready();
            plan_list_open(&mut pty);
            pty.wait_for("选中计划已加载", |screen| {
                screen.contents().contains("selected.md")
                    && !screen.contents().contains("Loading plans")
            });
            pty.input("\r");
            if location == "done" {
                pty.wait_for("Done 只展示正文", |screen| {
                    let text = screen.contents();
                    text.contains(".pp/done/selected.md")
                        && text.contains("Remaining work.")
                        && text.contains("Read-only view")
                });
                let text = pty.parser.screen().contents();
                assert!(!text.contains("Execute in a new session"), "{text}");
                assert!(!text.contains("Save draft and leave Plan mode"), "{text}");
                for input in ["1\r", "2\r", "3\r", "4\r", "5\r", "6\r"] {
                    pty.input(input);
                }
                no_more(&fixture);
                let sessions = home.path().join(".pp/agent/sessions");
                assert!(
                    !sessions.exists() || std::fs::read_dir(sessions).unwrap().next().is_none()
                );
            } else {
                pty.wait_for("列表 Enter 展示完整六项菜单", |screen| {
                    let text = screen.contents();
                    text.contains("Execute in a new session")
                        && text.contains("Execute (keep context)")
                        && text.contains("Compact context and execute")
                        && text.contains("Review Plan")
                        && text.contains("Continue discussing Plan")
                        && text.contains("Save draft and leave Plan mode")
                });
                assert!(pty.parser.screen().contents().contains("Selected plan"));
                if location == "prose" {
                    plan_choose(&mut pty, 0);
                    no_more(&fixture);
                    assert!(!project.path().join("canary.txt").exists());
                }
            }
            no_more(&fixture);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
            pty.input("\x1b");
            pty.wait_for("返回列表", |screen| {
                cancel_normalize(screen).contains("Planlist")
            });
            pty.input("\x03");
            pty.finish();
        }
    }
}

#[test]
fn plan_list_six_actions_use_the_selected_plan_and_leave_the_list() {
    for protocol in ["completion", "response"] {
        for action in [1, 2, 3, 4, 5] {
            let fixture = server();
            let home = tempfile::tempdir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let location = if action % 2 == 0 { "plans" } else { "drafts" };
            let path = project.path().join(format!(".pp/{location}/selected.md"));
            let content = "# Selected actions\n\n- [x] Verified item.\n- [ ] Remaining item.\n";
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            let mut pty = plan_start(&fixture, home.path(), project.path(), protocol);
            pty.ready();
            plan_list_open(&mut pty);
            pty.wait_for("选中计划已加载", |screen| {
                screen.contents().contains("selected.md")
                    && !screen.contents().contains("Loading plans")
            });
            pty.input("\r");
            pty.wait_for("菜单就绪", |screen| {
                screen
                    .contents()
                    .contains("Save draft and leave Plan mode")
            });
            plan_choose(&mut pty, action);
            if action == 5 {
                plan_wait_execute(&mut pty);
                pty.ready();
                no_more(&fixture);
                assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
            } else {
                if action == 4 {
                    pty.input("Clarify the remaining item\r");
                }
                let (request, body) = pty.request_at(&fixture, api_path(protocol));
                let encoded = body.to_string();
                assert!(encoded.contains("Selected actions"), "{body}");
                assert_eq!(
                    encoded.contains("[Approved Markdown snapshot]"),
                    action <= 2,
                    "{body}"
                );
                if action == 4 {
                    assert_eq!(
                        cancel_user_messages(&body, protocol).last().unwrap(),
                        "Clarify the remaining item"
                    );
                }
                let request = if action == 4 {
                    plan_calls(
                        request,
                        protocol,
                        json!([plan_call(
                            protocol,
                            "discuss-write",
                            "plan_write",
                            json!({"title":"Selected actions", "file_name":"selected.md", "expected_revision":1, "content":format!("{content}\nClarified acceptance criteria.\n")})
                        )]),
                    );
                    let (request, body) = pty.request_at(&fixture, api_path(protocol));
                    assert_eq!(
                        cancel_tool_outputs(&body, protocol).last().unwrap().1["ok"],
                        true,
                        "{body}"
                    );
                    assert!(
                        std::fs::read_to_string(&path)
                            .unwrap()
                            .contains("Clarified acceptance criteria.")
                    );
                    request
                } else {
                    request
                };
                plan_final(
                    request,
                    protocol,
                    if action == 3 {
                        "List reviewer findings"
                    } else {
                        "List action finished"
                    },
                );
                if action == 3 {
                    pty.wait_for("审查结束显示报告与完整菜单", |screen| {
                        let text = screen.contents();
                        text.contains("List reviewer findings")
                            && text.contains("Save draft and leave Plan mode")
                    });
                } else {
                    pty.text("List action finished");
                    pty.ready();
                    assert!(!cancel_normalize(pty.parser.screen()).contains("Planlist"));
                }
            }
            no_more(&fixture);
            pty.input("\x03");
            pty.finish();
        }
    }
}
