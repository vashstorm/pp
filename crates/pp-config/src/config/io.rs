//! Reading, writing and lock-guarded atomic saves for `settings.yml` and `hooks.yml`.
use super::{HooksSettings, Settings, default_settings};
use crate::yaml;
use anyhow::{Context, Result, ensure};
use std::{fs, io::Write, path::Path};

pub(crate) fn update_settings(
    path: &Path,
    update: impl FnOnce(Settings) -> Result<(&'static str, serde_yaml::Value)>,
) -> Result<()> {
    update_settings_fields(path, |settings| Ok(vec![update(settings)?]))
}

pub(crate) fn update_settings_fields(
    path: &Path,
    update: impl FnOnce(Settings) -> Result<Vec<(&'static str, serde_yaml::Value)>>,
) -> Result<()> {
    update_settings_fields_returning(path, update).map(|_| ())
}

pub(crate) fn update_settings_fields_returning(
    path: &Path,
    update: impl FnOnce(Settings) -> Result<Vec<(&'static str, serde_yaml::Value)>>,
) -> Result<Settings> {
    // Acquire the sibling settings.lock before the first read and serialize parsing, updating,
    // temporary file writes, comparison and persist end to end: two cross-process save paths no longer overwrite each other based on stale text.
    // A failed lock acquisition saves nothing; settings saves have no external cancellation token, so lock waiting keeps the auth lock's 30 second bound.
    let _lock = crate::auth::StoreLock::acquire_settings(
        path,
        &pp_core::cancellation::CancellationToken::new(),
    )?;
    let read = || match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).context("Failed to read settings.yml"),
    };
    let original = read()?;
    let settings: Settings = yaml::parse(original.as_deref().unwrap_or("{}"), "settings")?;
    settings.validate()?;
    let mut updated = original.as_deref().unwrap_or("{}\n").to_owned();
    for (name, value) in update(settings)? {
        updated = yaml::update_field(&updated, name, value)?;
    }
    let settings: Settings = yaml::parse(&updated, "settings")?;
    settings.validate()?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent).context("Failed to create global config directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .context("Failed to create temporary config file")?;
    if let Ok(meta) = fs::metadata(path) {
        temporary.as_file().set_permissions(meta.permissions())?;
    }
    temporary
        .write_all(updated.as_bytes())
        .context("Failed to write configuration")?;
    temporary
        .as_file()
        .sync_all()
        .context("Failed to sync configuration")?;
    ensure!(
        read()? == original,
        "Configuration was modified by another process; please try again."
    );
    // Test-only control point: syncs once only when the writer thread has explicitly registered, for deterministic reproduction of interleaved concurrent saves.
    #[cfg(test)]
    crate::config::tests::settings_interleave::pre_commit();
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .context("Failed to commit configuration")?;
    Ok(settings)
}

pub fn read_settings(path: &Path) -> Result<Settings> {
    match fs::read_to_string(path) {
        // A missing settings.yml falls back to the defaults; other read errors such as a directory or invalid UTF-8 still block loading.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(default_settings()),
        result => {
            let text = result
                .with_context(|| format!("reading YAML settings config {}", path.display()))?;
            yaml::parse::<Settings>(&text, "settings")
                .and_then(|settings| {
                    settings.validate()?;
                    Ok(settings)
                })
                .with_context(|| format!("loading YAML settings config {}", path.display()))
        }
    }
}

/// Reads one hooks YAML file: a missing file, an empty file or a comment-only file all count as empty configuration;
/// other read errors, format errors and structural validation failures include the source path and never leak file contents.
pub fn read_hooks(path: &Path) -> Result<HooksSettings> {
    match fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HooksSettings::default()),
        result => {
            let text =
                result.with_context(|| format!("reading YAML hooks config {}", path.display()))?;
            // Empty files and comment-only files parse into a null document, which counts as unconfigured and does not require `{}`.
            if serde_yaml::from_str::<serde_yaml::Value>(&text).is_ok_and(|value| value.is_null()) {
                return Ok(HooksSettings::default());
            }
            yaml::parse::<HooksSettings>(&text, "hooks")
                .and_then(|hooks| {
                    hooks.validate()?;
                    Ok(hooks)
                })
                .with_context(|| format!("loading YAML hooks config {}", path.display()))
        }
    }
}

/// Merges the user-level `hooks.yml` with the project-level `.pp/hooks.yml`: project-level groups follow the user-level ones
/// and match in that order within an event; the two sources never override each other.
pub fn load_hooks(user: &Path, project: &Path) -> Result<HooksSettings> {
    let mut hooks = read_hooks(user)?;
    hooks.extend(&read_hooks(project)?);
    Ok(hooks)
}
