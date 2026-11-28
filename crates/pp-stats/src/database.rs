//! Process-wide stats database handle: opens one connection, reuses it per request and caches the
//! pricing snapshot taken at collection time.

use super::{Rates, Store};
use anyhow::Result;
use std::{
    fmt, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

/// Stats database handle reused within the process; `Clone` only shares the same connection and
/// pricing snapshot.
///
/// [`Database::new`] performs no I/O: on the first write the handle opens `stats.db` and
/// idempotently brings the schema up to date, and every later request, tool execution and hook
/// record reuses that connection. A failed `Store::open` is not cached, so the next write retries;
/// cross-process concurrency is still governed by SQLite `busy_timeout`, and the `pp stats` server
/// keeps its own exclusive connection.
#[derive(Clone)]
pub struct Database {
    path: PathBuf,
    state: Arc<Mutex<State>>,
    /// Test observation: how many times this handle has established a connection (each
    /// `Store::open` runs `ensure_schema` exactly once); used only to assert "repeated writes do not
    /// reconnect".
    #[cfg(test)]
    opens: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Default)]
struct State {
    /// Open connection; `None` means it is not open yet or the last open failed (failures are not
    /// cached so they can be retried).
    store: Option<Store>,
    rates: Option<RatesCache>,
}

/// Cache key of the pricing snapshot taken at collection time: `(mtime, len)` of `models.yml`; `None`
/// means the file does not exist.
type Fingerprint = Option<(SystemTime, u64)>;

struct RatesCache {
    fingerprint: Fingerprint,
    rates: Rates,
}

impl Database {
    /// Zero-I/O construction: the path is only used to open the connection or read the pricing table
    /// on the first write.
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Arc::new(Mutex::new(State::default())),
            #[cfg(test)]
            opens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// Stats database path; read-only queries (`session_usage`/`session_cost`) and `pp gc` still take
    /// the path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn connect(&self) -> Result<Store> {
        let store = Store::open(&self.path)?;
        #[cfg(test)]
        self.opens
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(store)
    }

    /// Reuses the connection for one write; a failed open is not cached and the next call retries.
    pub(crate) fn with_store<T>(&self, write: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
        let mut state = self.lock();
        if state.store.is_none() {
            state.store = Some(self.connect()?);
        }
        write(state.store.as_mut().expect("statistics store opened"))
    }

    /// Runs the action only when the stats database is already open or the file exists on disk, and
    /// never creates an empty database for it.
    pub(crate) fn with_existing_store<T>(
        &self,
        action: impl FnOnce(&mut Store) -> Result<T>,
    ) -> Result<Option<T>> {
        let mut state = self.lock();
        if state.store.is_none() {
            match fs::symlink_metadata(&self.path) {
                Ok(metadata) => {
                    anyhow::ensure!(metadata.is_file(), "statistics database must be a file")
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
            }
            state.store = Some(self.connect()?);
        }
        action(state.store.as_mut().expect("statistics store opened")).map(Some)
    }

    /// Request-level write: both the connection and the pricing snapshot taken at collection time are
    /// reused, and the snapshot expires with the `models.yml` fingerprint.
    pub(crate) fn with_store_and_rates<T>(
        &self,
        write: impl FnOnce(&mut Store, &Rates) -> Result<T>,
    ) -> Result<T> {
        let mut state = self.lock();
        if state.store.is_none() {
            state.store = Some(self.connect()?);
        }
        let State { store, rates } = &mut *state;
        let store = store.as_mut().expect("statistics store opened");
        write(store, cached_rates(&store.models_path, rates))
    }

    #[cfg(test)]
    fn opens(&self) -> usize {
        self.opens.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl From<PathBuf> for Database {
    fn from(path: PathBuf) -> Self {
        Self::new(path)
    }
}

/// Handles compare by stats database path; connection and cache state do not take part in equality.
impl PartialEq for Database {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
    }
}

impl PartialEq<PathBuf> for Database {
    fn eq(&self, other: &PathBuf) -> bool {
        &self.path == other
    }
}

impl fmt::Debug for Database {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Database")
            .field("path", &self.path)
            .finish()
    }
}

/// Rebuilds the pricing snapshot only when the `(mtime, len)` of `models.yml` changes; `NotFound`
/// still falls back to the built-in price table through [`Rates::load`], and a parse failure still
/// means an unknown rate.
fn cached_rates<'a>(models_path: &Path, cache: &'a mut Option<RatesCache>) -> &'a Rates {
    let fingerprint = fingerprint(models_path);
    if cache
        .as_ref()
        .is_none_or(|cached| cached.fingerprint != fingerprint)
    {
        *cache = Some(RatesCache {
            fingerprint,
            rates: Rates::load(models_path),
        });
    }
    &cache.as_ref().expect("rates snapshot cached").rates
}

fn fingerprint(path: &Path) -> Fingerprint {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::*;
    use serde_json::{Value, json};

    /// Handle construction does no I/O; repeated writes establish a single connection, so
    /// `ensure_schema` runs only once.
    #[test]
    fn stats_database_connection_reuse_retry_and_rates_fingerprint() {
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("stats.db");
            let database = Database::new(path.clone());
            assert!(
                !path.exists(),
                "constructing the handle must not touch disk"
            );
            for key in ["one", "two"] {
                database
                    .with_store(|store| store.insert(&fixture(key, 1000), &Value::Null))
                    .unwrap();
            }
            // Clone shares the same connection state.
            database
                .clone()
                .with_store(|store| store.insert(&fixture("three", 1000), &Value::Null))
                .unwrap();
            assert_eq!(database.opens(), 1);
            let store = open_store(&path).unwrap();
            assert_eq!(store.latest(0, 2000, 500).unwrap().len(), 3);
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target.db");
            std::fs::write(&target, b"").unwrap();
            let path = dir.path().join("stats.db");
            std::os::unix::fs::symlink(&target, &path).unwrap();
            let database = Database::new(path.clone());
            assert!(database.with_store(|_| Ok(())).is_err());
            assert_eq!(database.opens(), 0);
            std::fs::remove_file(&path).unwrap();
            database
                .with_store(|store| store.insert(&fixture("retried", 1000), &Value::Null))
                .unwrap();
            database
                .with_store(|store| store.insert(&fixture("next", 1000), &Value::Null))
                .unwrap();
            assert_eq!(database.opens(), 1);
            let store = open_store(&path).unwrap();
            assert_eq!(store.latest(0, 2000, 500).unwrap().len(), 2);
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let models = dir.path().join("models.yml");
            let mut cache = None;
            let snapshot = |cache: &mut Option<RatesCache>| {
                cached_rates(&models, cache).snapshot(
                    "fixture/primary",
                    "fixture-model",
                    Some(100),
                    Some(20),
                    Some(80),
                    Some(0),
                )
            };
            // No models.yml: only the built-in price table remains, and the fixture model is not in it.
            assert_eq!(snapshot(&mut cache), (None, None));
            assert_eq!(
                cached_rates(&models, &mut cache)
                    .snapshot(
                        "openai/gpt-5.5",
                        "openai/gpt-5.5",
                        Some(1_000_000),
                        Some(0),
                        Some(0),
                        Some(0),
                    )
                    .1
                    .unwrap()
                    .input,
                5.0
            );
            std::fs::write(
                &models,
                models_with_cost(json!({"input":2,"cacheRead":0.2,"output":8})),
            )
            .unwrap();
            assert!(((snapshot(&mut cache).0.unwrap().total()) - 0.000216).abs() < 1e-10);
            // Reload after the file changes (different byte length, so the fingerprint necessarily
            // differs).
            std::fs::write(
                &models,
                models_with_cost(json!({"input":12.5,"cacheRead":0.25,"output":8.5})),
            )
            .unwrap();
            assert_eq!(snapshot(&mut cache).1.unwrap().input, 12.5);
            // Parse failure: unknown rate, not even the built-in price table is used.
            std::fs::write(&models, "invalid secret-marker").unwrap();
            assert_eq!(snapshot(&mut cache), (None, None));
            std::fs::write(
                &models,
                models_with_cost(json!({"input":3,"cacheRead":0.3,"output":9})),
            )
            .unwrap();
            assert_eq!(snapshot(&mut cache).1.unwrap().input, 3.0);
        }
    }
}
