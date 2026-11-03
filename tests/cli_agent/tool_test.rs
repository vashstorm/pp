use super::*;

fn session_jsonl(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let dir = home.join(".pp/agent/sessions");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect()
}

fn fixture_directories(cwd: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(cwd)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".pp-test-tools-"))
        })
        .collect()
}

#[test]
fn tool_test_line_mode_edit_and_grep_are_real_repeatable_and_clean() {
    for mode in ["replace", "hashline"] {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "project content\n").unwrap();
        let settings = format!("{}edit: {mode}\n", fixture_settings());
        let mut cli = Cli::start_with(dir.path(), None, &settings, &fixture_models(&server));
        cli.input("/test-edit\n/test-edit\n/test-grep\n/quit\n");
        let output = cli.finish();
        assert!(output.status.success(), "{output:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert_eq!(
            stderr.matches("Local tool suite complete:").count(),
            3,
            "{stderr}"
        );
        assert!(
            stderr.contains("file edit/replace.txt: modified"),
            "{stderr}"
        );
        assert!(
            stderr.contains("file edit/hashline.txt: modified"),
            "{stderr}"
        );
        assert!(stderr.contains("hashline snapshot"), "{stderr}");
        assert!(
            stderr.contains("[expected fixture failure] edit"),
            "{stderr}"
        );
        assert!(stderr.contains("needle first"), "{stderr}");
        assert!(stderr.contains("needle second"), "{stderr}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
            "project content\n"
        );
        assert!(fixture_directories(dir.path()).is_empty());
        assert!(session_jsonl(dir.path()).is_empty());
        no_more(&server);
    }
}

#[test]
fn tool_test_line_mode_all_suites_continue_after_expected_failures_and_clean_up() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, dir.path(), None);
    cli.input("/test-tools\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Local tool suite /test-tools:"), "{stderr}");
    for tool in ["read", "write", "edit", "grep", "ask"] {
        assert!(
            stderr.contains(&format!("[expected fixture failure] {tool}:")),
            "missing expected failure for {tool}: {stderr}"
        );
    }
    for marker in [
        "[read] ok",
        "[write] ok",
        "[edit] ok",
        "[grep] ok",
        "[glob] ok",
        "[bash] exit 7",
        "[bash_task] task bg-",
        "[wait] bash bg-",
        "[todo] ok",
        "pipeline-alpha",
        "background-start",
        "wait-short-settled",
    ] {
        assert!(stderr.contains(marker), "missing {marker}: {stderr}");
    }
    assert!(
        stderr.contains("Local tool suite complete:") && stderr.contains("tool calls."),
        "{stderr}"
    );
    assert!(fixture_directories(dir.path()).is_empty());
    assert!(session_jsonl(dir.path()).is_empty());
    no_more(&server);
}

#[test]
fn tool_test_line_mode_each_command_runs_only_its_own_suite() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, dir.path(), None);
    for command in [
        "/test-read",
        "/test-write",
        "/test-edit",
        "/test-grep",
        "/test-glob",
        "/test-bash",
        "/test-bash-task",
        "/test-wait",
        "/test-todo",
        "/test-ask",
    ] {
        cli.input(&format!("{command}\n"));
    }
    cli.input("/quit\n");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr.matches("Local tool suite complete:").count(),
        10,
        "{stderr}"
    );
    for command in [
        "/test-read",
        "/test-write",
        "/test-edit",
        "/test-grep",
        "/test-glob",
        "/test-bash",
        "/test-bash-task",
        "/test-wait",
        "/test-todo",
        "/test-ask",
    ] {
        assert!(
            stderr.contains(&format!("Local tool suite {command}:")),
            "missing {command}: {stderr}"
        );
    }
    assert!(fixture_directories(dir.path()).is_empty());
    assert!(session_jsonl(dir.path()).is_empty());
    no_more(&server);
}

#[test]
fn tool_test_line_mode_rejects_arguments_without_running_fixtures() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, dir.path(), None);
    cli.input("/test-edit extra\n/test-ui extra\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("usage: /test-edit"), "{stderr}");
    assert!(stderr.contains("usage: /test-ui"), "{stderr}");
    assert!(!stderr.contains("Local tool suite"), "{stderr}");
    assert!(fixture_directories(dir.path()).is_empty());
    no_more(&server);
}

#[test]
fn ui_test_line_mode_streams_mock_without_provider_or_session() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&server, dir.path(), None);
    cli.input("/test-ui\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.matches("UI-TEST-ASSISTANT-START").count(), 1);
    assert_eq!(stdout.matches("UI-TEST-CODE-MARKER").count(), 1);
    assert_eq!(stdout.matches("UI-TEST-ASSISTANT-END").count(), 1);
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("UI-TEST-THINKING-START"), "{stderr}");
    assert!(stderr.contains("~80 ms/token"), "{stderr}");
    assert!(stderr.contains("Mock UI stream complete"), "{stderr}");
    assert!(stderr.contains("no provider request"), "{stderr}");
    assert!(session_jsonl(dir.path()).is_empty());
    no_more(&server);
}
