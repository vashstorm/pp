// Real-PTY regression for LSP terminal display: late diagnostic card, severity/unverified, Ctrl+O
// expansion, the AI's original text committed as a diff, and frozen replay on resume. Both the model
// provider and LSP use isolated fixtures; the test advances only on screen content and on-disk results,
// never guessing timing with sleep.

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

/// Write the model/LSP config and project root markers into an isolated HOME; returns (FIFO directory, server log).
fn lsp_tui_fixture(
    home: &Path,
    cwd: &Path,
    provider: &TestServer,
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

fn lsp_tui_write_task(pty: &mut Pty, provider: &TestServer, path: &str, content: &str) {
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
    pty.wait_for("a separate card for the late diagnostic", |screen| {
        let text = screen.contents();
        text.contains("LSP diagnostics (late)")
            && text.contains("[error]")
            && text.contains("fixture wrong line 2")
    });
    // A background report is display-only and creates no new model request.
    no_more(&provider);
    let text = pty.contents();
    assert!(!text.contains("\"ok\""), "{text}");
    lsp_tui_structured_rule(pty);
}
