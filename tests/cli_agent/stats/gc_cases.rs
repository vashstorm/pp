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
fn gc_removes_expired_sessions_and_owned_data_but_preserves_live_and_recent_sessions() {
    use pp_config::config::HookEvent;
    use pp_core::cancellation::CancellationToken;
    use pp_runtime::history::Store;
    use rustix::fs::{FlockOperation, flock};
    use std::{
        os::unix::fs::symlink,
        time::{Duration, SystemTime},
    };

    let home = tempfile::tempdir().unwrap();
    let agent = home.path().join(".pp/agent");
    let sessions = agent.join("sessions");
    let locks = home.path().join(".pp/run/sessions");
    let logs = home.path().join(".pp/logs");
    for dir in [&sessions, &locks, &logs] {
        fs::create_dir_all(dir).unwrap();
    }
    let day = chrono::Local::now().format("%Y-%m-%d");
    let expired = format!("{day}T12-00-00-{:032x}", 1);
    let live = format!("{day}T12-00-00-{:032x}", 2);
    let recent = format!("{day}T12-00-00-{:032x}", 3);
    let old = SystemTime::now() - Duration::from_secs(10 * 24 * 60 * 60);
    for (id, text) in [
        (&expired, "expiredneedle"),
        (&live, "liveneedle"),
        (&recent, "recentneedle"),
    ] {
        let path = sessions.join(format!("{id}.jsonl"));
        let header =
            json!({"type":"session","id":id,"timestamp":"2026-10-17T01:02:03Z","cwd":home.path()});
        let message = json!({"type":"message","message":{"role":"user","content":text}});
        write_session_text(&path, &format!("{header}\n{message}\n"));
        if id != &recent {
            fs::File::open(&path).unwrap().set_modified(old).unwrap();
        }
        let artifact = sessions.join(id);
        fs::create_dir(&artifact).unwrap();
        fs::write(artifact.join("1.bash.log"), text).unwrap();
        fs::write(logs.join(format!("prompt-debug.{id}")), text).unwrap();
    }
    let journal = sessions.join(format!("{expired}.jsonl"));
    let live_journal = sessions.join(format!("{live}.jsonl"));
    let expired_lock = locks.join(format!("{expired}.lock"));
    fs::File::create(&expired_lock).unwrap();
    let live_lock = fs::File::create(locks.join(format!("{live}.lock"))).unwrap();
    live_lock.set_modified(old).unwrap();
    flock(&live_lock, FlockOperation::NonBlockingLockExclusive).unwrap();
    let symlink_path = sessions.join(format!("{day}T12-00-00-{:032x}.jsonl", 4));
    symlink(&live_journal, &symlink_path).unwrap();

    let subagents = journal.with_extension("subagents");
    let child = subagents.join("sessions/child.jsonl");
    fs::create_dir_all(child.parent().unwrap()).unwrap();
    fs::write(&child, "child journal").unwrap();
    for name in [
        "registry.json",
        "a1.session.json",
        "a1.events.jsonl",
        "r1.result.json",
    ] {
        fs::write(subagents.join(name), "child state").unwrap();
    }
    let recovery = subagents.join("a1.worktree");
    fs::create_dir(&recovery).unwrap();
    fs::write(recovery.join("unsaved.txt"), "recover me").unwrap();
    fs::write(subagents.join("a1.patch"), "keep this patch").unwrap();
    let adjacent = subagents.join("sessions-other/adjacent.jsonl");

    let history_path = agent.join("history.db");
    let mut history = Store::open(&history_path).unwrap();
    history
        .sync_sessions(&sessions, &CancellationToken::new())
        .unwrap();
    history.set_session_pinned(&expired, true).unwrap();
    history
        .append_input_images(home.path(), "independent input", &[], 10)
        .unwrap();
    assert_eq!(
        history.search_sessions(None, "expiredneedle").unwrap()[0].id,
        expired
    );

    let now = chrono::Utc::now().timestamp_millis();
    let stats = pp_stats::Database::new(home.path().join(".pp/stats/stats.db"));
    for session_file in [&journal, &child, &live_journal, &adjacent] {
        pp_stats::hook(
            &stats,
            &pp_stats::HookRun {
                session_file,
                cwd: home.path(),
                event: HookEvent::Stop,
                tool: None,
                status: pp_stats::HookStatus::Ok,
                modified: false,
                timestamp: now,
                duration_ms: 1.0,
            },
        );
    }
    let db = database(home.path());
    for (id, file) in [
        (1, &journal),
        (2, &child),
        (3, &live_journal),
        (4, &adjacent),
    ] {
        db.execute(
            "INSERT INTO requests(id,request_key,timestamp,model,provider,model_reference,api,project,cwd,purpose,duration_ms,status,session_file)
             VALUES (?1,?1,?2,'m','p','p/m','completion','project','/project','task',1,'ok',?3)",
            rusqlite::params![id, now, file.to_str().unwrap()],
        ).unwrap();
        db.execute(
            "INSERT INTO request_contexts(request_id,request_json) VALUES (?1,'{}')",
            [id],
        )
        .unwrap();
        db.execute(
            "INSERT INTO request_tools(request_id,tool,args_chars) VALUES (?1,'read',12)",
            [id],
        )
        .unwrap();
        db.execute(
            "INSERT INTO tool_calls(id,request_key,session_file,call_id,tool,timestamp,status,duration_ms)
             VALUES (?1,?1,?2,'call','read',?3,'ok',1)",
            rusqlite::params![id, file.to_str().unwrap(), now],
        ).unwrap();
    }

    let output = Cli::spawn(home.path(), &["gc", "7"], &[]).finish();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        gc_counts(&output),
        [vec![3, 1, 2], vec![4, 2, 2], vec![2, 1, 1], vec![3, 1, 2]]
    );
    assert!(!journal.exists());
    assert!(!expired_lock.exists());
    assert!(!sessions.join(&expired).exists());
    assert!(!subagents.join("sessions").exists());
    for name in [
        "registry.json",
        "a1.session.json",
        "a1.events.jsonl",
        "r1.result.json",
    ] {
        assert!(!subagents.join(name).exists(), "{name}");
    }
    assert_eq!(
        fs::read_to_string(recovery.join("unsaved.txt")).unwrap(),
        "recover me"
    );
    assert_eq!(
        fs::read_to_string(subagents.join("a1.patch")).unwrap(),
        "keep this patch"
    );
    assert!(symlink_path.symlink_metadata().unwrap().is_symlink());
    for id in [&live, &recent] {
        assert!(sessions.join(format!("{id}.jsonl")).is_file());
        assert!(sessions.join(id).join("1.bash.log").is_file());
        assert!(logs.join(format!("prompt-debug.{id}")).is_file());
    }
    assert!(!logs.join(format!("prompt-debug.{expired}")).exists());
    assert!(
        history
            .search_sessions(None, "expiredneedle")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        history.search_sessions(None, "liveneedle").unwrap()[0].id,
        live
    );
    assert_eq!(
        history.load_inputs(home.path(), None, 10).unwrap()[0].text,
        "independent input"
    );
    let history_db = Connection::open(history_path).unwrap();
    let pending_pins: i64 = history_db
        .query_row("SELECT COUNT(*) FROM session_pins", [], |r| r.get(0))
        .unwrap();
    assert_eq!(pending_pins, 0);
    for (table, column) in [
        ("requests", "id"),
        ("request_contexts", "request_id"),
        ("request_tools", "request_id"),
        ("tool_calls", "id"),
        ("hook_runs", "id"),
    ] {
        let ids: Vec<i64> = db
            .prepare(&format!("SELECT {column} FROM {table} ORDER BY {column}"))
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(ids, [3, 4], "{table}");
    }
}
