//! Bash 有界输出、会话资源及运行中快照。
use std::sync::{Mutex, MutexGuard};

mod normalize;
mod notice;
mod sink;
mod store;
mod window;

pub use normalize::Normalizer;
pub use notice::{PipesStatus, notice, pipes_status};
pub(crate) use sink::OutputSink;
#[cfg(test)]
pub(crate) use store::Snapshot;
pub(crate) use store::Store;

fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(test)]
mod fixtures {
    use super::{OutputSink, Snapshot, Store};
    use pp_config::config::OutputSettings;
    use std::fs;

    pub(super) fn fixture(settings: OutputSettings) -> (tempfile::TempDir, Store, OutputSink) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::default();
        store.bind(dir.path(), "session").unwrap();
        let sink = OutputSink::new(settings, store.clone());
        (dir, store, sink)
    }

    pub(super) fn read(snapshot: Snapshot) -> String {
        fs::read_to_string(snapshot.path).unwrap()
    }
}
