//! Pipeline output and failure diagnostics through the real CLI and both provider protocols.

use super::*;
use rusqlite::Connection;
use std::path::Path;

fn session_journal(home: &Path) -> std::path::PathBuf {
    let sessions = config_path(home, "sessions");
    let mut files = std::fs::read_dir(&sessions)
        .unwrap_or_else(|error| panic!("missing sessions directory {sessions:?}: {error}"))
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect::<Vec<_>>();
    files.sort();
    assert_eq!(files.len(), 1, "expected one session journal: {files:?}");
    files.pop().unwrap()
}

fn assert_no_marker(path: &Path, marker: &str) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).unwrap().flatten() {
            assert_no_marker(&entry.path(), marker);
        }
    } else if metadata.is_file() {
        let bytes = std::fs::read(path).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains(marker),
            "transient pipe body reached {path:?}"
        );
    }
}

fn assert_stats_have_no_marker(home: &Path, marker: &str) {
    let path = home.join(".pp/stats/stats.db");
    assert!(path.is_file(), "stats database missing: {path:?}");
    let database = Connection::open(&path).unwrap();
    let mut statement = database
        .prepare("SELECT request_json, skills_json FROM request_contexts")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok([
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
            ])
        })
        .unwrap();
    for row in rows {
        for text in row.unwrap().into_iter().flatten() {
            assert!(
                !text.contains(marker),
                "transient pipe body reached the stats database"
            );
            assert!(
                !text.contains("tool_pipeline_snapshot"),
                "transient pipeline snapshot reached the stats database"
            );
        }
    }
}

#[test]
fn pipeline_foreground_keeps_failures_without_exposing_stage_output() {
    const MARKER: &str = "hidden-foreground-secret";
    for protocol in PROTOCOLS {
        let server = server();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("secret.txt"),
            format!("{MARKER}\nvisible-line\n"),
        )
        .unwrap();
        let cli = protocol.start(&server, dir.path(), Some("inspect pipeline"));
        let (request, _) = protocol.receive(&server);
        protocol.calls_reply(
            request,
            json!([protocol.call(
                "pipeline",
                "bash",
                json!({"command":"{ cat secret.txt; exit 7; } | tail -n 1"})
            )]),
        );
        let (request, body) = protocol.receive(&server);
        let result = protocol.tool_result(&body, "pipeline");
        assert_eq!(result["output"], "visible-line\n", "{result}");
        assert_eq!(result["ok"], true, "{result}");
        assert_eq!(result["exit_code"], 0, "{result}");
        assert_eq!(result["timed_out"], false, "{result}");
        assert_eq!(result["cancelled"], false, "{result}");
        assert!(
            !result.to_string().contains(MARKER),
            "stage body leaked into the bash result: {result}"
        );

        assert_eq!(
            result["pipeline_failures"],
            json!({"count":1,"stages":[{"pipeline":1,"stage":1,"status":7}]}),
            "{result}"
        );
        assert!(result.get("pipes").is_none(), "{result}");

        protocol.calls_reply(
            request,
            json!([protocol.call(
                "pipe-read",
                "read",
                json!({"path":"pipe://00112233445566778899aabbccddeeff/1"})
            )]),
        );
        let (request, body) = protocol.receive(&server);
        let read = protocol.tool_result(&body, "pipe-read");
        assert_eq!(read["ok"], false, "{read}");
        assert!(
            read["error"]
                .as_str()
                .unwrap()
                .contains("no longer available"),
            "{read}"
        );
        assert!(!body.to_string().contains(MARKER), "{body}");

        protocol.final_reply(request, "foreground pipeline verified");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("Non-zero pipeline stages: 1"),
            "missing bounded stage warning: {stderr}"
        );
        assert!(
            !stderr.contains(MARKER),
            "stage body reached the CLI output: {stderr}"
        );
        assert_no_marker(&session_journal(dir.path()), MARKER);
        assert_stats_have_no_marker(dir.path(), MARKER);
        no_more(&server);
    }
}
