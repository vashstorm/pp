use super::schema::{Store, unavailable};
use anyhow::{Result, ensure};
use std::path::Path;

const BLANK_INPUT: &str = "History input text is blank.";

/// One accepted persistent input body; `id` is an increasing cursor for paged reads.
pub struct InputEntry {
    pub id: i64,
    pub text: String,
    pub images: Vec<crate::user_input::Image>,
}

impl Store {
    /// Records one accepted raw input and returns its cursor `id`; blank text is rejected and
    /// repeated text is kept as a separate event (no UNIQUE constraint, no dedup).
    #[cfg(test)]
    pub(crate) fn append_input(
        &mut self,
        cwd: &Path,
        text: &str,
        created_at_ms: i64,
    ) -> Result<i64> {
        self.append_input_images(cwd, text, &[], created_at_ms)
    }

    pub fn append_input_images(
        &mut self,
        cwd: &Path,
        text: &str,
        images: &[crate::user_input::Image],
        created_at_ms: i64,
    ) -> Result<i64> {
        ensure!(!text.trim().is_empty(), BLANK_INPUT);
        let cwd = cwd.to_string_lossy();
        self.conn
            .execute(
                "INSERT INTO input_history(cwd,text,created_at_ms,images) VALUES (?1,?2,?3,?4)",
                rusqlite::params![
                    cwd.as_ref(),
                    text,
                    created_at_ms,
                    serde_json::to_string(images).map_err(unavailable)?
                ],
            )
            .map_err(unavailable)?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Loads a page of inputs for `cwd`. `before_id` is the smallest `id` returned by the previous page (None means the first page);
    /// pages are always taken by id DESC and flipped within the page to return oldest-to-newest. The caller controls page size (currently 100).
    pub fn load_inputs(
        &self,
        cwd: &Path,
        before_id: Option<i64>,
        limit: usize,
    ) -> Result<Vec<InputEntry>> {
        let cwd = cwd.to_string_lossy();
        let mut statement = self
            .conn
            .prepare(
                "SELECT id,text,images FROM input_history
                 WHERE cwd=?1 AND (?2 IS NULL OR id<?2)
                 ORDER BY id DESC LIMIT ?3",
            )
            .map_err(unavailable)?;
        let rows = statement
            .query_map(
                rusqlite::params![cwd.as_ref(), before_id, limit as i64],
                |row| {
                    Ok(InputEntry {
                        id: row.get(0)?,
                        text: row.get(1)?,
                        images: serde_json::from_str(&row.get::<_, String>(2)?)
                            .map_err(|_| rusqlite::Error::InvalidQuery)?,
                    })
                },
            )
            .map_err(unavailable)?;
        let mut entries = Vec::with_capacity(limit);
        for entry in rows {
            entries.push(entry.map_err(unavailable)?);
        }
        entries.reverse();
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{count, cwd, temp_store};
    use super::*;
    use std::fs;

    #[test]
    fn sqlite_input_history_pagination_and_cwd_isolation() {
        let (root, mut store) = temp_store("history.db");
        let a = cwd(root.path(), "project-a");
        let b = cwd(root.path(), "project-b");

        for text in ["", "   ", "\n\t ", "  \u{3000}  "] {
            let error = store.append_input(&a, text, 0).unwrap_err();
            assert_eq!(error.to_string(), BLANK_INPUT);
        }

        let mut a_ids = Vec::new();
        for (index, text) in ["first slash", "second", "duplicate", "duplicate", "fifth"]
            .into_iter()
            .enumerate()
        {
            a_ids.push(store.append_input(&a, text, 1000 + index as i64).unwrap());
        }
        let mut b_ids = Vec::new();
        for index in 0..3 {
            b_ids.push(
                store
                    .append_input(&b, &format!("other {index}"), 2000 + index)
                    .unwrap(),
            );
        }

        // First page: take the latest `limit` rows by id DESC and flip within the page to oldest-to-newest; repeated text stays as separate events.
        let page = store.load_inputs(&a, None, 3).unwrap();
        assert_eq!(
            page.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            a_ids[2..5].to_vec()
        );
        assert_eq!(
            page.iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["duplicate", "duplicate", "fifth"]
        );

        // Full read is oldest-to-newest; the two cwds are fully isolated.
        let all = store.load_inputs(&a, None, 100).unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(
            all.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            a_ids.clone()
        );
        assert_eq!(
            all.iter()
                .map(|entry| entry.text.as_str())
                .collect::<Vec<_>>(),
            ["first slash", "second", "duplicate", "duplicate", "fifth"]
        );
        assert_eq!(
            store
                .load_inputs(&b, None, 100)
                .unwrap()
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            b_ids
        );
        let empty = cwd(root.path(), "project-c");
        assert!(store.load_inputs(&empty, None, 100).unwrap().is_empty());

        // Cursor paging: before_id is the previous page's smallest id, returning the page before it (oldest to newest).
        let older = store.load_inputs(&a, Some(a_ids[3]), 2).unwrap();
        assert_eq!(
            older.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            a_ids[1..3].to_vec()
        );
        let exhausted = store.load_inputs(&a, Some(a_ids[0]), 10).unwrap();
        assert!(exhausted.is_empty());
        assert!(store.load_inputs(&a, None, 0).unwrap().is_empty());

        // More than one page: 150 inputs are taken in two pages and restored in write order.
        let c = cwd(root.path(), "project-c-long");
        for index in 0..150 {
            store
                .append_input(&c, &format!("line {index}"), 3000 + index)
                .unwrap();
        }
        let first = store.load_inputs(&c, None, 100).unwrap();
        assert_eq!(first.len(), 100);
        assert_eq!(first[0].text, "line 50");
        assert_eq!(first[99].text, "line 149");
        let second = store.load_inputs(&c, Some(first[0].id), 100).unwrap();
        assert_eq!(second.len(), 50);
        assert_eq!(second[0].text, "line 0");
        assert_eq!(second[49].text, "line 49");
    }

    #[test]
    fn sqlite_concurrent_input_append() {
        let (root, mut store) = temp_store("history.db");
        let a = cwd(root.path(), "project-a");
        for index in 0..25 {
            store
                .append_input(&a, &format!("main {index}"), 1000 + index)
                .unwrap();
        }
        let path = root.path().join("history.db");
        let worker = std::thread::spawn(move || {
            let mut store = Store::open(&path).unwrap();
            let b = path.parent().unwrap().join("project-b");
            fs::create_dir_all(&b).unwrap();
            for index in 0..25 {
                store
                    .append_input(&b, &format!("side {index}"), 2000 + index)
                    .unwrap();
            }
        });
        worker.join().unwrap();
        assert_eq!(count(&store.conn, "SELECT COUNT(*) FROM input_history"), 50);
        assert_eq!(store.load_inputs(&a, None, 100).unwrap().len(), 25);
        assert_eq!(
            store
                .load_inputs(&root.path().join("project-b"), None, 100)
                .unwrap()
                .len(),
            25
        );
    }
}

#[cfg(test)]
mod image_input_tests {
    use super::super::schema::SCHEMA;
    use super::*;
    #[test]
    fn sqlite_image_history_migrates_v1_and_roundtrips_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        let conn = pp_core::sqlite::open(&path).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO input_history(cwd,text,created_at_ms) VALUES ('/project','old',1)",
            [],
        )
        .unwrap();
        drop(conn);
        let mut store = Store::open(&path).unwrap();
        let images = vec![crate::user_input::Image {
            marker: "[图片 1]".into(),
            summary: "fixture 2x3".into(),
            data_url: "data:image/png;base64,AA==".into(),
            source_range: None,
        }];
        store
            .append_input_images(Path::new("/project"), "[图片 1]", &images, 2)
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        let entries = store.load_inputs(Path::new("/project"), None, 100).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].text, "old");
        assert!(entries[0].images.is_empty());
        assert_eq!(entries[1].images, images);
        let version: i64 = store
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3);
    }
}
