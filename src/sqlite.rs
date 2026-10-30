use anyhow::{Result, ensure};
use rusqlite::{Connection, OpenFlags};
use std::{
    fs::{self, DirBuilder, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::Duration,
};

/// 调用方负责将底层错误转换为不含正文或凭据的领域提示。
pub(crate) fn open(path: &Path) -> Result<Connection> {
    let dir = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| anyhow::anyhow!("Database directory unavailable."))?;
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let directory = fs::symlink_metadata(dir)?;
    ensure!(directory.is_dir(), "Database directory unavailable.");
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    check_file(path)?;
    check_sidecars(path)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.mode() & 0o077 == 0,
        "Database file unavailable."
    );
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    // macOS 的 /var 等系统祖先可以是链接；只规范目录，不解析最终数据库文件。
    let resolved = dir.canonicalize()?.join(
        path.file_name()
            .ok_or_else(|| anyhow::anyhow!("Database file unavailable."))?,
    );
    let conn = Connection::open_with_flags(&resolved, flags)?;
    let current = fs::symlink_metadata(path)?;
    ensure!(
        current.is_file()
            && current.dev() == metadata.dev()
            && current.ino() == metadata.ino()
            && current.mode() & 0o077 == 0,
        "Database file changed."
    );
    conn.busy_timeout(Duration::from_millis(250))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    check_sidecars(path)?;
    Ok(conn)
}

fn check_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_file() && metadata.mode() & 0o077 == 0,
            "Database file unavailable."
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn check_sidecars(path: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        check_file(Path::new(&name))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn sqlite_database_security() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("agent");
        let path = dir.join("agent.db");
        let conn = open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE example(value TEXT); INSERT INTO example VALUES ('safe');",
        )
        .unwrap();
        for path in [
            dir.clone(),
            path.clone(),
            dir.join("agent.db-wal"),
            dir.join("agent.db-shm"),
        ] {
            let mode = fs::metadata(&path).unwrap().mode() & 0o777;
            assert_eq!(mode, if path == dir { 0o700 } else { 0o600 });
        }
        drop(conn);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open(&path).is_err());
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = dir.join(format!("agent.db{suffix}"));
            symlink(&path, &sidecar).unwrap();
            assert!(open(&path).is_err());
            fs::remove_file(&sidecar).unwrap();
            fs::write(&sidecar, b"unsafe").unwrap();
            fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(open(&path).is_err());
            assert_eq!(fs::read(&sidecar).unwrap(), b"unsafe");
            fs::remove_file(&sidecar).unwrap();
            fs::create_dir(&sidecar).unwrap();
            assert!(open(&path).is_err());
            fs::remove_dir(&sidecar).unwrap();
        }
        let link = dir.join("linked.db");
        symlink(&path, &link).unwrap();
        assert!(open(&link).is_err());
        let not_file = dir.join("directory.db");
        fs::create_dir(&not_file).unwrap();
        assert!(open(&not_file).is_err());
        let conn = open(&path).unwrap();
        assert_eq!(
            conn.query_row("SELECT value FROM example", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "safe"
        );
        assert_eq!(
            conn.pragma_query_value(None, "synchronous", |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            conn.pragma_query_value(None, "foreign_keys", |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn sqlite_corrupt_databases_are_preserved_without_sensitive_errors() {
        let root = tempfile::tempdir().unwrap();
        for name in ["agent.db", "history.db"] {
            let path = root.path().join(name);
            let bytes = b"not-sqlite fixture-private-credential";
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let error = if name == "agent.db" {
                crate::agent_store::AgentStore::open(&path).err().unwrap()
            } else {
                crate::history::Store::open(&path).err().unwrap()
            };
            assert!(!format!("{error:#}").contains("fixture-private-credential"));
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }
}
