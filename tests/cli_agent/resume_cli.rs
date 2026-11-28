// Line-mode coverage for /resume: registration is visible in /help, the
// command is answered locally before any model or configuration is needed,
// and it never sends an HTTP request.

use super::*;

// `pp -r` without a SESSION_ID: a non-interactive terminal cannot open the session picker, so it must be refused
// explicitly instead of silently falling back to the most recent or an empty session.
#[test]
fn resume_flag_without_id_requires_interactive_terminal() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["-r"],
        vec!["-r", "-p", "resume-flag task"],
        vec!["--resume"],
    ] {
        let output = Cli::spawn(dir.path(), &args, &[]).finish();
        assert!(!output.status.success(), "args {args:?}");
        assert!(output.stdout.is_empty(), "args {args:?}");
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains("`-r` without a session ID opens the session picker"),
            "args {args:?}\n{stderr}"
        );
    }
}
