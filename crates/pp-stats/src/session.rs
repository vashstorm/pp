use super::{Store, database_path};
use anyhow::{Context, Result, ensure};
use std::fs;
use std::path::Path;

/// 汇总已关联到根会话及其子 Agent 的请求所记录的费用。
///
/// 缺费用快照的旧记录只把 [`pp_config::task_status::HistoricalCost::incomplete`] 置真，
/// 不按当前配置回算。旧库不存在时不建库。
pub fn session_cost(session: &Path) -> Result<pp_config::task_status::HistoricalCost> {
    let path = database_path()?;
    match fs::symlink_metadata(&path) {
        Ok(metadata) => ensure!(metadata.is_file(), "statistics database must be a file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(error) => return Err(error.into()),
    }
    Store::open(&path)?.session_cost(session)
}

/// 根会话与其专属子 Agent 目录的索引范围。
///
/// `upper` 是 `prefix` 的字典序上界（取末字符加一个码点），因此 `[prefix, upper)` 恰好覆盖
/// `<root>.subagents/sessions/` 下的文件，而 `sessions-other/` 等相邻目录不在区间内。
pub(crate) struct SessionScope {
    pub root: String,
    pub prefix: String,
    pub upper: String,
}

impl SessionScope {
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

/// 根会话与子 Agent 目录的两段 `UNION ALL`；参数固定为 `?1`=root、`?2`=prefix、`?3`=upper。
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
        // root 与 child 各 20 未缓存输入 + 80 缓存读 + 20 输出；sessions-other 不计入。
        let history = store.session_cost(&root).unwrap();
        assert!(!history.incomplete);
        assert!((history.usd.unwrap() - 0.000432).abs() < 1e-12);

        // 没有费用快照的旧记录只标记覆盖不完整，金额不变。
        let mut legacy = fixture("legacy", 1001);
        legacy.session_file = Some(root.to_string_lossy().into_owned());
        store.insert(&legacy, &Value::Null).unwrap();
        let history = store.session_cost(&root).unwrap();
        assert!(history.incomplete);
        assert!((history.usd.unwrap() - 0.000432).abs() < 1e-12);

        // 改写 models.yml 不重算历史金额。
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
}
