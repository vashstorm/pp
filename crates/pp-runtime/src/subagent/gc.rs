use anyhow::Result;
use std::{fs, io, path::Path};

pub(crate) fn has_session_data(dir: &Path) -> Result<bool> {
    match fs::symlink_metadata(dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let mut empty = true;
    for entry in fs::read_dir(dir)? {
        empty = false;
        let entry = entry?;
        let kind = entry.file_type()?;
        let name = entry.file_name();
        if (name == "sessions" && kind.is_dir())
            || (kind.is_file() && name.to_str().is_some_and(is_session_record))
        {
            return Ok(true);
        }
    }
    Ok(empty)
}

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
    // The worktree, patches and their baseline may be the only copy of the isolated changes, so keep them for the user to recover.
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
