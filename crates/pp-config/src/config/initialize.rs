use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, DirBuilder, Permissions},
    io::{ErrorKind, Write},
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::Path,
};

const DEFAULT_MODELS: &str = "\
providers: {}\n\
\n\
# Use /login in the TUI, or replace the mapping above with your own provider.\n\
# providers:\n\
#   openai:\n\
#     api_key: your-api-key\n\
#     base_url: https://api.openai.com/v1\n\
#     protocol: response\n\
#     models:\n\
#       main: { model: your-model-id }\n";

pub fn initialize_defaults(dir: &Path) -> Result<()> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .context("Creating the user configuration directory")?;
    ensure!(
        fs::symlink_metadata(dir)?.is_dir(),
        "User configuration directory is unavailable"
    );
    for (name, contents) in [("settings.yml", "{}\n"), ("models.yml", DEFAULT_MODELS)] {
        create_missing(dir, name, contents).with_context(|| format!("Initializing {name}"))?;
    }
    Ok(())
}

fn create_missing(dir: &Path, name: &str, contents: &str) -> Result<()> {
    let path = dir.join(name);
    match fs::symlink_metadata(&path) {
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut temporary = tempfile::NamedTempFile::new_in(dir)?;
    temporary
        .as_file()
        .set_permissions(Permissions::from_mode(0o600))?;
    temporary.write_all(contents.as_bytes())?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error.error.into()),
    }
}
