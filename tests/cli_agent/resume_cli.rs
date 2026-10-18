// Line-mode coverage for /resume: registration is visible in /help, the
// command is answered locally before any model or configuration is needed,
// and it never sends an HTTP request.

use super::*;

#[test]
fn resume_line_mode_hint_is_local_without_requests() {
    let a = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = Cli::start(&a, dir.path(), None);
    cli.input("/resume\n/help\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("/resume requires the TUI; use pp --resume SESSION_ID."),
        "{stderr}"
    );
    for command in ["/help", "/model", "/new", "/resume", "/quit"] {
        assert!(stderr.contains(command), "{stderr}");
    }
    no_more(&a);

    // Configuration unavailable: the same local answers still work and the
    // session still exits cleanly.
    let unavailable = tempfile::tempdir().unwrap();
    let mut cli = Cli::start_with(unavailable.path(), None, "{}", "{}");
    cli.input("/resume\n/help\n/quit\n");
    let output = cli.finish();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("/resume requires the TUI; use pp --resume SESSION_ID."),
        "{stderr}"
    );
    assert!(stderr.contains("Configuration unavailable"), "{stderr}");
}
