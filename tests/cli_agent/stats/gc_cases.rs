use super::*;
use std::fs;

fn gc_counts(output: &Output) -> Vec<Vec<u64>> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            line.split(|character: char| !character.is_ascii_digit())
                .filter(|field| !field.is_empty())
                .map(|field| field.parse().unwrap())
                .collect()
        })
        .collect()
}

#[test]
fn gc_removes_expired_logs_and_sqlite_snapshots_without_models() {
    use chrono::{Days, Local, Utc};
    use std::{fs, os::unix::fs::symlink};

    let home = tempfile::tempdir().unwrap();
    let (_server, _address) = start_stats_server(home.path());
    let db = database(home.path());
    let cutoff = Utc::now().timestamp_millis() - 90 * 24 * 60 * 60 * 1000;
    for (id, timestamp) in [
        (1, cutoff - 60_000),
        (2, cutoff + 60_000),
        (3, cutoff + 91 * 24 * 60 * 60 * 1000),
    ] {
        db.execute("INSERT INTO requests(id,request_key,timestamp,model,provider,model_reference,api,project,cwd,purpose,duration_ms,status) VALUES (?1,?1,?2,'m','p','p/m','completion','project','/project','task',1,'ok')", rusqlite::params![id, timestamp]).unwrap();
        // 过期行携带 1 MiB 首请求快照与一条工具调用，使 VACUUM 回收可观测。
        db.execute(
            "INSERT INTO request_contexts(request_id,request_json,skills_json) VALUES (?1,?2,'[]')",
            rusqlite::params![
                id,
                if id == 1 {
                    format!("\"{}\"", "x".repeat(1024 * 1024))
                } else {
                    "{}".to_owned()
                }
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO request_tools(request_id,tool,args_chars) VALUES (?1,'read',12)",
            [id],
        )
        .unwrap();
    }
    let pages_before: i64 = db
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    let logs = home.path().join(".pp/logs");
    let day = Local::now()
        .date_naive()
        .checked_sub_days(Days::new(90))
        .unwrap();
    let old = day.pred_opt().unwrap();
    let old_names = [
        format!("pp.{old}.1.log"),
        format!("debug.{old}.0001"),
        format!("prompt-debug.{old}T12-00-00-{}", "ab".repeat(16)),
    ];
    let kept_names = [
        format!("pp.{day}.1.log"),
        format!("debug.{day}.0001"),
        format!("prompt-debug.{day}T12-00-00-{}", "ab".repeat(16)),
        format!("pp.{old}.01.log"),
        "unrelated.log".to_owned(),
    ];
    for name in old_names.iter().chain(&kept_names) {
        fs::write(logs.join(name), b"fixture").unwrap();
    }
    let sentinel = home.path().join("sentinel");
    fs::write(&sentinel, b"keep").unwrap();
    let link = logs.join(format!("pp.{old}.2.log"));
    symlink(&sentinel, &link).unwrap();
    let directory = logs.join(format!("pp.{old}.3.log"));
    fs::create_dir(&directory).unwrap();
    let sessions = home.path().join(".pp/agent/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let session = sessions.join(format!("{old}T12-00-00-{}.jsonl", "ab".repeat(16)));
    fs::write(&session, b"saved session").unwrap();

    let output = Cli::spawn(home.path(), &["gc"], &[]).finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Three expired and three boundary logs, plus the server's current log.
    // Foreign names, symlinks, directories, and snapshots are not extra inventory.
    assert_eq!(
        gc_counts(&output),
        [vec![7, 3, 4], vec![3, 1, 2], vec![0, 0, 0]]
    );
    for name in old_names {
        assert!(!logs.join(name).exists());
    }
    for name in kept_names {
        assert_eq!(fs::read(logs.join(name)).unwrap(), b"fixture");
    }
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(directory.is_dir());
    assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
    assert_eq!(fs::read(&session).unwrap(), b"saved session");
    let remaining: Vec<i64> = db
        .prepare("SELECT id FROM requests ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(remaining, [2, 3]);
    // 首请求快照与工具调用跟随请求删除，不会变成孤儿行。
    for table in ["request_contexts", "request_tools"] {
        let children: Vec<i64> = db
            .prepare(&format!(
                "SELECT request_id FROM {table} ORDER BY request_id"
            ))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(children, [2, 3], "{table} must follow its request");
    }
    let pages_after: i64 = db
        .pragma_query_value(None, "page_count", |r| r.get(0))
        .unwrap();
    assert!(
        pages_after < pages_before,
        "VACUUM must reclaim expired payload pages"
    );
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let second = Cli::spawn(home.path(), &["gc"], &[]).finish();
    assert!(second.status.success());
    assert_eq!(
        gc_counts(&second),
        [vec![4, 0, 4], vec![2, 0, 2], vec![0, 0, 0]]
    );
}

#[test]
fn gc_empty_home_does_not_create_storage_and_rejects_symlink_database() {
    use std::{fs, os::unix::fs::symlink};
    let home = tempfile::tempdir().unwrap();
    let output = Cli::spawn(home.path(), &["gc"], &[]).finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        gc_counts(&output),
        [vec![0, 0, 0], vec![0, 0, 0], vec![0, 0, 0]]
    );
    assert!(!home.path().join(".pp").exists());
    let stats = home.path().join(".pp/stats");
    fs::create_dir_all(&stats).unwrap();
    let sentinel = home.path().join("sentinel");
    fs::write(&sentinel, b"keep").unwrap();
    symlink(&sentinel, stats.join("stats.db")).unwrap();
    let output = Cli::spawn(home.path(), &["gc"], &[]).finish();
    assert!(!output.status.success());
    assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
}

#[test]
fn gc_migrates_legacy_payloads_before_deleting_requests() {
    use chrono::Utc;

    let home = tempfile::tempdir().unwrap();
    let (_server, _address) = start_stats_server(home.path());
    let db = database(home.path());
    let cutoff = Utc::now().timestamp_millis() - 90 * 24 * 60 * 60 * 1000;
    // 旧版本二进制仍在写这张表；升级后第一次 `pp gc` 必须先把快照与工具调用迁走。
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS request_payloads (
            request_id INTEGER PRIMARY KEY REFERENCES requests(id), output_json TEXT NOT NULL,
            session_entry_json TEXT, request_json TEXT, skills_json TEXT
        );",
    )
    .unwrap();
    for (id, timestamp, context) in [
        (1, cutoff - 60_000, None),
        (2, cutoff + 60_000, Some("{\"model\":\"retained\"}")),
    ] {
        db.execute("INSERT INTO requests(id,request_key,timestamp,model,provider,model_reference,api,project,cwd,purpose,duration_ms,status) VALUES (?1,?1,?2,'m','p','p/m','completion','project','/project','task',1,'ok')", rusqlite::params![id, timestamp]).unwrap();
        db.execute(
            "INSERT INTO request_payloads(request_id,output_json,session_entry_json,request_json,skills_json) VALUES (?1,'{\"native\":[{\"type\":\"function_call\",\"name\":\"read\",\"arguments\":\"abcd\"}]}','[]',?2,'[]')",
            rusqlite::params![id, context],
        )
        .unwrap();
    }
    let output = Cli::spawn(home.path(), &["gc"], &[]).finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // 迁移先把旧正文与工具调用回填到新表，再删旧表；删除请求因此不受外键阻挡。
    let legacy: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name='request_payloads'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(legacy, 0);
    let requests: Vec<i64> = db
        .prepare("SELECT id FROM requests ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(requests, [2]);
    let legacy_tools: Vec<(i64, String, i64)> = db
        .prepare("SELECT request_id,tool,args_chars FROM request_tools ORDER BY request_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(legacy_tools, [(2, "read".to_owned(), 4)]);
    let retained: Vec<(i64, String)> = db
        .prepare("SELECT request_id,request_json FROM request_contexts ORDER BY request_id")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(retained, [(2, "{\"model\":\"retained\"}".to_owned())]);
    assert_eq!(
        db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn gc_days_argument_overrides_default_cutoff() {
    use chrono::{Days, Local, Utc};

    let home = tempfile::tempdir().unwrap();
    let (_server, _address) = start_stats_server(home.path());
    let db = database(home.path());
    let timestamp = Utc::now().timestamp_millis() - 30 * 24 * 60 * 60 * 1000;
    db.execute(
        "INSERT INTO requests(id,request_key,timestamp,model,provider,model_reference,api,project,cwd,purpose,duration_ms,status) VALUES (?1,?1,?2,'m','p','p/m','completion','project','/project','task',1,'ok')",
        rusqlite::params![1, timestamp],
    )
    .unwrap();
    db.execute(
        "INSERT INTO request_contexts(request_id,request_json,skills_json) VALUES (?1,'{}','[]')",
        [1],
    )
    .unwrap();
    db.execute(
        "INSERT INTO request_tools(request_id,tool,args_chars) VALUES (?1,'read',5)",
        [1],
    )
    .unwrap();
    let logs = home.path().join(".pp/logs");
    let day = Local::now()
        .date_naive()
        .checked_sub_days(Days::new(30))
        .unwrap();
    let old_log = logs.join(format!("pp.{day}.1.log"));
    fs::write(&old_log, b"fixture").unwrap();
    let request_ids = || -> Vec<i64> {
        db.prepare("SELECT id FROM requests ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    };

    // 非法天数（0、非数字、多参数、超过上限）在参数解析阶段失败，不删除任何数据。
    for arguments in [
        &["gc", "0"][..],
        &["gc", "abc"][..],
        &["gc", "7", "8"][..],
        &["gc", "4294967295"][..],
    ] {
        let output = Cli::spawn(home.path(), arguments, &[]).finish();
        assert!(!output.status.success());
        assert!(old_log.is_file());
        assert_eq!(request_ids(), [1]);
    }

    // 默认 90 天 cutoff 保留 30 天前的数据；总数含 stats server 的当日日志。
    let output = Cli::spawn(home.path(), &["gc"], &[]).finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        gc_counts(&output),
        [vec![2, 0, 2], vec![1, 0, 1], vec![0, 0, 0]]
    );
    assert!(old_log.is_file());
    assert_eq!(request_ids(), [1]);

    // 显式更短的天数同时清理日志和请求。
    let output = Cli::spawn(home.path(), &["gc", "7"], &[]).finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        gc_counts(&output),
        [vec![2, 1, 1], vec![1, 1, 0], vec![0, 0, 0]]
    );
    assert!(!old_log.exists());
    assert!(request_ids().is_empty());

    let output = Cli::spawn(home.path(), &["gc", "1"], &[]).finish();
    assert!(output.status.success());
    assert_eq!(
        gc_counts(&output),
        [vec![1, 0, 1], vec![0, 0, 0], vec![0, 0, 0]]
    );
}
