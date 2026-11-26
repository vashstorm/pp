use anyhow::Result;
use std::{fs, io, path::Path};

pub(crate) fn remove_session_data(dir: &Path) -> Result<()> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = entry.file_name();
        if name == "sessions" && kind.is_dir() {
            fs::remove_dir_all(entry.path())?;
        } else if kind.is_file() && name.to_str().is_some_and(is_session_record) {
            fs::remove_file(entry.path())?;
        }
    }
    // 工作树、补丁及其基线可能是隔离改动的唯一副本，保留给用户恢复。
    match fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn is_session_record(name: &str) -> bool {
    if name == "registry.json" {
        return true;
    }
    [
        ("a", ".events.jsonl"),
        ("a", ".session.json"),
        ("r", ".result.json"),
    ]
    .iter()
    .any(|(prefix, suffix)| {
        name.strip_prefix(prefix)
            .and_then(|name| name.strip_suffix(suffix))
            .is_some_and(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
    })
}
