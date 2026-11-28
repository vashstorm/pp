// `!command` line-mode regression: local execution touches neither the model context nor persistence,
// dual-pipe streaming output, empty command/failure/timeout/working-directory boundaries, and `-p` plus
// the unconfigured scenario never executing bang input.
// Reuses the parent module's loopback fixture; waits use only bounded polling, output and file barriers.

use super::streaming::{LiveCli, Pipe};
use super::*;

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
enum Protocol {
    Chat,
    Responses,
}

// Markers only the local shell produces: no model request or session record of either protocol may
// contain them.
const FORBIDDEN_MARKERS: [&str; 5] = [
    "out-first",
    "err-first",
    "out-last",
    "local-shell-cli",
    "[shell",
];

impl Protocol {
    fn models(self, server: &TestServer) -> String {
        match self {
            Self::Chat => fixture_models(server),
            Self::Responses => response_models(server, Some("response")),
        }
    }

    fn start(self, server: &TestServer, cwd: &std::path::Path) -> LiveCli {
        LiveCli::new(Cli::start_with(
            cwd,
            None,
            fixture_settings(),
            &self.models(server),
        ))
    }

    fn receive(self, server: &TestServer) -> (CapturedRequest, Value) {
        receive_at(
            server,
            match self {
                Self::Chat => "/v1/chat/completions",
                Self::Responses => "/v1/responses",
            },
        )
    }

    fn final_reply(self, request: CapturedRequest, text: &str) {
        match self {
            Self::Chat => final_reply(request, text),
            Self::Responses => response_final(request, text),
        }
    }

    // The first model task must carry only the typed prompt: no tool exchange and no shell command text
    // or output mixed into the native messages/input.
    fn assert_isolated_prompt(self, body: &Value, prompt: &str) {
        match self {
            Self::Chat => {
                let messages = body["messages"].as_array().unwrap();
                assert_eq!(messages.len(), 2, "chat history polluted: {body}");
                assert_eq!(messages[0]["role"], "system");
                assert_eq!(messages[1]["role"], "user");
                assert_eq!(messages[1]["content"], prompt);
            }
            Self::Responses => {
                let input = body["input"].as_array().unwrap();
                assert_eq!(input.len(), 1, "response input polluted: {body}");
                response_user(&input[0], prompt);
            }
        }
        let wire = body.to_string();
        for marker in FORBIDDEN_MARKERS {
            assert!(
                !wire.contains(marker),
                "native request leaked {marker}: {body}"
            );
        }
    }
}

fn sessions_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".pp/agent/sessions")
}

fn session_files(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(sessions_dir(home))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                .collect()
        })
        .unwrap_or_default()
}

fn assert_no_session(home: &std::path::Path) {
    let files = session_files(home);
    assert!(
        files.is_empty(),
        "shell-only interaction created a session: {files:?}"
    );
}

// Bounded polling until the model task writes expected into the session JSONL: file creation and flushed
// content are the observable barriers, not a fixed sleep.
fn wait_for_session_record(home: &std::path::Path, expected: &str) -> String {
    let deadline = Instant::now() + PROCESS_DEADLINE;
    loop {
        let files = session_files(home);
        assert!(
            files.len() <= 1,
            "the shell and the model task each created a session: {files:?}"
        );
        if files.len() == 1
            && let Ok(text) = std::fs::read_to_string(&files[0])
            && text.contains(expected)
        {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "the session never recorded {expected:?}: {:?}",
            session_files(home)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_no_shell_leak(text: &str, scope: &str) {
    for marker in FORBIDDEN_MARKERS {
        assert!(
            !text.contains(marker),
            "{scope} leaked local shell content {marker}: {text}"
        );
    }
}

// A shell-only interaction sends no Session notification and shows no context/usage lines.
fn assert_no_task_notifications(stderr: &str) {
    for line in ["Session: ", "Context: "] {
        assert!(
            !stderr.contains(line),
            "shell-only interaction produced a {line:?} line: {stderr}"
        );
    }
}

// The command is still stuck before the release barrier: both streams are already visible, while the
// controlled exit status and the gated output have not appeared and stdout stays empty.
fn assert_shell_gated(cli: &LiveCli) {
    let stderr = cli.text(Pipe::Stderr);
    assert!(
        !stderr.contains("out-last"),
        "out-last was printed before release was created: {stderr}"
    );
    for status in [
        "shell: exit",
        "shell: failed",
        "shell: cancelled",
        "shell: timed out",
    ] {
        assert!(
            !stderr.contains(status),
            "the shell finished early: {stderr}"
        );
    }
    assert!(
        cli.text(Pipe::Stdout).is_empty(),
        "shell content was written to stdout"
    );
    assert_no_task_notifications(&stderr);
}

#[test]
fn bang_shell_cli_is_local_and_streams() {
    let protocol = Protocol::Responses;
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let mut cli = protocol.start(&server, dir.path());
    assert_no_session(dir.path());
    cli.input("!printf out-first; printf err-first >&2; while [[ ! -e release ]]; do sleep 0.01; done; printf out-last\n");
    // The main output is a bounded preview merging stdout/stderr: within one batch the relative order of
    // the two streams' bytes depends on the reader race, so only timely appearance of both is required.
    cli.wait_for(Pipe::Stderr, "out-first");
    cli.wait_for(Pipe::Stderr, "err-first");
    // The barrier is still held: give "finished early" a bounded observation window.
    thread::sleep(Duration::from_millis(200));
    assert_shell_gated(&cli);
    assert!(
        server.try_recv().unwrap().is_none(),
        "a model request was sent while the shell was running"
    );
    assert_no_session(dir.path());

    std::fs::write(dir.path().join("release"), b"").unwrap();
    cli.wait_for(Pipe::Stderr, "[shell output] out-last");
    cli.wait_for(Pipe::Stderr, "shell: exit 0");
    let finished = cli.text(Pipe::Stderr);
    assert_no_task_notifications(&finished);
    assert!(
        server.try_recv().unwrap().is_none(),
        "shell completion triggered a model request"
    );
    assert_no_session(dir.path());
    assert!(
        cli.text(Pipe::Stdout).is_empty(),
        "stdout is not empty during the shell-only interaction"
    );

    cli.input("ordinary task\n");
    let (request, body) = protocol.receive(&server);
    protocol.assert_isolated_prompt(&body, "ordinary task");
    protocol.final_reply(request, "ordinary-complete");
    cli.wait_for(Pipe::Stdout, "ordinary-complete");
    let recorded = wait_for_session_record(dir.path(), "ordinary task");
    assert_no_shell_leak(&recorded, "session records");

    cli.input("/quit\n");
    let output = cli.finish();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{protocol:?}: {stderr}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "ordinary-complete\n",
        "{protocol:?}"
    );
    for (marker, count) in [
        ("out-first", 1),
        ("err-first", 1),
        ("out-last", 1),
        ("shell: running", 1),
        ("shell: exit 0", 1),
    ] {
        assert_eq!(
            stderr.matches(marker).count(),
            count,
            "{protocol:?}: {stderr}"
        );
    }
    let files = session_files(dir.path());
    assert_eq!(files.len(), 1, "{protocol:?}: {files:?}");
    let saved = std::fs::read_to_string(&files[0]).unwrap();
    assert!(saved.contains("ordinary task"), "{protocol:?}: {saved}");
    assert!(saved.contains("ordinary-complete"), "{protocol:?}: {saved}");
    assert_no_shell_leak(&saved, "session records");
    no_more(&server);
}
