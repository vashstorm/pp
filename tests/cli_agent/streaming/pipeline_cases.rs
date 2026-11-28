//! Plan §3 集成回归：真实 CLI 上验证前台/后台管道阶段镜像、pipe:// 只读正文、
//! 瞬时阶段事件不进入持久化，以及恢复会话只保留状态而不恢复正文。
//!
//! 用例只观察工具结果、进程间历史、会话/统计/artifact 文件与终端输出；
//! 不断言提示词、schema 文案或源码文本。

use super::*;
use rusqlite::Connection;
use std::path::Path;

fn assert_pipes(result: &Value) -> &Value {
    assert!(
        result["pipes"].is_object(),
        "missing pipes metadata: {result}"
    );
    &result["pipes"]
}

fn pipeline_run(result: &Value) -> String {
    assert_pipes(result)["run_id"]
        .as_str()
        .unwrap_or_else(|| panic!("missing pipe run id: {result}"))
        .to_owned()
}

fn pipeline_stream_uri(result: &Value, index: usize) -> String {
    assert_pipes(result)["streams"][index]["uri"]
        .as_str()
        .unwrap_or_else(|| panic!("missing stream {index}: {result}"))
        .to_owned()
}

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

fn collect_text(path: &Path, text: &mut String) {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_dir() {
        for entry in std::fs::read_dir(path).unwrap().flatten() {
            collect_text(&entry.path(), text);
        }
    } else if metadata.is_file() {
        text.push_str(&String::from_utf8_lossy(&std::fs::read(path).unwrap()));
    }
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
fn pipeline_foreground_stage_mirror_reads_into_model_history() {
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

        let pipes = assert_pipes(&result);
        assert_eq!(pipes["nonzero_stages"], 1, "{result}");
        assert_eq!(pipes["incomplete"], false, "{result}");
        let pipelines = pipes["pipelines"].as_array().unwrap();
        assert_eq!(pipelines.len(), 1, "{result}");
        assert_eq!(pipelines[0]["stage_count"], 2, "{result}");
        assert_eq!(pipelines[0]["stages"][0]["status"], 7, "{result}");
        assert_eq!(pipelines[0]["stages"][1]["status"], 0, "{result}");

        let uri = pipeline_stream_uri(&result, 0);
        assert_eq!(
            uri,
            format!("pipe://{}/1", pipeline_run(&result)),
            "{result}"
        );
        let total = format!("{MARKER}\nvisible-line\n").len() as u64;
        assert_eq!(pipes["streams"][0]["open"], false, "{result}");
        assert_eq!(pipes["streams"][0]["total_bytes"], total, "{result}");

        protocol.calls_reply(
            request,
            json!([protocol.call("pipe-read", "read", json!({"path":uri}))]),
        );
        let (request, body) = protocol.receive(&server);
        let read = protocol.tool_result(&body, "pipe-read");
        assert_eq!(read["content"], format!("{MARKER}\nvisible-line\n"));
        assert_eq!(read["pipe"]["total_bytes"], total);
        assert_eq!(read["pipe"]["start"], 0);
        assert_eq!(read["pipe"]["end"], total);
        assert_eq!(read["pipe"]["open"], false);
        assert_eq!(read["pipe"]["prefix_evicted"], false);
        // The read result is now part of the next model request's tool history,
        // while the bash result still carries only bounded metadata.
        assert!(
            body.to_string().contains(MARKER),
            "read content missing from model history: {body}"
        );
        assert_eq!(
            protocol.tool_result(&body, "pipeline")["pipes"]["run_id"],
            pipeline_run(&result)
        );

        protocol.final_reply(request, "foreground pipeline verified");
        let output = cli.finish();
        assert!(output.status.success(), "{protocol:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("[pipes: pipe://"),
            "missing bounded pipe summary: {stderr}"
        );
        assert!(
            stderr.contains("Non-zero stage results: 1"),
            "missing bounded stage warning: {stderr}"
        );
        assert!(
            !stderr.contains(MARKER),
            "stage body reached the CLI output: {stderr}"
        );
        no_more(&server);
    }
}

fn is_subagent_request(body: &Value) -> bool {
    body["instructions"]
        .as_str()
        .or_else(|| body["messages"][0]["content"].as_str())
        .is_some_and(|text| text.contains("You are subagent"))
}
