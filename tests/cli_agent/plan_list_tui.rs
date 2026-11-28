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

    // 打开列表：三个目录的标题、紧凑时间与 All 分类标记。
    plan_list_open(&mut pty);
    pty.wait_for("三目录条目齐全", |screen| {
        let text = cancel_normalize(screen);
        text.contains("修复配置")
            && !text.contains("0/2")
            && text.contains("Approvedplan")
            && text.contains("Completedplan")
            && text.contains("justnow")
            && text.contains("[Draft]")
            && text.contains("[Plan]")
            && text.contains("[Done]")
            && text.contains("Search:Press/tosearch")
            && !text.contains("Current")
    });

    // 分类过滤：Right 切到 Drafts，只显示草稿。
    pty.input("\x1b[C");
    pty.wait_for("Drafts 分类只显示草稿", |screen| {
        let text = cancel_normalize(screen);
        text.contains("修复配置")
            && !text.contains("Approvedplan")
            && !text.contains("Completedplan")
            && !text.contains("[Draft]")
    });
    pty.input("\x1b[D");
    pty.wait_for("回到 All 显示全部", |screen| {
        let text = cancel_normalize(screen);
        text.contains("修复配置")
            && text.contains("Approvedplan")
            && text.contains("Completedplan")
    });

    // 搜索：多词 AND、大小写无关；Enter 回列表后 query 继续过滤。
    pty.input("/Completed");
    pty.wait_for("搜索命中唯一条目", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Completedplan")
            && !text.contains("修复配置")
            && !text.contains("Approvedplan")
    });
    pty.input("\r");
    pty.wait_for("搜索退出但过滤保留", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Completedplan") && !text.contains("修复配置")
            && text.contains("Search:Completed")
    });
    // 重新进入搜索后 Ctrl+U 才清空 query。
    pty.input("/\x15");
    pty.wait_for("Ctrl+U 清空恢复全部", |screen| {
        let text = cancel_normalize(screen);
        text.contains("修复配置")
            && text.contains("Approvedplan")
            && text.contains("Completedplan")
            && !text.contains("Search:Completed")
    });

    // Enter 打开完整操作菜单；打开本身不调模型，Esc 返回列表。
    pty.input("修复");
    pty.wait_for("选中 fix-config", |screen| {
        let text = cancel_normalize(screen);
        text.contains("修复配置") && !text.contains("Approvedplan")
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
        text.contains("Planlist") && text.contains("修复配置")
            && text.contains("Search:修复")
    });

    // F5 刷新与 resize 后列表仍正常。
    pty.input("\x1b[15~");
    pty.wait_for("F5 刷新完成", |screen| {
        let text = cancel_normalize(screen);
        text.contains("修复配置") && text.contains("Search:修复")
    });
    pty.resize(50, 120);
    pty.wait_for("resize 后列表仍渲染", |screen| {
        cancel_normalize(screen).contains("修复配置")
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
fn plan_list_execution_requires_new_local_approval() {
    let protocol = "response";
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
            text.contains("Listedexec") && text.contains("Finished")
        });
        no_more(&fixture);

        // Done 条目只读：e 没有执行入口，零请求且列表保持。
        pty.input("\x1b[B");
        pty.wait_for("选中 done 条目", |screen| {
            cancel_normalize(screen).contains("Finished")
        });
        pty.input("e");
        no_more(&fixture);
        pty.wait_for("Done 无执行动作", |screen| {
            let text = cancel_normalize(screen);
            text.contains("Planlist") && text.contains("Finished")
        });
        pty.input("\x1b[A");
        pty.wait_for("回到 drafts 条目", |screen| {
            cancel_normalize(screen).contains("Listedexec")
        });

        // e 打开完整五项菜单；已勾选进度原样展示，打开菜单零模型请求。
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
        plan_final(
            request,
            protocol,
            "Executed. Shall we continue with the remaining canary step?",
        );
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

