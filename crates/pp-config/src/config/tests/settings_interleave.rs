//! Deterministic interleaving regression for cross-process `settings.yml` saves.
//!
//! The read-then-write window of `update_settings_fields_returning` once relied only on a pre-commit comparison against concurrency:
//! two writers could both pass the comparison on the same stale text, and the later persist overwrote the earlier writer's fields entirely.
//! After the fix every save path acquires the sibling `settings.lock` before the first read and holds it across parsing, updating,
//! temporary file writes, comparison and persist; the comparison degrades to a second line of defense against unexpected external edits.
//!
//! The interleaving needs no sleep: the writer thread stops at this module's handshake control point before persist, and the main
//! thread probes with a non-blocking flock whether `settings.lock` is held, which distinguishes the two implementations:
//! under the new implementation a writer stopped at the control point always holds the lock, while the old one has no lock file to stop on.

use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use anyhow::Result;

use crate::config::{
    EditMode, Settings, read_settings, update_settings, update_settings_fields_returning,
};

/// Handshake wait bound: far above the 30 second lock acquisition wait, so it only fires if the implementation regresses.
const HANDSHAKE: Duration = Duration::from_secs(60);

/// Handshake registered in the writer thread's thread-local: notify the main thread before persist and wait for release.
struct Handshake {
    ready: Sender<()>,
    go: Receiver<()>,
}

thread_local! {
    static HANDSHAKE_SLOT: RefCell<Option<Handshake>> = const { RefCell::new(None) };
}

/// Test control point of `update_settings_fields_returning`: a writer thread with a registered handshake
/// stops before persist and waits for the main thread to release it; unregistered threads (production paths and the main thread) pass through.
pub(crate) fn pre_commit() {
    HANDSHAKE_SLOT.with(|slot| {
        if let Some(handshake) = slot.borrow_mut().as_mut() {
            handshake.ready.send(()).ok();
            // Stay inside the critical section until the main thread releases; continue immediately when the channel closes so teardown never hangs.
            handshake.go.recv().ok();
        }
    });
}

/// Registers the handshake in a separate thread and performs one settings save.
struct Staged {
    handle: std::thread::JoinHandle<Result<()>>,
    ready: Receiver<()>,
    go: Sender<()>,
}

fn staged_update(
    path: &Path,
    update: impl FnOnce(Settings) -> Result<Vec<(&'static str, serde_yaml::Value)>> + Send + 'static,
) -> Staged {
    let (ready_tx, ready_rx) = channel();
    let (go_tx, go_rx) = channel();
    let path = path.to_path_buf();
    let handle = std::thread::spawn(move || {
        HANDSHAKE_SLOT.with(|slot| {
            *slot.borrow_mut() = Some(Handshake {
                ready: ready_tx,
                go: go_rx,
            })
        });
        let result = update_settings_fields_returning(&path, update).map(|_| ());
        HANDSHAKE_SLOT.with(|slot| *slot.borrow_mut() = None);
        result
    });
    Staged {
        handle,
        ready: ready_rx,
        go: go_tx,
    }
}

/// Probes with a non-blocking flock whether some writer holds the sibling `settings.lock`.
/// A missing lock file (old implementation or nothing saved yet) counts as unheld.
fn lock_is_held(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new().read(true).write(true).open(
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join("settings.lock"),
    ) else {
        return false;
    };
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            while let Err(rustix::io::Errno::INTR) =
                rustix::fs::flock(&file, rustix::fs::FlockOperation::Unlock)
            {}
            false
        }
        Err(rustix::io::Errno::WOULDBLOCK) => true,
        Err(_) => false,
    }
}

#[test]
fn settings_lock_serializes_concurrent_field_updates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.yml");
    fs::write(&path, "# preserved comment\nedit: hashline\n").unwrap();

    // Writer A stops before persist: the old implementation still stops without any lock; the new one holds settings.lock here.
    let first = staged_update(&path, |_| {
        Ok(vec![("debug", serde_yaml::Value::Bool(true))])
    });
    first
        .ready
        .recv_timeout(HANDSHAKE)
        .expect("writer A reaches the pre-commit control point");

    // Writer B starts concurrently with A: the old implementation skips the read and comparison and stops at the control point directly;
    // the new one first waits on settings.lock and does not read the file until A releases it.
    let second = staged_update(&path, |_| {
        Ok(vec![(
            "auto_compact_threshold_percent",
            serde_yaml::Value::Number(42.into()),
        )])
    });

    if lock_is_held(&path) {
        // New implementation: only after A commits while holding the lock and releases it does B read the latest text containing debug.
        first
            .go
            .send(())
            .expect("writer A is still waiting for release");
        second
            .ready
            .recv_timeout(HANDSHAKE)
            .expect("writer B reaches the control point after the lock is released");
        second
            .go
            .send(())
            .expect("writer B is still waiting for release");
    } else {
        // Old implementation: B already passed the comparison on the same stale text, so whichever persist runs later
        // overwrites the earlier writer's fields.
        second
            .ready
            .recv_timeout(HANDSHAKE)
            .expect("writer B reaches the pre-commit control point");
        first
            .go
            .send(())
            .expect("writer A is still waiting for release");
        second
            .go
            .send(())
            .expect("writer B is still waiting for release");
    }
    first.handle.join().unwrap().unwrap();
    second.handle.join().unwrap().unwrap();

    // Whichever writer commits later, both top-level fields must survive and the comments and existing fields stay untouched.
    let settings = read_settings(&path).unwrap();
    assert!(settings.debug);
    assert_eq!(settings.auto_compact_threshold_percent, 42);
    assert_eq!(settings.edit, EditMode::Hashline);
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("# preserved comment"), "{text}");
}

#[test]
fn settings_lock_releases_after_write_and_failure_keeps_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("settings.yml");
    fs::write(&path, "# preserved comment\ndebug: false\n").unwrap();

    // The writer walks the whole critical section: the lock is released with the scope, so the next write need not wait.
    let writer = staged_update(&path, |_| {
        Ok(vec![("debug", serde_yaml::Value::Bool(true))])
    });
    writer
        .ready
        .recv_timeout(HANDSHAKE)
        .expect("writer reaches the pre-commit control point");
    writer
        .go
        .send(())
        .expect("writer is still waiting for release");
    writer.handle.join().unwrap().unwrap();

    // The lock is released: the next save on the same thread succeeds immediately and keeps the earlier fields and comments.
    update_settings(&path, |_| {
        Ok(("read_max_lines", serde_yaml::Value::Number(33.into())))
    })
    .unwrap();
    let settings = read_settings(&path).unwrap();
    assert!(settings.debug);
    assert_eq!(settings.read_max_lines, 33);

    // A strict validation failure commits nothing: the file stays as-is and the failing path releases the lock too.
    let before = fs::read_to_string(&path).unwrap();
    let error = update_settings(&path, |_| {
        Ok((
            "auto_compact_threshold_percent",
            serde_yaml::Value::Number(0.into()),
        ))
    })
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("auto_compact_threshold_percent must be between 1 and 99"),
        "{error:#}"
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
    assert_eq!(
        read_settings(&path).unwrap().auto_compact_threshold_percent,
        80
    );

    // The save after the failure can continue.
    update_settings(&path, |_| {
        Ok(("read_max_bytes", serde_yaml::Value::Number(17.into())))
    })
    .unwrap();
    let settings = read_settings(&path).unwrap();
    assert_eq!(settings.read_max_bytes, 17);
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("# preserved comment")
    );
}
