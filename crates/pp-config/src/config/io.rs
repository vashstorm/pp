//! `settings.yml` 与 `hooks.yml` 的读写及持锁原子保存。
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
    // 首次读取之前取得同目录 settings.lock，把解析、更新、临时文件写入、
    // 比较和 persist 全程串行化：跨进程的两个保存入口不再各自基于旧文本覆盖对方。
    // 锁获取失败不保存；设置保存没有外部取消令牌，锁等待沿用认证锁的 30 秒上限。
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
    // 测试专用控制点：只在显式注册的写线程同步一次，用于确定性复现并发保存交错。
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
        // 缺少 settings.yml 时使用默认值；目录或无效 UTF-8 等其他读取错误仍会阻止加载。
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

/// 读取一个 hooks YAML 文件：文件缺失、空文件或只有注释都视为空配置；
/// 其他读取错误、格式错误与结构校验失败都带来源路径返回，不泄露文件内容。
pub fn read_hooks(path: &Path) -> Result<HooksSettings> {
    match fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(HooksSettings::default()),
        result => {
            let text =
                result.with_context(|| format!("reading YAML hooks config {}", path.display()))?;
            // 空文件与只有注释的文件解析为 null 文档，等同未配置，不要求写成 `{}`。
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

/// 用户级 `hooks.yml` 与项目级 `.pp/hooks.yml` 合并：项目级组追加在用户级之后，
/// 同事件内按该顺序匹配；两个来源互不覆盖。
pub fn load_hooks(user: &Path, project: &Path) -> Result<HooksSettings> {
    let mut hooks = read_hooks(user)?;
    hooks.extend(&read_hooks(project)?);
    Ok(hooks)
}
