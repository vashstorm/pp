use super::{Store, database_path};
use anyhow::{Context, Result, ensure};
use pp_core::context::TokenUsage;
use std::fs;
use std::path::Path;

/// Server-side token usage of the latest ordinary request of a root session plus the accumulated
/// session output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionUsage {
    pub last_request: Option<TokenUsage>,
    pub session_output_tokens: u64,
}

/// Restores the latest ordinary request usage of a root session and the accumulated session output
/// from the given stats database.
///
/// Only rows with `session_file = session`, `purpose = 'task'` and `status = 'ok'` are counted,
/// excluding `task_title`, `plan_review` and subagent requests. No empty database is created when the
/// file is missing.
pub fn session_usage(database: &Path, session: &Path) -> Result<SessionUsage> {
    match fs::symlink_metadata(database) {
        Ok(metadata) => ensure!(metadata.is_file(), "statistics database must be a file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error.into()),
    }
    Store::open(database)?.session_usage(session)
}

/// Sums the recorded cost of the requests already linked to the root session and its subagents.
///
/// A legacy record without a cost snapshot only sets
/// [`pp_config::task_status::HistoricalCost::incomplete`] true; nothing is recomputed from the current
/// config. No database is created when the legacy database is missing.
pub fn session_cost(session: &Path) -> Result<pp_config::task_status::HistoricalCost> {
    let path = database_path()?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(metadata.is_file(), "statistics database must be a file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error.into()),
    }
    Store::open(&path)?.session_cost(session)
}

/// Index range of a root session and its dedicated subagent directory.
///
/// `upper` is the lexicographic upper bound of `prefix` (last character plus one code point), so
/// `[prefix, upper)` covers exactly the files under `<root>.subagents/sessions/`, while sibling
/// directories such as `sessions-other/` stay out of the range.
pub(crate) struct SessionScope {
    pub root: String,
    pub prefix: String,
    pub upper: String,
}

impl SessionScope {
    pub(crate) fn for_root(session: &Path) -> Result<Self> {
        Self::new(
            session.to_string_lossy().into_owned(),
            format!(
                "{}/",
                session
                    .with_extension("subagents")
                    .join("sessions")
                    .display()
            ),
        )
    }

    pub(crate) fn new(root: String, prefix: String) -> Result<Self> {
        let last = prefix
            .chars()
            .next_back()
            .context("session prefix must not be empty")?;
        let next = char::from_u32(u32::from(last) + 1)
            .context("session prefix must end with a character that has a successor")?;
        let mut upper = prefix.clone();
        upper.pop();
        upper.push(next);
        Ok(Self {
            root,
            prefix,
            upper,
        })
    }
}

/// Two `UNION ALL` branches for the root session and the subagent directory; parameters are fixed as
/// `?1`=root, `?2`=prefix, `?3`=upper.
pub(crate) fn session_requests_sql(columns: &str) -> String {
    format!(
        "SELECT {columns} FROM requests WHERE session_file=?1 \
         UNION ALL \
         SELECT {columns} FROM requests WHERE session_file>=?2 AND session_file<?3"
    )
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::*;
    use super::*;
    use crate::Rates;
    use serde_json::{Value, json};

    #[test]
    fn session_cost_uses_root_and_child_records_and_recorded_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open_store(&dir.path().join("stats.db")).unwrap();
        fs::write(
            &store.models_path,
            models_with_cost(json!({"input":2,"cacheRead":0.2,"output":8})),
        )
        .unwrap();
        let rates = Rates::load(&store.models_path);
        let root = dir.path().join("root.jsonl");
        let child = dir.path().join("root.subagents/sessions/child.jsonl");
        let other = dir.path().join("root.subagents/sessions-other/child.jsonl");
        for (key, file) in [("root", &root), ("child", &child), ("other", &other)] {
            let mut record = fixture(key, 1000);
            record.session_file = Some(file.to_string_lossy().into_owned());
            snapshot_for(&rates, &mut record);
            assert!(record.cost.is_some());
            store.insert(&record, &Value::Null).unwrap();
        }
        // root and child each have 20 uncached input + 80 cache read + 20 output; sessions-other is not
        // counted.
        let history = store.session_cost(&root).unwrap();
        assert!(!history.incomplete);
        assert!((history.usd.unwrap() - 0.000432).abs() < 1e-12);

        // A legacy record without a cost snapshot only marks the coverage incomplete; the amount stays
        // the same.
        let mut legacy = fixture("legacy", 1001);
        legacy.session_file = Some(root.to_string_lossy().into_owned());
        store.insert(&legacy, &Value::Null).unwrap();
        let history = store.session_cost(&root).unwrap();
        assert!(history.incomplete);
        assert!((history.usd.unwrap() - 0.000432).abs() < 1e-12);

        // Rewriting models.yml does not recompute historical amounts.
        fs::write(
            &store.models_path,
            models_with_cost(json!({"input":0,"cacheRead":0,"output":0})),
        )
        .unwrap();
        let history = store.session_cost(&root).unwrap();
        assert!(history.incomplete);
        assert!((history.usd.unwrap() - 0.000432).abs() < 1e-12);

        assert_eq!(
            store
                .session_cost(&dir.path().join("missing.jsonl"))
                .unwrap(),
            Default::default()
        );
    }

    #[test]
    fn session_usage_restores_latest_ok_task_request_and_output_sum_without_creating_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let root = dir.path().join("root.jsonl");
        let child = dir.path().join("root.subagents/sessions/child.jsonl");

        assert_eq!(session_usage(&db, &root).unwrap(), Default::default());
        assert!(!db.exists());

        let mut store = open_store(&db).unwrap();
        let mut r1 = fixture("t1", 1000);
        r1.session_file = Some(root.to_string_lossy().into_owned());
        r1.input_tokens = Some(1000);
        r1.output_tokens = Some(120);
        r1.cached_tokens = Some(400);
        r1.cache_write_tokens = Some(100);
        store.insert(&r1, &Value::Null).unwrap();

        // Non-task, non-ok and subagent requests do not count toward the root session's ordinary request
        // usage.
        let mut title = fixture("title", 1001);
        title.session_file = Some(root.to_string_lossy().into_owned());
        title.purpose = "task_title".into();
        title.input_tokens = Some(500);
        title.output_tokens = Some(15);
        store.insert(&title, &Value::Null).unwrap();

        let mut failed = fixture("failed", 1002);
        failed.session_file = Some(root.to_string_lossy().into_owned());
        failed.status = "failed".into();
        failed.input_tokens = Some(9999);
        failed.output_tokens = Some(999);
        store.insert(&failed, &Value::Null).unwrap();

        let mut sub = fixture("sub", 1003);
        sub.session_file = Some(child.to_string_lossy().into_owned());
        sub.purpose = "subagent".into();
        sub.input_tokens = Some(300);
        sub.output_tokens = Some(80);
        store.insert(&sub, &Value::Null).unwrap();

        let mut r2 = fixture("t2", 1004);
        r2.session_file = Some(root.to_string_lossy().into_owned());
        r2.input_tokens = Some(1500);
        r2.output_tokens = Some(230);
        r2.cached_tokens = Some(900);
        r2.cache_write_tokens = None;
        store.insert(&r2, &Value::Null).unwrap();
        drop(store);

        let restored = session_usage(&db, &root).unwrap();
        assert_eq!(restored.session_output_tokens, 350);
        assert_eq!(
            restored.last_request,
            Some(TokenUsage {
                input_tokens: 1500,
                output_tokens: 230,
                cached_tokens: Some(900),
                cache_write_tokens: None,
                reasoning_tokens: None,
            })
        );

        let link = dir.path().join("link.db");
        std::os::unix::fs::symlink(&db, &link).unwrap();
        assert!(session_usage(&link, &root).is_err());
    }
}
