//! 分卷日志文件与命名空间。
//!
//! 构造只校验 `~/.pp/logs` 目录并打开跨进程 `.lock`，不读目录内容；当日最大
//! 分卷号的扫描推迟到首次真正追加，仍然在 `.lock` 临界区内完成，因此长时间
//! 使用后目录条目线性增长也不会拖慢启动。

use std::{
    fs::{self, DirBuilder, File},
    io::{self, Write},
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    sync::Arc,
};

use chrono::{Local, NaiveDate};
use rustix::fs::{FlockOperation, flock};

use super::{MAX_LOG_BYTES, open_regular, open_regular_with_len};

// Share the open lock fd without duplicating it for every event. The guard
// does not borrow RotatingFile, allowing its date and volume cache to change.
pub(super) struct FileLock(Arc<File>);

impl FileLock {
    pub(super) fn acquire(file: &Arc<File>) -> io::Result<Self> {
        let file = Arc::clone(file);
        loop {
            match flock(file.as_ref(), FlockOperation::LockExclusive) {
                Ok(()) => return Ok(Self(file)),
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        while let Err(rustix::io::Errno::INTR) = flock(self.0.as_ref(), FlockOperation::Unlock) {}
    }
}

pub(super) struct RotatingFile {
    pub(super) dir: PathBuf,
    pub(super) lock: Arc<File>,
    current: Option<(NaiveDate, u64)>,
}

impl RotatingFile {
    /// 只建目录、校验目录类型并打开共享 `.lock`；分卷号的扫描推迟到首次追加。
    pub(super) fn new(dir: PathBuf) -> io::Result<Self> {
        DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let metadata = fs::symlink_metadata(&dir)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log directory is not a directory",
            ));
        }
        Ok(Self {
            lock: Arc::new(open_regular(&dir.join(".lock"), false, true)?),
            dir,
            current: None,
        })
    }

    pub(super) fn append_record(&mut self, record: &[u8]) -> io::Result<()> {
        let _lock = FileLock::acquire(&self.lock)?;
        self.append_record_locked(record, Local::now().date_naive())
    }

    pub(super) fn append_record_for(
        &mut self,
        namespace: Namespace,
        record: &[u8],
    ) -> io::Result<()> {
        let _lock = FileLock::acquire(&self.lock)?;
        self.append_record_locked_for(namespace, record, Local::now().date_naive())
    }

    pub(super) fn append_record_locked(
        &mut self,
        record: &[u8],
        today: NaiveDate,
    ) -> io::Result<()> {
        self.append_record_locked_for(Namespace::Pp, record, today)
    }

    // Caller holds .lock. The explicit date keeps boundary tests clock-independent.
    fn append_record_locked_for(
        &mut self,
        namespace: Namespace,
        record: &[u8],
        today: NaiveDate,
    ) -> io::Result<()> {
        if record.len() as u64 > MAX_LOG_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log event too large",
            ));
        }
        let mut number = match self.current {
            Some((date, number)) if date == today => {
                match fs::symlink_metadata(self.path_for(namespace, today, number)) {
                    Ok(_) => Some(number),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        self.scan(namespace, today)?
                    }
                    Err(error) => return Err(error),
                }
            }
            _ => self.scan(namespace, today)?,
        };
        if let Some(mut latest) = number {
            // Other instances create contiguous successors while holding .lock.
            while let Some(next) = latest.checked_add(1) {
                match fs::symlink_metadata(self.path_for(namespace, today, next)) {
                    Ok(_) => {
                        open_regular(&self.path_for(namespace, today, next), false, false)?;
                        latest = next;
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                    Err(error) => return Err(error),
                }
            }
            number = Some(latest);
        }
        if let Some(number) = number {
            let (mut file, length) =
                open_regular_with_len(&self.path_for(namespace, today, number), false, false)?;
            if length <= MAX_LOG_BYTES && record.len() as u64 <= MAX_LOG_BYTES - length {
                self.current = Some((today, number));
                return file.write_all(record);
            }
        }
        loop {
            let next = match number {
                Some(number) => number
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("log volume overflow"))?,
                None => 1,
            };
            match open_regular(&self.path_for(namespace, today, next), true, false) {
                Ok(mut file) => {
                    self.current = Some((today, next));
                    return file.write_all(record);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    // Validate conflicts rather than skipping unsafe paths.
                    open_regular(&self.path_for(namespace, today, next), false, false)?;
                    number = self.scan(namespace, today)?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(test)]
    pub(super) fn path(&self, date: NaiveDate, number: u64) -> PathBuf {
        self.path_for(Namespace::Pp, date, number)
    }

    pub(super) fn path_for(&self, namespace: Namespace, date: NaiveDate, number: u64) -> PathBuf {
        self.dir.join(namespace.file_name(date, number))
    }

    // Caller holds .lock, so the scanned volume cannot change under us.
    fn scan(&self, namespace: Namespace, today: NaiveDate) -> io::Result<Option<u64>> {
        let mut maximum = None;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if let Some((date, number)) = entry
                .file_name()
                .to_str()
                .and_then(|name| namespace.parse_file_name(name))
                && date == today
            {
                open_regular(&entry.path(), false, false)?;
                maximum = Some(maximum.map_or(number, |old: u64| old.max(number)));
            }
        }
        Ok(maximum)
    }
}

#[derive(Clone, Copy)]
pub(super) enum Namespace {
    Pp,
    Debug,
}

impl Namespace {
    // gc 等按名字扫描的路径遍历全部 namespace，新增变体自动纳入。
    pub(super) const ALL: [Namespace; 2] = [Self::Pp, Self::Debug];

    pub(super) fn file_name(self, date: NaiveDate, number: u64) -> String {
        match self {
            Self::Pp => format!("pp.{date}.{number}.log"),
            // Debug volumes are padded to four digits minimum, no ".log".
            Self::Debug => format!("debug.{date}.{number:04}"),
        }
    }

    pub(super) fn parse_file_name(self, name: &str) -> Option<(NaiveDate, u64)> {
        match self {
            Self::Pp => parse_name(name),
            Self::Debug => parse_debug_name(name),
        }
    }
}

fn parse_name(name: &str) -> Option<(NaiveDate, u64)> {
    let body = name.strip_prefix("pp.")?.strip_suffix(".log")?;
    let (date, number) = body.rsplit_once('.')?;
    if date.len() != 10 || number.starts_with('0') || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    if parsed.to_string() != date {
        return None;
    }
    Some((parsed, number.parse().ok()?))
}

pub(super) fn parse_prompt_name(name: &str) -> Option<NaiveDate> {
    let (time, random) = name.strip_prefix("prompt-debug.")?.rsplit_once('-')?;
    if time.len() != 19
        || random.len() != 32
        || !random
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let timestamp = chrono::NaiveDateTime::parse_from_str(time, "%Y-%m-%dT%H-%M-%S").ok()?;
    (timestamp.format("%Y-%m-%dT%H-%M-%S").to_string() == time).then_some(timestamp.date())
}

fn parse_debug_name(name: &str) -> Option<(NaiveDate, u64)> {
    let body = name.strip_prefix("debug.")?;
    let (date, digits) = body.rsplit_once('.')?;
    if date.len() != 10 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let parsed = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    if parsed.to_string() != date {
        return None;
    }
    let number: u64 = digits.parse().ok()?;
    // Only the exact {:04} spelling is a volume: unpadded or over-padded
    // numbers and volume zero are foreign files, never rotated into.
    (number > 0 && format!("{number:04}") == digits).then_some((parsed, number))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::{
        collections::BTreeSet,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
    };

    fn date() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 19).unwrap()
    }

    fn append(file: &mut RotatingFile, record: &[u8], day: NaiveDate) -> io::Result<()> {
        let _lock = FileLock::acquire(&file.lock)?;
        file.append_record_locked(record, day)
    }

    fn debug_append(file: &mut RotatingFile, record: &[u8], day: NaiveDate) -> io::Result<()> {
        let _lock = FileLock::acquire(&file.lock)?;
        file.append_record_locked_for(Namespace::Debug, record, day)
    }

    fn padded_record(size: usize) -> Vec<u8> {
        let mut record = vec![b' '; size];
        record[..2].copy_from_slice(b"{}");
        record[size - 1] = b'\n';
        record
    }

    fn records(dir: &Path) -> Vec<Value> {
        let mut values = Vec::new();
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name().to_str().and_then(parse_name).is_none() {
                continue;
            }
            let bytes = fs::read(entry.path()).unwrap();
            assert!(bytes.len() as u64 <= MAX_LOG_BYTES);
            assert!(bytes.ends_with(b"\n"));
            for line in bytes.strip_suffix(b"\n").unwrap().split(|b| *b == b'\n') {
                values.push(serde_json::from_slice(line).unwrap());
            }
        }
        values
    }

    #[test]
    fn rotating_volumes_deferred_scan_and_date_isolation() {
        {
            let dir = tempfile::tempdir().unwrap();
            let mut file = RotatingFile::new(dir.path().join("logs")).unwrap();
            fs::write(file.path(date(), 2), b"{}\n").unwrap();
            fs::write(file.path(date(), 10), b"{\"old\":true}\n").unwrap();
            append(&mut file, b"{\"new\":true}\n", date()).unwrap();
            let prefix = fs::read(file.path(date(), 10)).unwrap();
            let path = file.dir.clone();
            drop(file);
            let mut file = RotatingFile::new(path).unwrap();
            append(&mut file, b"{}\n", date()).unwrap();
            assert!(
                fs::read(file.path(date(), 10))
                    .unwrap()
                    .starts_with(&prefix)
            );
            assert!(!file.path(date(), 11).exists());
            fs::write(file.path(date(), 10), padded_record(MAX_LOG_BYTES as usize)).unwrap();
            append(&mut file, b"{}\n", date()).unwrap();
            assert_eq!(fs::read(file.path(date(), 11)).unwrap(), b"{}\n");
            fs::remove_file(file.path(date(), 11)).unwrap();
            append(&mut file, b"{\"recovered\":true}\n", date()).unwrap();
            assert_eq!(
                fs::read(file.path(date(), 11)).unwrap(),
                b"{\"recovered\":true}\n"
            );
            assert_eq!(records(&file.dir).len(), 3);
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let logs = dir.path().join("logs");
            fs::create_dir(&logs).unwrap();
            let today = Local::now().date_naive();
            // 日志不自动删除，长期使用后目录里已有大量历史分卷。
            let old = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap();
            for number in 1..=500 {
                fs::write(logs.join(format!("pp.{old}.{number}.log")), b"{}\n").unwrap();
            }
            // 构造阶段不打开当日分卷：不安全路径只在首次追加的扫描里被发现。
            let sentinel = dir.path().join("sentinel");
            fs::write(&sentinel, b"untouched").unwrap();
            let first = logs.join(format!("pp.{today}.1.log"));
            symlink(&sentinel, &first).unwrap();
            let mut file = RotatingFile::new(logs.clone()).unwrap();
            assert!(file.append_record(b"{\"lost\":true}\n").is_err());
            assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");

            // 首次成功追加才扫描，并落在当日分卷号：历史分卷与当日无关。
            fs::remove_file(&first).unwrap();
            file.append_record(b"{\"first\":true}\n").unwrap();
            assert_eq!(fs::read(&first).unwrap(), b"{\"first\":true}\n");
            file.append_record(b"{\"second\":true}\n").unwrap();
            assert_eq!(
                fs::read(&first).unwrap(),
                b"{\"first\":true}\n{\"second\":true}\n"
            );
            assert_eq!(
                fs::read(logs.join(format!("pp.{old}.1.log"))).unwrap(),
                b"{}\n"
            );
            assert_eq!(fs::read_dir(&logs).unwrap().count(), 1 + 500 + 1);
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let logs = dir.path().join("logs");
            fs::create_dir(&logs).unwrap();
            let names = ["pp.2000-01-01.1.log", "debug.2000-01-01.0001"];
            for name in names {
                fs::write(logs.join(name), b"old log\n").unwrap();
            }
            let mut pp = RotatingFile::new(logs.clone()).unwrap();
            let mut debug = RotatingFile::new(logs.clone()).unwrap();
            for day in [date(), date().succ_opt().unwrap()] {
                append(&mut pp, b"{}\n", day).unwrap();
                debug_append(&mut debug, b"{}\n", day).unwrap();
                for name in names {
                    assert_eq!(fs::read(logs.join(name)).unwrap(), b"old log\n");
                }
            }
            drop(pp);
            drop(debug);
            let _pp = RotatingFile::new(logs.clone()).unwrap();
            let _debug = RotatingFile::new(logs.clone()).unwrap();
            for name in names {
                assert_eq!(fs::read(logs.join(name)).unwrap(), b"old log\n");
            }
        }
        {
            for kind in ["symlink", "fifo", "directory"] {
                for lock_path in [true, false] {
                    let dir = tempfile::tempdir().unwrap();
                    let logs = dir.path().join("logs");
                    fs::create_dir(&logs).unwrap();
                    let sentinel = dir.path().join("sentinel");
                    fs::write(&sentinel, b"untouched").unwrap();
                    let path = logs.join(if lock_path {
                        ".lock".to_owned()
                    } else {
                        format!("pp.{}.1.log", Local::now().date_naive())
                    });
                    match kind {
                        "symlink" => symlink(&sentinel, &path).unwrap(),
                        "fifo" => assert!(
                            std::process::Command::new("mkfifo")
                                .arg(&path)
                                .status()
                                .unwrap()
                                .success()
                        ),
                        _ => fs::create_dir(&path).unwrap(),
                    }
                    let mut file = RotatingFile::new(logs.clone());
                    if lock_path {
                        // .lock 是构造阶段唯一打开的路径，不安全就立即失败。
                        assert!(file.is_err(), "{kind}");
                    } else {
                        // 不安全的当日分卷延迟到首次追加才校验。
                        file.as_mut()
                            .unwrap_or_else(|error| panic!("{kind} construction: {error}"))
                            .append_record(b"{}\n")
                            .expect_err(kind);
                    }
                    assert_eq!(fs::read(&sentinel).unwrap(), b"untouched");
                    if !lock_path {
                        let lock = open_regular(&logs.join(".lock"), false, false).unwrap();
                        flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
                        flock(&lock, FlockOperation::Unlock).unwrap();
                    }
                    if kind == "directory" {
                        fs::remove_dir(&path).unwrap();
                    } else {
                        fs::remove_file(&path).unwrap();
                    }
                    let mut file = RotatingFile::new(logs).unwrap();
                    file.append_record(b"{}\n").unwrap();
                }
            }
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target");
            fs::create_dir(&target).unwrap();
            symlink(&target, dir.path().join("logs")).unwrap();
            assert!(RotatingFile::new(dir.path().join("logs")).is_err());
            assert_eq!(fs::read_dir(target).unwrap().count(), 0);
        }
    }

    #[test]
    fn cached_writer_permissions_overflow_and_debug_namespaces() {
        {
            let dir = tempfile::tempdir().unwrap();
            let logs = dir.path().join("logs");
            let write =
                |file: &mut RotatingFile, namespace: Namespace, record: &[u8], day: NaiveDate| {
                    match namespace {
                        Namespace::Pp => append(file, record, day),
                        Namespace::Debug => debug_append(file, record, day),
                    }
                };
            for (namespace, label) in [(Namespace::Pp, "pp"), (Namespace::Debug, "debug")] {
                let mut first = RotatingFile::new(logs.clone()).unwrap();
                let mut second = RotatingFile::new(logs.clone()).unwrap();
                write(&mut first, namespace, b"{\"first\":1}\n", date()).unwrap();
                write(&mut second, namespace, b"{\"second\":1}\n", date()).unwrap();
                assert_eq!(
                    fs::read(first.path_for(namespace, date(), 1)).unwrap(),
                    b"{\"first\":1}\n{\"second\":1}\n",
                    "{label} namespace"
                );
                fs::write(
                    first.path_for(namespace, date(), 1),
                    padded_record(MAX_LOG_BYTES as usize),
                )
                .unwrap();
                write(&mut first, namespace, b"{\"first\":2}\n", date()).unwrap();
                write(&mut second, namespace, b"{\"second\":2}\n", date()).unwrap();
                assert_eq!(
                    fs::read(first.path_for(namespace, date(), 2)).unwrap(),
                    b"{\"first\":2}\n{\"second\":2}\n",
                    "{label} namespace"
                );
                let tomorrow = date().succ_opt().unwrap();
                fs::write(first.path_for(namespace, tomorrow, 10), b"{}\n").unwrap();
                write(&mut first, namespace, b"{\"next\":true}\n", tomorrow).unwrap();
                assert_eq!(
                    fs::read(first.path_for(namespace, tomorrow, 10)).unwrap(),
                    b"{}\n{\"next\":true}\n",
                    "{label} namespace"
                );
                assert!(!first.path_for(namespace, tomorrow, 2).exists());
            }
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let mut file = RotatingFile::new(dir.path().join("logs")).unwrap();
            assert_eq!(
                fs::metadata(&file.dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                file.lock.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
            fs::write(
                file.path(date(), 1),
                padded_record(MAX_LOG_BYTES as usize + 1),
            )
            .unwrap();
            append(&mut file, b"{}\n", date()).unwrap();
            assert_eq!(fs::read(file.path(date(), 2)).unwrap(), b"{}\n");
            assert_eq!(
                fs::metadata(file.path(date(), 2))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            fs::write(
                file.path(date(), u64::MAX),
                padded_record(MAX_LOG_BYTES as usize),
            )
            .unwrap();
            file.current = None;
            assert!(append(&mut file, b"{}\n", date()).is_err());
            assert_eq!(fs::read(file.path(date(), 2)).unwrap(), b"{}\n");
        }
        {
            let dir = tempfile::tempdir().unwrap();
            let logs = dir.path().join("logs");
            let mut file = RotatingFile::new(logs.clone()).unwrap();
            // Nothing is eagerly created beyond the shared .lock.
            let only: Vec<String> = fs::read_dir(&logs)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(only, [".lock".to_owned()]);
            // pp volumes and non-canonical debug names must not seed numbering.
            for name in [
                "pp.2026-10-19.1.log",
                "pp.2026-10-19.7.log",
                "debug.2026-10-19.1",
                "debug.2026-10-19.0000",
                "debug.2026-10-19.00001",
                "debug.2026-10-19.0001.log",
            ] {
                fs::write(logs.join(name), b"{}\n").unwrap();
            }
            debug_append(&mut file, b"{\"first\":true}\n", date()).unwrap();
            assert_eq!(
                fs::read(logs.join("debug.2026-10-19.0001")).unwrap(),
                b"{\"first\":true}\n"
            );
            fs::write(
                logs.join("debug.2026-10-19.0001"),
                padded_record(MAX_LOG_BYTES as usize),
            )
            .unwrap();
            debug_append(&mut file, b"{\"second\":true}\n", date()).unwrap();
            assert_eq!(
                fs::read(logs.join("debug.2026-10-19.0002")).unwrap(),
                b"{\"second\":true}\n"
            );
            // Every foreign or pp file is still present, byte for byte.
            let names: BTreeSet<String> = fs::read_dir(&logs)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(
                names,
                [
                    ".lock",
                    "debug.2026-10-19.1",
                    "debug.2026-10-19.0000",
                    "debug.2026-10-19.00001",
                    "debug.2026-10-19.0001",
                    "debug.2026-10-19.0001.log",
                    "debug.2026-10-19.0002",
                    "pp.2026-10-19.1.log",
                    "pp.2026-10-19.7.log",
                ]
                .into_iter()
                .map(String::from)
                .collect()
            );
        }
    }
}
