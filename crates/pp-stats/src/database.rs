//! 进程级统计库句柄：一次打开连接，按请求复用，并缓存采集时的计价快照。

use super::{Rates, Store};
use anyhow::Result;
use std::{
    fmt, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::SystemTime,
};

/// 进程内复用的统计库句柄；`Clone` 只共享同一份连接与计价快照。
///
/// [`Database::new`] 不做任何 I/O：句柄在首次写入时打开 `stats.db` 并幂等补齐
/// schema，之后每次请求、工具执行与 hook 记录都复用同一连接。`Store::open`
/// 失败不会被缓存，下一次写入重新尝试；跨进程并发仍由 SQLite `busy_timeout`
/// 约束，`pp stats` 服务端继续独占自己的连接。
#[derive(Clone)]
pub struct Database {
    path: PathBuf,
    state: Arc<Mutex<State>>,
    /// 测试观测：本句柄成功建立连接的次数（每次 `Store::open` 恰好执行一次
    /// `ensure_schema`），只用于断言“重复写入不重复建连”。
    #[cfg(test)]
    opens: Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Default)]
struct State {
    /// 已打开的连接；`None` 表示尚未打开或上次打开失败（失败不缓存以便重试）。
    store: Option<Store>,
    rates: Option<RatesCache>,
}

/// 采集时计价快照的缓存键：`models.yml` 的 `(mtime, len)`；`None` 表示文件不存在。
type Fingerprint = Option<(SystemTime, u64)>;

struct RatesCache {
    fingerprint: Fingerprint,
    rates: Rates,
}

impl Database {
    /// 零 I/O 构造：路径只用于首次写入时打开连接或读取计价表。
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            state: Arc::new(Mutex::new(State::default())),
            #[cfg(test)]
            opens: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    /// 统计库路径；只读查询（`session_usage`／`session_cost`）与 `pp gc` 仍按路径使用。
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

    /// 复用连接执行一次写入；打开失败不缓存，下一次调用重新尝试。
    pub(crate) fn with_store<T>(&self, write: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
        let mut state = self.lock();
        if state.store.is_none() {
            state.store = Some(self.connect()?);
        }
        write(state.store.as_mut().expect("statistics store opened"))
    }

    /// 请求级写入：连接与采集时的计价快照都复用，快照随 `models.yml` 指纹失效。
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

/// 句柄按统计库路径比较；连接与缓存状态不参与相等判断。
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

/// 只在 `models.yml` 的 `(mtime, len)` 变化时重建计价快照；
/// `NotFound` 仍由 [`Rates::load`] 落到内置价目表，解析失败仍表示未知费率。
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

    /// 句柄构造不做 I/O；重复写入只建立一条连接，`ensure_schema` 随之只执行一次。
    #[test]
    fn stats_database_reuses_one_connection_across_writes() {
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
        // Clone 共享同一份连接状态。
        database
            .clone()
            .with_store(|store| store.insert(&fixture("three", 1000), &Value::Null))
            .unwrap();
        assert_eq!(database.opens(), 1);
        let store = open_store(&path).unwrap();
        assert_eq!(store.latest(0, 2000, 500).unwrap().len(), 3);
    }

    /// `Store::open` 失败（这里是 symlink 拒绝）不缓存，下一次写入必须重试成功。
    #[test]
    fn stats_database_retries_after_a_failed_open() {
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

    /// 计价快照按 `models.yml` 的 `(mtime, len)` 缓存：缺失用内置表，变化即失效，
    /// 解析失败仍表示未知费率。
    #[test]
    fn stats_rates_follow_the_models_fingerprint() {
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
        // 没有 models.yml：只保留内置价格表，fixture 模型不在其中。
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
        // 文件变化（字节数不同，因而指纹必然不同）后重新加载。
        std::fs::write(
            &models,
            models_with_cost(json!({"input":12.5,"cacheRead":0.25,"output":8.5})),
        )
        .unwrap();
        assert_eq!(snapshot(&mut cache).1.unwrap().input, 12.5);
        // 解析失败：未知费率，连内置价格表也不用。
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
