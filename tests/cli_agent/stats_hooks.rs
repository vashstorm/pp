use super::*;
use crate::stats::{http, start_stats_server};
use rusqlite::Connection;

fn hook_script(dir: &std::path::Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/bash\n{body}\n")).unwrap();
    let mut permissions = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(&path, permissions).unwrap();
    path.display().to_string()
}

#[test]
fn stats_hooks_are_recorded_served_by_api_and_cleaned_by_gc() {
    let server = server();
    let dir = tempfile::tempdir().unwrap();
    let rewrite = hook_script(
        dir.path(),
        "rewrite.sh",
        "cat > /dev/null\nprintf '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"updatedInput\":{\"path\":\"rewritten.txt\",\"content\":\"ok\"}}}'",
    );
    let post_fail = hook_script(
        dir.path(),
        "post-fail.sh",
        "cat > /dev/null\necho hook-secret-marker >&2\nexit 1",
    );
    write_settings(
        dir.path(),
        json!({"rate_limit_retry_budget_seconds": 0}).to_string(),
    )
    .unwrap();
    write_hooks(
        dir.path(),
        json!({
            "PreToolUse": [{"matcher": "write", "hooks": [
                {"type": "command", "command": rewrite}
            ]}],
            "PostToolUse": [{"hooks": [
                {"type": "command", "command": post_fail, "onFailure": "continue"}
            ]}],
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        config_path(dir.path(), "models.yml"),
        fixture_models(&server),
    )
    .unwrap();

    let cli = Cli::spawn(
        dir.path(),
        &["--allowed-tools", "Write", "-p", "write one file"],
        &[],
    );
    let (request, _) = receive_at(&server, "/v1/chat/completions");
    calls_reply(
        request,
        json!([call(
            "write-1",
            "write",
            json!({"path": "target.txt", "content": "orig"})
        )]),
    );
    let (request, _) = receive_at(&server, "/v1/chat/completions");
    final_reply(request, "done");
    let output = cli.finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let (_stats, address) = start_stats_server(dir.path());
    let (status, body) = http(&address, "/api/stats/hooks?range=24h", &address);
    assert_eq!(status, 200);
    let hooks: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(hooks["summary"]["runs"], 2);
    assert_eq!(hooks["summary"]["ok"], 1);
    assert_eq!(hooks["summary"]["blocked"], 0);
    assert_eq!(hooks["summary"]["errors"], 1);
    assert_eq!(hooks["summary"]["modified"], 1);
    assert_eq!(hooks["summary"]["sessions"], 1);
    assert_eq!(hooks["summary"]["projects"], 1);
    let events = hooks["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    for event_name in ["PreToolUse", "PostToolUse"] {
        let entry = events
            .iter()
            .find(|e| e["event"] == event_name)
            .unwrap_or_else(|| panic!("missing {event_name}: {hooks}"));
        assert_eq!(entry["runs"], 1);
        assert_eq!(entry["tools"][0]["tool"], "write");
        assert_eq!(entry["tools"][0]["runs"], 1);
    }
    assert!(!body.contains("hook-secret-marker"));
    assert!(!body.contains("post-fail.sh"));

    // 写入一条超过 90 天的旧 hook 记录，验证 `pp gc` 会清理它并保留近期记录。
    let db = Connection::open(dir.path().join(".pp/stats/stats.db")).unwrap();
    let old_ts = chrono::Utc::now().timestamp_millis() - 91 * 24 * 60 * 60 * 1000;
    db.execute(
        "INSERT INTO hook_runs(timestamp,session_file,project,cwd,event,tool,status,modified,duration_ms)
         VALUES (?1,'/old.jsonl','old','/old','Stop',NULL,'ok',0,1.0)",
        [old_ts],
    )
    .unwrap();
    drop(db);

    let gc_out = Cli::spawn(dir.path(), &["gc"], &[]).finish();
    assert!(gc_out.status.success());
    let db = Connection::open(dir.path().join(".pp/stats/stats.db")).unwrap();
    let remaining: i64 = db
        .query_row("SELECT COUNT(*) FROM hook_runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(remaining, 2);
}
