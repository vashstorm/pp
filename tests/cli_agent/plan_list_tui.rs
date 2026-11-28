// Real TUI coverage for the /plan-list management page: list/category/search/preview/zero requests,
// plus conflict-safe rename and delete.
// Reuses the real binary + PTY + loopback fixture from plan_tui.rs (same include module shares helpers).

fn plan_list_seed(project: &Path) {
    for dir in ["drafts", "plans", "done"] {
        std::fs::create_dir_all(project.join(".pp").join(dir)).unwrap();
    }
    std::fs::write(
        project.join(".pp/drafts/fix-config.md"),
        "# Fix config\n\n- [ ] Update config loading\n- [ ] Read back to verify\n",
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
    pty.wait_for("plan management page opens", |screen| {
        cancel_normalize(screen).contains("Planlist")
    });
}

fn plan_list_wait_closed(pty: &mut Pty) {
    pty.wait_for("management page closes and restores the main input", |screen| {
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

    // Open the list: titles from all three directories, compact time and the All category marker.
    plan_list_open(&mut pty);
    pty.wait_for("entries from all three directories are present", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Fixconfig")
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

    // Category filter: Right switches to Drafts, which shows only drafts.
    pty.input("\x1b[C");
    pty.wait_for("Drafts category shows only drafts", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Fixconfig")
            && !text.contains("Approvedplan")
            && !text.contains("Completedplan")
            && !text.contains("[Draft]")
    });
    pty.input("\x1b[D");
    pty.wait_for("back on All, everything is shown", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Fixconfig")
            && text.contains("Approvedplan")
            && text.contains("Completedplan")
    });

    // Search: multi-word AND, case-insensitive; after Enter returns to the list the query keeps filtering.
    pty.input("/Completed");
    pty.wait_for("search matches a single entry", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Completedplan")
            && !text.contains("Fixconfig")
            && !text.contains("Approvedplan")
    });
    pty.input("\r");
    pty.wait_for("search exits but the filter persists", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Completedplan") && !text.contains("Fixconfig")
            && text.contains("Search:Completed")
    });
    // Only Ctrl+U after re-entering search clears the query.
    pty.input("/\x15");
    pty.wait_for("Ctrl+U clears the query and restores all entries", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Fixconfig")
            && text.contains("Approvedplan")
            && text.contains("Completedplan")
            && !text.contains("Search:Completed")
    });

    // Enter opens the full action menu; opening it does not call the model, Esc returns to the list.
    pty.input("Fix");
    pty.wait_for("fix-config is selected", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Fixconfig") && !text.contains("Approvedplan")
    });
    pty.input("\r");
    pty.input("\r");
    pty.wait_for("action menu opens and shows the body", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Executeinanewsession") && text.contains("SavedraftandleavePlanmode")
            && text.contains(".pp/drafts/fix-config.md")
            && text.contains("Updateconfigloading")
    });
    pty.input("\x1b");
    pty.wait_for("preview returns to the list and keeps state", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Planlist") && text.contains("Fixconfig")
            && text.contains("Search:Fix")
    });

    // The list still works after an F5 refresh and a resize.
    pty.input("\x1b[15~");
    pty.wait_for("F5 refresh completes", |screen| {
        let text = cancel_normalize(screen);
        text.contains("Fixconfig") && text.contains("Search:Fix")
    });
    pty.resize(50, 120);
    pty.wait_for("list still renders after resize", |screen| {
        cancel_normalize(screen).contains("Fixconfig")
    });

    // Closing restores the main input; opening the menu saved the loaded session, but browsing makes
    // zero model requests.
    pty.input("\x1b");
    plan_list_wait_closed(&mut pty);
    pty.input("kept draft");
    pty.wait_for("main input is usable and the draft is kept", |screen| {
        cancel_normalize(screen).contains("keptdraft")
    });
    no_more(&fixture);
    let sessions = home.path().join(".pp/agent/sessions");
    assert!(std::fs::read_dir(&sessions).unwrap().next().is_some(), "opening the menu saves the selected plan session");
    pty.input("\x03");
    pty.finish();
}

/// User text in the native requests of the candidate session (extracted uniformly for both protocols).
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

/// System prompt text of the candidate session (the system message for completion / instructions for responses).
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

        // done is written first (older modification time), so listed-exec.md in drafts sorts first.
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
        pty.wait_for("unregistered file is picked up and displayed", |screen| {
            let text = cancel_normalize(screen);
            text.contains("Listedexec") && text.contains("Finished")
        });
        no_more(&fixture);

        // Done entries are read-only: `e` has no execution entry point, so zero requests and the list stays.
        pty.input("\x1b[B");
        pty.wait_for("done entry is selected", |screen| {
            cancel_normalize(screen).contains("Finished")
        });
        pty.input("e");
        no_more(&fixture);
        pty.wait_for("Done has no execution action", |screen| {
            let text = cancel_normalize(screen);
            text.contains("Planlist") && text.contains("Finished")
        });
        pty.input("\x1b[A");
        pty.wait_for("back on the drafts entry", |screen| {
            cancel_normalize(screen).contains("Listedexec")
        });

        // `e` opens the full five-item menu; completed progress is shown as-is, and opening the menu makes
        // zero model requests.
        pty.input("e");
        pty.wait_for("full action menu opens", |screen| {
            let text = cancel_normalize(screen);
            text.contains("Executeinanewsession")
                && text.contains("ReviewPlan")
                && text.contains("Alreadyverifiedstep.")
                && text.contains("Remainingcanarystep.")
                && text.contains(".pp/drafts/listed-exec.md")
        });
        no_more(&fixture);

        // Return to the list and enter again: cancelling has no code side effects.
        pty.input("\x1b");
        pty.wait_for("returns to the list", |screen| {
            cancel_normalize(screen).contains("Planlist")
        });
        no_more(&fixture);
        assert!(project.path().join(".pp/drafts/listed-exec.md").exists());
        pty.input("e");
        pty.wait_for("execution review opens again", |screen| {
            cancel_normalize(screen).contains("Executeinanewsession")
        });

        // Explicit Execute (always Fresh): the new execution session carries only the selected plan and
        // keeps completed progress; the model request is sent only after local approval.
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

        // Local approval moves drafts to plans; an unfinished plan is not archived to done.
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

