use super::*;

#[test]
fn changes_replace_write_edit_uses_normalized_first_to_last_baseline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("demo.txt");
    let original = "user-local\r\nkeep";
    fs::write(&path, original).unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    tools.begin_changes_task();
    let initial = tools.changes_view();
    assert!(initial.files.is_empty());
    for (name, args) in [
        (
            "edit",
            json!({"path":"./demo.txt","edits":[{"oldText":"user-local","newText":"agent-one"}]}),
        ),
        (
            "write",
            json!({"path":"nested/../demo.txt","content":"agent-two\r\nkeep"}),
        ),
        (
            "edit",
            json!({"path":path,"edits":[{"oldText":"agent-two","newText":"agent-final"}]}),
        ),
    ] {
        let result = execute(&tools, name, args);
        assert_eq!(result["ok"], true, "{result}");
    }
    let view = tools.changes_view();
    assert_eq!(view.task_id, initial.task_id);
    assert_eq!(view.revision, initial.revision + 3);
    assert_eq!(view.files.len(), 1);
    let file = &view.files[0];
    assert_eq!(file.path, Path::new("demo.txt"));
    assert_eq!(
        serde_json::to_value(&file.diff).unwrap(),
        serde_json::to_value(diff::build(Some(original), "agent-final\r\nkeep")).unwrap()
    );
    let token = file.undo.as_ref().unwrap();
    assert_eq!(token.task_id, view.task_id);
    assert_eq!(token.revision, view.revision);
    assert_eq!(token.path, Path::new("demo.txt"));
    assert_eq!(fs::read(&path).unwrap(), b"agent-final\r\nkeep");

    let result = execute(
        &tools,
        "write",
        json!({"path":"demo.txt","content":original}),
    );
    assert_eq!(result["ok"], true, "{result}");
    assert!(tools.changes_view().files.is_empty());
    assert_eq!(fs::read(&path).unwrap(), original.as_bytes());
}

#[test]
fn changes_undo_restores_exact_original_bytes() {
    for original in [
        b"user-local\r\nkeep".as_slice(),
        b"\xef\xbb\xbfuser-local\r\nkeep".as_slice(),
        b"\xff\0user-local\r\nkeep\xfe".as_slice(),
        b"".as_slice(),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.txt");
        fs::write(&path, original).unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        tools.begin_changes_task();
        for (name, args) in [
            (
                "write",
                json!({"path":"./demo.txt","content":"agent-one\r\nkeep"}),
            ),
            (
                "edit",
                json!({"path":"demo.txt","edits":[{"oldText":"agent-one","newText":"agent-final"}]}),
            ),
        ] {
            let result = execute(&tools, name, args);
            assert_eq!(result["ok"], true, "{result}");
        }
        let view = tools.changes_view();
        assert_eq!(view.files.len(), 1);
        let token = view.files[0].undo.as_ref().unwrap();
        let undone = tools.undo_file(token).unwrap();
        assert!(undone.warning.is_none());
        assert!(undone.view.files.is_empty());
        assert_eq!(undone.view.task_id, view.task_id);
        assert_eq!(undone.view.revision, view.revision + 1);
        assert_eq!(fs::read(&path).unwrap(), original);
    }
}

#[test]
fn changes_undo_removes_only_task_created_empty_and_nonempty_files() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    for (path, content) in [("empty.txt", ""), ("nested/created.txt", "created")] {
        let result = execute(&tools, "write", json!({"path":path,"content":content}));
        assert_eq!(result["ok"], true, "{result}");
    }
    let first = tools.changes_view();
    let undone = tools
        .undo_file(first.files[0].undo.as_ref().unwrap())
        .unwrap();
    assert_eq!(undone.view.files.len(), 1);
    assert!(!dir.path().join("empty.txt").exists());
    assert_eq!(
        fs::read(dir.path().join("nested/created.txt")).unwrap(),
        b"created"
    );
    tools
        .undo_file(undone.view.files[0].undo.as_ref().unwrap())
        .unwrap();
    assert!(tools.changes_view().files.is_empty());
    assert!(!dir.path().join("nested/created.txt").exists());
    assert!(dir.path().join("nested").is_dir());
}

#[test]
fn changes_undo_refuses_external_bytes_but_refresh_can_make_it_ready_again() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("demo.txt");
    fs::write(&path, b"user-local\r\nkeep").unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = execute(
        &tools,
        "write",
        json!({"path":"demo.txt","content":"latest"}),
    );
    assert_eq!(result["ok"], true, "{result}");
    let view = tools.changes_view();
    let token = view.files[0].undo.as_ref().unwrap();
    fs::write(&path, b"latest external").unwrap();
    assert_eq!(
        tools.undo_file(token).unwrap_err().to_string(),
        "File changed outside this task; undo refused."
    );
    assert_eq!(fs::read(&path).unwrap(), b"latest external");
    let blocked = tools.changes_view();
    assert_eq!(blocked.revision, view.revision);
    assert_eq!(blocked.files.len(), 1);
    assert!(blocked.files[0].undo.is_err());
    assert_eq!(
        serde_json::to_value(&blocked.files[0].diff).unwrap(),
        serde_json::to_value(&view.files[0].diff).unwrap()
    );
    fs::write(&path, b"latest").unwrap();
    let ready = tools.changes_view();
    assert_eq!(ready.files[0].undo.as_ref().unwrap(), token);
    tools.undo_file(token).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"user-local\r\nkeep");
}

#[test]
fn changes_undo_permanently_blocks_interleaved_tool_writes() {
    for disappeared in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("demo.txt");
        fs::write(&path, b"original").unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let first = execute(
            &tools,
            "write",
            json!({"path":"demo.txt","content":"first"}),
        );
        assert_eq!(first["ok"], true, "{first}");
        if disappeared {
            fs::remove_file(&path).unwrap();
        } else {
            fs::write(&path, b"external").unwrap();
        }
        let second = execute(
            &tools,
            "write",
            json!({"path":"demo.txt","content":"original"}),
        );
        assert_eq!(second["ok"], true, "{second}");
        let view = tools.changes_view();
        assert_eq!(view.files.len(), 1);
        assert!(
            view.files[0]
                .undo
                .as_ref()
                .unwrap_err()
                .contains("between tool writes")
        );
        let token = UndoToken {
            task_id: view.task_id,
            revision: view.revision,
            path: "demo.txt".into(),
        };
        assert!(
            tools
                .undo_file(&token)
                .unwrap_err()
                .to_string()
                .contains("between tool writes")
        );
        assert_eq!(tools.changes_view().revision, view.revision);
        assert_eq!(fs::read(&path).unwrap(), b"original");
    }
}

#[test]
fn changes_undo_stale_and_repeated_tokens_never_mutate_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("demo.txt");
    fs::write(&path, b"original").unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = execute(
        &tools,
        "write",
        json!({"path":"demo.txt","content":"first"}),
    );
    assert_eq!(result["ok"], true, "{result}");
    let view = tools.changes_view();
    let token = view.files[0].undo.as_ref().unwrap();
    for invalid in [
        UndoToken {
            task_id: token.task_id + 1,
            ..token.clone()
        },
        UndoToken {
            revision: token.revision + 1,
            ..token.clone()
        },
        UndoToken {
            path: "untracked.txt".into(),
            ..token.clone()
        },
        UndoToken {
            path: "../outside.txt".into(),
            ..token.clone()
        },
    ] {
        assert_eq!(
            tools.undo_file(&invalid).unwrap_err().to_string(),
            "Changes are stale; reopen /changes."
        );
        assert_eq!(tools.changes_view().revision, view.revision);
        assert_eq!(fs::read(&path).unwrap(), b"first");
    }
    let second = execute(
        &tools,
        "write",
        json!({"path":"other.txt","content":"other"}),
    );
    assert_eq!(second["ok"], true, "{second}");
    assert_eq!(
        tools.undo_file(token).unwrap_err().to_string(),
        "Changes are stale; reopen /changes."
    );
    assert_eq!(fs::read(&path).unwrap(), b"first");
    let current = tools.changes_view();
    let current_token = current.files[0].undo.as_ref().unwrap();
    let undone = tools.undo_file(current_token).unwrap();
    assert_eq!(undone.view.files.len(), 1);
    assert_eq!(fs::read(&path).unwrap(), b"original");
    assert_eq!(
        tools.undo_file(current_token).unwrap_err().to_string(),
        "Changes are stale; reopen /changes."
    );
    assert_eq!(fs::read(&path).unwrap(), b"original");
    assert_eq!(tools.changes_view().revision, undone.view.revision);
    assert_eq!(fs::read(dir.path().join("other.txt")).unwrap(), b"other");
}

#[test]
fn changes_undo_previous_task_token_cannot_match_a_new_record() {
    let dir = tempfile::tempdir().unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = execute(
        &tools,
        "write",
        json!({"path":"demo.txt","content":"first"}),
    );
    assert_eq!(result["ok"], true, "{result}");
    let view = tools.changes_view();
    let token = view.files[0].undo.as_ref().unwrap();
    for clear in [true, false] {
        if clear {
            tools.clear_changes();
        } else {
            tools.begin_changes_task();
        }
        let result = execute(&tools, "write", json!({"path":"demo.txt","content":"next"}));
        assert_eq!(result["ok"], true, "{result}");
        let current = tools.changes_view();
        assert_eq!(
            tools.undo_file(token).unwrap_err().to_string(),
            "Changes are stale; reopen /changes."
        );
        assert_eq!(fs::read(dir.path().join("demo.txt")).unwrap(), b"next");
        assert_eq!(tools.changes_view().revision, current.revision);
    }
}

#[cfg(unix)]
#[test]
fn changes_undo_reacquires_target_and_rejects_missing_or_unsafe_paths() {
    use std::os::unix::fs::symlink;

    for replacement in [
        "missing",
        "missing-parent",
        "symlink",
        "parent-symlink",
        "directory",
        "fifo",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("demo.txt"), b"latest").unwrap();
        let path = dir.path().join("nested/demo.txt");
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(&path, b"original").unwrap();
        let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
        let result = execute(
            &tools,
            "write",
            json!({"path":"nested/demo.txt","content":"latest"}),
        );
        assert_eq!(result["ok"], true, "{result}");
        let view = tools.changes_view();
        let token = view.files[0].undo.as_ref().unwrap();
        if replacement == "parent-symlink" {
            fs::rename(dir.path().join("nested"), dir.path().join("saved")).unwrap();
            symlink(outside.path(), dir.path().join("nested")).unwrap();
        } else {
            fs::remove_file(&path).unwrap();
            match replacement {
                "missing" => {}
                "missing-parent" => fs::remove_dir(dir.path().join("nested")).unwrap(),
                "symlink" => symlink(outside.path().join("demo.txt"), &path).unwrap(),
                "directory" => fs::create_dir(&path).unwrap(),
                "fifo" => {
                    assert!(
                        std::process::Command::new("mkfifo")
                            .arg(&path)
                            .status()
                            .unwrap()
                            .success()
                    );
                }
                _ => unreachable!(),
            }
        }
        assert_eq!(
            tools.undo_file(token).unwrap_err().to_string(),
            "File changed outside this task; undo refused.",
            "{replacement}"
        );
        let blocked = tools.changes_view();
        assert_eq!(blocked.revision, view.revision);
        assert_eq!(blocked.files.len(), 1);
        assert!(blocked.files[0].undo.is_err());
        assert_eq!(
            fs::read(outside.path().join("demo.txt")).unwrap(),
            b"latest"
        );
        match replacement {
            "missing" => assert!(!path.exists()),
            "missing-parent" => assert!(!dir.path().join("nested").exists()),
            "symlink" => assert!(fs::symlink_metadata(&path).unwrap().is_symlink()),
            "parent-symlink" => {
                assert!(
                    fs::symlink_metadata(dir.path().join("nested"))
                        .unwrap()
                        .is_symlink()
                );
                assert_eq!(
                    fs::read(dir.path().join("saved/demo.txt")).unwrap(),
                    b"latest"
                );
            }
            "directory" => assert!(path.is_dir()),
            "fifo" => {
                use std::os::unix::fs::FileTypeExt;
                assert!(fs::symlink_metadata(&path).unwrap().file_type().is_fifo());
            }
            _ => unreachable!(),
        }
    }
}

#[cfg(unix)]
#[test]
fn changes_undo_unknown_baseline_remains_unavailable_after_permissions_return() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::getuid().as_raw() == 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("demo.txt");
    fs::write(&path, b"original").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = execute(
        &tools,
        "write",
        json!({"path":"demo.txt","content":"latest"}),
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    let view = tools.changes_view();
    assert_eq!(view.files.len(), 1);
    assert!(
        view.files[0]
            .undo
            .as_ref()
            .unwrap_err()
            .contains("could not be read")
    );
    let token = UndoToken {
        task_id: view.task_id,
        revision: view.revision,
        path: "demo.txt".into(),
    };
    assert!(
        tools
            .undo_file(&token)
            .unwrap_err()
            .to_string()
            .contains("could not be read")
    );
    assert_eq!(tools.changes_view().revision, view.revision);
    assert_eq!(fs::read(&path).unwrap(), b"latest");
}

#[cfg(unix)]
#[test]
fn changes_undo_unreadable_current_file_keeps_record_until_readable_again() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::getuid().as_raw() == 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("demo.txt");
    fs::write(&path, b"original").unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let result = execute(
        &tools,
        "write",
        json!({"path":"demo.txt","content":"latest"}),
    );
    assert_eq!(result["ok"], true, "{result}");
    let view = tools.changes_view();
    let token = view.files[0].undo.as_ref().unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
    let refused = tools.undo_file(token).unwrap_err();
    let blocked = tools.changes_view();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        refused.to_string(),
        "File changed outside this task; undo refused."
    );
    assert_eq!(blocked.revision, view.revision);
    assert_eq!(blocked.files.len(), 1);
    assert!(blocked.files[0].undo.is_err());
    assert_eq!(fs::read(&path).unwrap(), b"latest");
    assert_eq!(tools.changes_view().files[0].undo.as_ref().unwrap(), token);
    tools.undo_file(token).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"original");
}

#[test]
fn changes_failed_edit_and_cancelled_or_unauthorized_write_do_not_record() {
    let workflow = WorkflowState::default();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("demo.txt");
    fs::write(&path, b"user-local\n").unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    tools.begin_changes_task();
    let initial = tools.changes_view();
    let failed = execute(
        &tools,
        "edit",
        json!({"path":"demo.txt","edits":[{"oldText":"absent","newText":"bad"}]}),
    );
    assert_eq!(failed["ok"], false, "{failed}");
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let document = plan_document();
    for (policy, token) in [
        (
            ToolPolicy::Execute {
                workflow: &workflow,
            },
            cancellation,
        ),
        (
            ToolPolicy::Plan {
                document: &document,
                sealed: false,
                input: None,
            },
            CancellationToken::new(),
        ),
    ] {
        let execution = tools.execute(
            "write",
            &json!({"path":"demo.txt","content":"bad"}).to_string(),
            "refused-write",
            policy,
            &token,
            &mut |_| {},
        );
        let result: Value = serde_json::from_str(&execution.output).unwrap();
        assert_eq!(result["ok"], false, "{result}");
    }
    let view = tools.changes_view();
    assert!(view.files.is_empty());
    assert_eq!(view.task_id, initial.task_id);
    assert_eq!(view.revision, initial.revision);
    assert_eq!(fs::read(path).unwrap(), b"user-local\n");
}

#[cfg(unix)]
#[test]
fn changes_failed_save_preserves_existing_record_and_revision() {
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::getuid().as_raw() == 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("locked");
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("demo.txt"), b"user-local").unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let committed = execute(
        &tools,
        "write",
        json!({"path":"locked/demo.txt","content":"agent-one"}),
    );
    assert_eq!(committed["ok"], true, "{committed}");
    let before = tools.changes_view();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o500)).unwrap();
    assert!(
        tools
            .undo_file(before.files[0].undo.as_ref().unwrap())
            .is_err()
    );
    for (name, args) in [
        ("write", json!({"path":"locked/demo.txt","content":"bad"})),
        (
            "edit",
            json!({"path":"locked/demo.txt","edits":[{"oldText":"agent-one","newText":"bad"}]}),
        ),
        ("write", json!({"path":"locked/new.txt","content":"bad"})),
    ] {
        let result = execute(&tools, name, args);
        assert_eq!(result["ok"], false, "{result}");
    }
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    let after = tools.changes_view();
    assert_eq!(after.task_id, before.task_id);
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.files.len(), 1);
    assert_eq!(
        serde_json::to_value(&after.files[0].diff).unwrap(),
        serde_json::to_value(&before.files[0].diff).unwrap()
    );
    assert!(after.files[0].undo.is_ok());
    assert_eq!(fs::read(parent.join("demo.txt")).unwrap(), b"agent-one");
    assert!(!parent.join("new.txt").exists());
}

#[test]
fn changes_hashline_commits_keep_first_baseline_and_reject_stale_edit() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("demo.txt"), b"user-local\r\nkeep").unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Hashline).unwrap();
    tools.begin_changes_task();
    let original = read_content(&tools, "demo.txt").0;
    for content in ["agent-one", "agent-final"] {
        let read = read_content(&tools, "demo.txt").0;
        let result = execute(
            &tools,
            "edit",
            hashline_args(&read, &format!("PUT 1.=1:\n+{content}")),
        );
        assert_eq!(result["ok"], true, "{result}");
    }
    let view = tools.changes_view();
    assert_eq!(view.files.len(), 1);
    assert_eq!(view.files[0].path, Path::new("demo.txt"));
    assert_eq!(
        serde_json::to_value(&view.files[0].diff).unwrap(),
        serde_json::to_value(diff::build(
            Some("user-local\r\nkeep"),
            "agent-final\r\nkeep"
        ))
        .unwrap()
    );
    assert!(view.files[0].undo.is_ok());
    let stale = execute(&tools, "edit", hashline_args(&original, "PUT 1.=1:\n+bad"));
    assert_eq!(stale["ok"], false, "{stale}");
    let after = tools.changes_view();
    assert_eq!(after.revision, view.revision);
    assert_eq!(after.files.len(), 1);
    assert_eq!(
        fs::read(dir.path().join("demo.txt")).unwrap(),
        b"agent-final\r\nkeep"
    );
}

#[test]
fn file_changed_follows_commit_and_never_changes_model_tool_result() {
    let workflow = WorkflowState::default();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("demo.txt"), b"old\nkeep\n").unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let token = CancellationToken::new();
    let mut events = Vec::new();
    let output = tools
        .execute(
            "edit",
            &json!({"path":"demo.txt","edits":[{"oldText":"old","newText":"new"}]}).to_string(),
            "edit-1",
            ToolPolicy::Execute {
                workflow: &workflow,
            },
            &token,
            &mut |event| {
                if matches!(event, AgentEvent::FileChanged { .. }) {
                    assert_eq!(
                        fs::read(dir.path().join("demo.txt")).unwrap(),
                        b"new\nkeep\n"
                    );
                    let view = tools.changes_view();
                    assert_eq!(view.files.len(), 1);
                    assert_eq!(view.files[0].path, Path::new("demo.txt"));
                    assert!(view.files[0].undo.is_ok());
                }
                events.push(event);
            },
        )
        .output;
    assert!(matches!(
        &events[..],
        [
            AgentEvent::ToolStarted { .. },
            AgentEvent::FileChanged {
                diff: crate::events::FileDiff::Text {
                    added: 1,
                    removed: 1,
                    created: false,
                    ..
                },
                ..
            },
            AgentEvent::ToolFinished { .. }
        ]
    ));
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!({"ok":true,"edits_applied":1,"bytes_written":9})
    );
    events.clear();
    tools.execute(
        "edit",
        &json!({"path":"demo.txt","edits":[{"oldText":"missing","newText":"bad"}]}).to_string(),
        "edit-fail",
        ToolPolicy::Execute {
            workflow: &workflow,
        },
        &token,
        &mut |event| events.push(event),
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::FileChanged { .. }))
    );
    fs::write(dir.path().join("binary.txt"), [0xff, 0xfe]).unwrap();
    events.clear();
    let output = tools
        .execute(
            "write",
            &json!({"path":"binary.txt","content":"text\n"}).to_string(),
            "write-binary",
            ToolPolicy::Execute {
                workflow: &workflow,
            },
            &token,
            &mut |event| events.push(event),
        )
        .output;
    assert_eq!(fs::read(dir.path().join("binary.txt")).unwrap(), b"text\n");
    assert_eq!(
        serde_json::from_str::<Value>(&output).unwrap(),
        json!({"ok":true,"bytes_written":5})
    );
    assert!(
        matches!(&events[1], AgentEvent::FileChanged { diff: crate::events::FileDiff::Unavailable { reason }, .. } if reason == "Previous content is not UTF-8; text diff unavailable.")
    );
}

#[cfg(unix)]
#[test]
fn write_snapshot_permission_failure_does_not_block_commit() {
    let workflow = WorkflowState::default();
    use std::os::unix::fs::PermissionsExt;
    if rustix::process::getuid().as_raw() == 0 {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unreadable.txt");
    fs::write(&path, b"old").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();
    let tools = Tools::new(dir.path().into(), EditMode::Replace).unwrap();
    let mut events = Vec::new();
    let output = tools
        .execute(
            "write",
            &json!({"path":"unreadable.txt","content":"new"}).to_string(),
            "unreadable",
            ToolPolicy::Execute {
                workflow: &workflow,
            },
            &CancellationToken::new(),
            &mut |event| events.push(event),
        )
        .output;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"new");
    assert_eq!(serde_json::from_str::<Value>(&output).unwrap()["ok"], true);
    assert!(
        matches!(&events[1], AgentEvent::FileChanged { diff: crate::events::FileDiff::Unavailable { reason }, .. } if reason == "Previous content could not be read; text diff unavailable.")
    );
}
