use super::*;
use rustix::fs::{FlockOperation, flock};
use std::time::SystemTime;

fn lock_path(home: &Path, id: &str) -> PathBuf {
    home.join(".pp/run/sessions").join(format!("{id}.lock"))
}

fn assert_held(path: &Path) {
    let file = fs::File::open(path).unwrap();
    assert_eq!(
        flock(&file, FlockOperation::NonBlockingLockExclusive).unwrap_err(),
        rustix::io::Errno::WOULDBLOCK
    );
}

#[test]
fn session_lock_survives_idle_and_gc_then_releases_after_process_death_both_apis() {
    for api in APIS {
        let server = server();
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let (id, journal) = make_saved_session(
            api,
            &server,
            home.path(),
            cwd.path(),
            "lock context",
            "lock answer",
        );
        let path = lock_path(home.path(), &id);
        assert!(path.is_file());
        let old = SystemTime::now() - Duration::from_secs(91 * 24 * 60 * 60);
        fs::File::open(&path).unwrap().set_modified(old).unwrap();
        let journal_before = fs::read(&journal).unwrap();
        let journal_mtime = fs::metadata(&journal).unwrap().modified().unwrap();
        let mut holder = start(home.path(), cwd.path(), &["-r", &id]);
        holder.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
        assert!(fs::metadata(&path).unwrap().modified().unwrap() > old);
        assert_eq!(
            fs::metadata(&journal).unwrap().modified().unwrap(),
            journal_mtime
        );
        assert_held(&path);
        expect_restore_failure(
            &server,
            home.path(),
            cwd.path(),
            &["-r", &id],
            &["session is already in use"],
            &["fixture-key"],
        );
        // Even when idle for a long time and the lock file is past its retention period, GC must skip the file that is really locked.
        fs::File::open(&path).unwrap().set_modified(old).unwrap();
        let output = command(home.path(), cwd.path(), &["gc"]).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("Session locks: total=1, removed=0, remaining=1")
        );
        assert_held(&path);
        assert_eq!(fs::read(&journal).unwrap(), journal_before);

        holder.cli.0.as_mut().unwrap().kill().unwrap();
        let output = holder.finish();
        assert!(!output.status.success());
        assert!(path.is_file());
        let mut resumed = start(home.path(), cwd.path(), &["-r", &id]);
        resumed.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
        assert_held(&path);
        resumed.input("/quit\n");
        assert!(resumed.finish().status.success());
        fs::File::open(&path).unwrap().set_modified(old).unwrap();
        let output = command(home.path(), cwd.path(), &["gc"]).output().unwrap();
        assert!(output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("Session locks: total=1, removed=1, remaining=0")
        );
        assert!(!path.exists());
        assert_eq!(fs::read(&journal).unwrap(), journal_before);
        // Once the lock file is cleaned up, the original session can be created and restored again.
        let mut resumed = start(home.path(), cwd.path(), &["-r", &id]);
        resumed.wait_for(Pipe::Err, &format!("Resumed session: {id}"));
        assert_held(&path);
        resumed.input("/quit\n");
        assert!(resumed.finish().status.success());
        no_more(&server);
    }
}
