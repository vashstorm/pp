// LSP 终端展示的真实 PTY 回归：迟到诊断卡、severity/unverified、Ctrl+O
// 展开、AI 原文提交 diff、恢复时冻结重放。模型 provider 与 LSP 都用隔离
// 夹具；测试只通过屏幕内容与磁盘结果推进，不以 sleep 猜测时序。

use super::*;
use std::io::Write;

fn lsp_tui_script() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lsp_tui_server.py")
}

fn lsp_tui_settings(home: &Path, enabled: bool) {
    let text = format!(
        "rate_limit_retry_budget_seconds: 0\nlsp:\n  enabled: {enabled}\n  lazy: false\n  formatOnWrite: false\n  diagnosticsOnWrite: true\n  diagnosticsOnEdit: false\n  requestTimeout: 5\n"
    );
    std::fs::write(config_path(home, "settings.yml"), text).unwrap();
}

/// 写入隔离 HOME 的模型/LSP 配置和项目根标记；返回 (FIFO 目录, 服务器日志)。
fn lsp_tui_fixture(
    home: &Path,
    cwd: &Path,
    provider: &Server,
    pull: &str,
    severity: u8,
) -> (std::path::PathBuf, std::path::PathBuf) {
    lsp_tui_settings(home, true);
    std::fs::write(config_path(home, "models.yml"), fixture_models(provider)).unwrap();
    let barriers = home.join("lsp-barriers");
    std::fs::create_dir_all(&barriers).unwrap();
    let log = home.join("lsp-server.log");
    std::fs::create_dir_all(cwd.join("src")).unwrap();
    std::fs::write(cwd.join("Cargo.toml"), "[package]\nname=\"fixture\"\n").unwrap();
    let config = json!({
        "servers": {
            "fixture": {
                "command": "python3",
                "args": [
                    lsp_tui_script().display().to_string(),
                    "--barrier-dir", barriers.display().to_string(),
                    "--log", log.display().to_string(),
                    "--pull", pull,
                    "--severity", severity.to_string()
                ],
                "fileTypes": [".rs"],
                "rootMarkers": ["Cargo.toml"],
                "languageId": "rust"
            }
        }
    });
    std::fs::write(config_path(home, "lsp.json"), config.to_string()).unwrap();
    (barriers, log)
}

fn lsp_tui_fifo(dir: &Path, name: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );
    path
}

fn lsp_tui_wait_log(path: &Path, needle: &str) {
    let deadline = Instant::now() + Duration::from_millis(5000);
    loop {
        if std::fs::read_to_string(path).is_ok_and(|text| text.contains(needle)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fixture log never contained {needle}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn lsp_tui_release(fifo: &Path) {
    let mut writer = std::fs::OpenOptions::new().write(true).open(fifo).unwrap();
    writer.write_all(b"x").unwrap();
    writer.flush().unwrap();
}

fn lsp_tui_write_task(pty: &mut Pty, provider: &Server, path: &str, content: &str) {
    pty.input("write the file\r");
    let (request, _) = pty.request(provider);
    calls_reply(
        request,
        json!([call(
            "lsp-w",
            "write",
            json!({"path": path, "content": content})
        )]),
    );
    let (request, _) = pty.request(provider);
    final_reply(request, "done");
    pty.completed();
}

fn lsp_tui_structured_rule(mut pty: Pty) {
    pty.input("\x03");
    pty.finish();
}

#[test]
fn lsp_tui_late_card_is_independent_and_does_not_request_model_again() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (barriers, log) = lsp_tui_fixture(home.path(), cwd.path(), &provider, "full", 1);
    let fifo = lsp_tui_fifo(&barriers, "tui-late");
    let mut pty = Pty::start_with_args(cwd.path(), home.path(), &[]);
    pty.ready();
    let before = pty.contents();
    lsp_tui_write_task(
        &mut pty,
        &provider,
        "src/a.rs",
        "fn main() {\n    wrong();\n}\n",
    );
    assert!(!before.contains("LSP diagnostics (late)"));
    lsp_tui_wait_log(&log, "tui-late");
    lsp_tui_release(&fifo);
    pty.wait_for("迟到诊断独立卡片", |screen| {
        let text = screen.contents();
        text.contains("LSP diagnostics (late)")
            && text.contains("[error]")
            && text.contains("fixture wrong line 2")
    });
    // 后台报告只投递展示，不产生新的模型请求。
    no_more(&provider);
    let text = pty.contents();
    assert!(!text.contains("\"ok\""), "{text}");
    lsp_tui_structured_rule(pty);
}

#[test]
fn lsp_tui_late_unverified_is_not_displayed_as_clean() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (barriers, log) = lsp_tui_fixture(home.path(), cwd.path(), &provider, "unchanged", 1);
    let fifo = lsp_tui_fifo(&barriers, "tui-late");
    let mut pty = Pty::start_with_args(cwd.path(), home.path(), &[]);
    pty.ready();
    lsp_tui_write_task(&mut pty, &provider, "src/a.rs", "fn main() {}\n");
    lsp_tui_wait_log(&log, "tui-late");
    lsp_tui_release(&fifo);
    pty.wait_for("未验证报告不显示 clean", |screen| {
        let text = screen.contents();
        text.contains("LSP diagnostics (late)")
            && text.contains("fixture · unverified")
            && !text.contains("fixture · clean")
    });
    no_more(&provider);
    lsp_tui_structured_rule(pty);
}

#[test]
fn lsp_tui_late_report_collapses_and_ctrl_o_expands() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (barriers, log) = lsp_tui_fixture(home.path(), cwd.path(), &provider, "full", 2);
    let fifo = lsp_tui_fifo(&barriers, "tui-late");
    let mut pty = Pty::start_with_args(cwd.path(), home.path(), &[]);
    pty.ready();
    let content = (1..=15)
        .map(|line| format!("wrong(); // {line}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    lsp_tui_write_task(&mut pty, &provider, "src/a.rs", &content);
    lsp_tui_wait_log(&log, "tui-late");
    lsp_tui_release(&fifo);
    pty.wait_for("折叠的迟到诊断卡", |screen| {
        let text = screen.contents();
        text.contains("LSP diagnostics (late)")
            && text.contains("[warning]")
            && text.contains("more diagnostic lines")
    });
    let collapsed = pty.contents();
    assert!(!collapsed.contains("fixture wrong line 15"), "{collapsed}");
    pty.function_key('o');
    pty.wait_for("Ctrl+O 展开完整报告", |screen| {
        screen.contents().contains("fixture wrong line 15")
    });
    no_more(&provider);
    lsp_tui_structured_rule(pty);
}

#[test]
fn lsp_tui_ai_content_diff_and_resume_replay_are_frozen() {
    let provider = server();
    let home = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let (barriers, log) = lsp_tui_fixture(home.path(), cwd.path(), &provider, "full", 1);
    let fifo = lsp_tui_fifo(&barriers, "tui-late");
    let mut pty = Pty::start_with_args(cwd.path(), home.path(), &[]);
    pty.ready();
    lsp_tui_write_task(
        &mut pty,
        &provider,
        "src/a.rs",
        "fn main() {    \n    wrong();   \n}\n",
    );
    // 最终磁盘内容保持 AI 提供的文本，卡片展示真实提交 diff。
    let saved = std::fs::read_to_string(cwd.path().join("src/a.rs")).unwrap();
    assert_eq!(saved, "fn main() {    \n    wrong();   \n}\n");
    let text = pty.contents();
    assert!(text.contains("fn main() {"), "{text}");
    lsp_tui_wait_log(&log, "tui-late");
    lsp_tui_release(&fifo);
    pty.wait_for("迟到报告", |screen| {
        screen.contents().contains("LSP diagnostics (late)")
    });
    let id = resume_boundary_session_ids(home.path()).pop().unwrap();
    let starts = std::fs::read_to_string(&log)
        .unwrap()
        .matches("\"event\": \"started\"")
        .count();
    lsp_tui_structured_rule(pty);

    // 禁用 LSP 后恢复：报告只从 journal 冻结重放，不重建旧服务器。
    lsp_tui_settings(home.path(), false);
    let mut resumed = Pty::start_with_args(cwd.path(), home.path(), &["-r", &id]);
    resumed.wait_for("恢复后仍显示冻结的迟到报告", |screen| {
        screen.contents().contains("LSP diagnostics (late)")
    });
    let after = std::fs::read_to_string(&log)
        .unwrap()
        .matches("\"event\": \"started\"")
        .count();
    assert_eq!(
        starts, after,
        "resume must not start another language server"
    );
    no_more(&provider);
    resumed.input("\x03");
    resumed.finish();
}
