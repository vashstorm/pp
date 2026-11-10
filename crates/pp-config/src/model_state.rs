//! 进程内唯一的模型决议权威：`a`（手动覆盖，仅内存）优先于 `b`（配置默认）。
//!
//! 单例由 `main::run` 构造 `Arc<ModelState>`（或经 [`SharedModels`] 延迟安装）并显式注入
//! `Bootstrap`、`Agent` 与 UI；不使用全局静态。
//!
//! 约束：
//! - `a` 不落盘，进程退出即丢弃；重启回到 `b`。
//! - 读侧（`resolve`/`catalog_snapshot`/`revision`）只拿读锁拷贝结果，绝不持锁做 I/O。
//! - 写入口（`set_override`/`clear_override`/`set_default`/`set_role`/`reload_models`）
//!   先在锁外完成校验与 `agent.db` 事务，再短拿写锁发布；必须由单一写者串行调用
//!   （`model_control::Service` 线程或 CLI 主线程）。
//! - 不变量：`resolve() == a.unwrap_or(b)`；`a.is_some() ⇒ a != b`；
//!   `a.is_none()` 时 `resolve()` 随 `b` 变化。

use std::{
    path::Path,
    sync::{Arc, RwLock},
};

use anyhow::{Result, ensure};
use pp_core::model::{ModelSource, ThinkLevel};

use crate::config::{ModelCatalog, ModelChoice, ModelRole, ModelRoles, RoleAssignment};

/// 一次模型选择的完整快照：完整配置引用 + 已解析档位。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selection {
    pub reference: String,
    pub effort: ThinkLevel,
}

struct Inner {
    catalog: ModelCatalog,
    /// `a`：手动覆盖；仅内存。
    override_selection: Option<Selection>,
    revision: u64,
}

pub struct ModelState {
    inner: RwLock<Inner>,
}

impl ModelState {
    pub fn from_catalog(catalog: ModelCatalog) -> Self {
        Self {
            inner: RwLock::new(Inner {
                catalog,
                override_selection: None,
                revision: 0,
            }),
        }
    }

    pub fn shared(catalog: ModelCatalog) -> Arc<Self> {
        Arc::new(Self::from_catalog(catalog))
    }

    pub fn load(settings_path: &Path, models_path: &Path) -> Result<Arc<Self>> {
        Ok(Self::shared(ModelCatalog::from_files(
            settings_path,
            models_path,
        )?))
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Inner> {
        self.inner.read().unwrap_or_else(|error| error.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner
            .write()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub fn revision(&self) -> u64 {
        self.read().revision
    }

    /// 配置快照；调用方在请求边界自行刷新，不逐请求 clone。
    pub fn catalog_snapshot(&self) -> ModelCatalog {
        self.read().catalog.clone()
    }

    pub fn choices(&self) -> Vec<ModelChoice> {
        self.read().catalog.choices()
    }

    pub fn model_roles(&self) -> ModelRoles {
        self.read().catalog.model_roles().clone()
    }

    pub fn default_think_level(&self) -> ThinkLevel {
        self.read().catalog.default_think_level()
    }

    pub fn favorite_models(&self) -> Vec<String> {
        self.read().catalog.favorite_models().to_vec()
    }

    /// `b`：配置默认（启用的 DEFAULT 角色 → `preferences.default_model` → 首个可用模型）。
    pub fn default_selection(&self) -> Result<Selection> {
        Self::default_selection_of(&self.read().catalog)
    }

    fn default_selection_of(catalog: &ModelCatalog) -> Result<Selection> {
        let reference = catalog.default_reference().to_owned();
        let config = catalog.resolve(&reference)?;
        Ok(Selection {
            reference,
            effort: config
                .thinking
                .effective_default(catalog.default_think_level()),
        })
    }

    /// `a`：手动覆盖；`None` 表示跟随配置默认。
    pub fn override_selection(&self) -> Option<Selection> {
        self.read().override_selection.clone()
    }

    pub fn source(&self) -> ModelSource {
        if self.read().override_selection.is_some() {
            ModelSource::Override
        } else {
            ModelSource::Default
        }
    }

    /// 唯一决议出口：`a.unwrap_or(b)`；模型不可用时返回错误，不静默降级。
    pub fn resolve(&self) -> Result<Selection> {
        let inner = self.read();
        if let Some(selection) = &inner.override_selection {
            let config = inner.catalog.resolve(&selection.reference)?;
            ensure!(
                config.thinking.supports(selection.effort),
                "override thinking level is not supported by the selected model"
            );
            return Ok(selection.clone());
        }
        Self::default_selection_of(&inner.catalog)
    }

    /// 把一次切换解析成完整选择，沿用既有的目标档位策略：
    /// 模型显式 `defaultLevel` 优先 → 保留当前档位（目标模型支持时）→ 该模型的有效默认档。
    pub fn resolve_reference(
        &self,
        reference: &str,
        current: Option<ThinkLevel>,
    ) -> Result<Selection> {
        let inner = self.read();
        let config = inner.catalog.resolve(reference)?;
        let effort = config.thinking.explicit_default().unwrap_or_else(|| {
            current
                .filter(|level| config.thinking.supports(*level))
                .unwrap_or_else(|| {
                    config
                        .thinking
                        .effective_default(inner.catalog.default_think_level())
                })
        });
        Ok(Selection {
            reference: reference.to_owned(),
            effort,
        })
    }

    /// 写入 `a`；与 `b` 完全相同时不保留覆盖。返回是否记录了覆盖。
    pub fn set_override(&self, selection: Selection) -> Result<bool> {
        {
            let inner = self.read();
            let config = inner.catalog.resolve(&selection.reference)?;
            ensure!(
                config.thinking.supports(selection.effort),
                "model {} does not support effort {}; options: {}",
                selection.reference,
                selection.effort.as_str(),
                config
                    .thinking
                    .levels()
                    .map(ThinkLevel::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            if Self::default_selection_of(&inner.catalog).is_ok_and(|default| default == selection)
            {
                dbg_file(&format!("set_override EQUALS_B ref={}", selection.reference));
                drop(inner);
                self.clear_override()?;
                return Ok(false);
            }
        }
        dbg_file(&format!("set_override SET ref={}", selection.reference));
        let mut inner = self.write();
        inner.override_selection = Some(selection);
        inner.revision += 1;
        Ok(true)
    }

    /// 清除 `a`（回到 `b`）。返回是否真的有覆盖被清除。
    pub fn clear_override(&self) -> Result<bool> {
        let mut inner = self.write();
        let changed = inner.override_selection.take().is_some();
        dbg_file(&format!("clear_override changed={changed}"));
        if changed {
            inner.revision += 1;
        }
        Ok(changed)
    }

    /// 手动档位：只改内存中的覆盖档位，不写配置文件。
    pub fn set_override_effort(&self, effort: ThinkLevel) -> Result<()> {
        let mut inner = self.write();
        let Some(selection) = inner.override_selection.clone() else {
            return Ok(());
        };
        let config = inner.catalog.resolve(&selection.reference)?;
        ensure!(
            config.thinking.supports(effort),
            "thinking level is not supported by the current model"
        );
        if selection.effort != effort {
            inner.override_selection = Some(Selection {
                reference: selection.reference,
                effort,
            });
            inner.revision += 1;
        }
        Ok(())
    }

    /// 保存全局默认模型（写 `b`，落 `agent.db`）。
    pub fn set_default(&self, reference: &str) -> Result<()> {
        self.mutate(|catalog| catalog.save_default_model(reference))
    }

    /// 保存全局默认档位（写 `b`）。
    pub fn save_default_think_level(&self, level: ThinkLevel) -> Result<()> {
        self.mutate(|catalog| catalog.save_default_think_level(level))
    }

    /// 保存角色配置；DEFAULT 角色即 `b`。
    pub fn set_role(
        &self,
        role: ModelRole,
        assignment: Option<RoleAssignment>,
    ) -> Result<ModelRoles> {
        self.mutate(|catalog| catalog.save_model_role(role, assignment))?;
        Ok(self.model_roles())
    }

    pub fn toggle_favorite_model(&self, reference: &str) -> Result<Vec<String>> {
        self.mutate(|catalog| catalog.toggle_favorite_model(reference).map(|_| ()))?;
        Ok(self.favorite_models())
    }

    pub fn reload_models(&self) -> Result<()> {
        self.mutate(ModelCatalog::reload_models)
    }

    pub fn model_usage(&self) -> Result<std::collections::BTreeMap<String, u64>> {
        self.read().catalog.model_usage()
    }

    /// 写入口：锁外校验/持久化，锁内只换快照并递增 revision。
    fn mutate(&self, update: impl FnOnce(&mut ModelCatalog) -> Result<()>) -> Result<()> {
        let mut next = self.catalog_snapshot();
        update(&mut next)?;
        let mut inner = self.write();
        inner.catalog = next;
        inner.revision += 1;
        Self::reconcile_override(&mut inner);
        Ok(())
    }

    /// 目录/默认值变化后修正 `a`：模型不可用或与 `b` 重合时丢弃覆盖。
    fn reconcile_override(inner: &mut Inner) {
        let Some(selection) = inner.override_selection.clone() else {
            return;
        };
        let available = inner
            .catalog
            .resolve(&selection.reference)
            .is_ok_and(|config| config.thinking.supports(selection.effort));
        let duplicates_default =
            Self::default_selection_of(&inner.catalog).is_ok_and(|default| default == selection);
        if !available || duplicates_default {
            inner.override_selection = None;
        }
    }
}

/// 进程内唯一模型状态的安装槽：首次成功加载后一直复用，认证/登录重建 Agent 不丢失 `a`。
#[derive(Clone, Default)]
pub struct SharedModels {
    slot: Arc<RwLock<Option<Arc<ModelState>>>>,
}

impl SharedModels {
    pub fn install(&self, state: Arc<ModelState>) {
        *self.slot.write().unwrap_or_else(|error| error.into_inner()) = Some(state);
    }

    pub fn current(&self) -> Option<Arc<ModelState>> {
        self.slot
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// 已安装则复用；否则加载并安装。加载失败不安装，调用方可重试。
    pub fn get_or_load(&self, settings_path: &Path, models_path: &Path) -> Result<Arc<ModelState>> {
        if let Some(state) = self.current() {
            return Ok(state);
        }
        let state = ModelState::load(settings_path, models_path)?;
        let mut slot = self.slot.write().unwrap_or_else(|error| error.into_inner());
        Ok(slot.get_or_insert(state).clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> (tempfile::TempDir, Arc<ModelState>) {
        let dir = tempfile::tempdir().unwrap();
        write_models(&dir);
        fs::write(
            dir.path().join("settings.yml"),
            "default_model: fixture/one\nedit: replace\n",
        )
        .unwrap();
        let catalog = ModelCatalog::from_files(
            &dir.path().join("settings.yml"),
            &dir.path().join("models.yml"),
        )
        .unwrap();
        (dir, ModelState::shared(catalog))
    }

    fn write_models(dir: &tempfile::TempDir) {
        fs::write(
            dir.path().join("models.yml"),
            r#"providers:
  fixture:
    api_key: fixture-key
    models:
      one: {model: one}
      two: {model: two, thinking: {efforts: [low, high], defaultLevel: low}}
"#,
        )
        .unwrap();
    }

    fn selection(reference: &str, effort: ThinkLevel) -> Selection {
        Selection {
            reference: reference.into(),
            effort,
        }
    }

    #[test]
    fn resolve_prefers_override_and_clears_when_equal_to_default() {
        let (_dir, state) = fixture();
        assert_eq!(state.source(), ModelSource::Default);
        assert_eq!(
            state.resolve().unwrap(),
            selection("fixture/one", ThinkLevel::None)
        );

        let two = state
            .resolve_reference("fixture/two", None)
            .expect("resolution succeeds");
        assert_eq!(two, selection("fixture/two", ThinkLevel::Low));
        assert!(state.set_override(two.clone()).unwrap());
        assert_eq!(state.source(), ModelSource::Override);
        assert_eq!(state.resolve().unwrap(), two);

        // 与配置默认一致时不保留覆盖。
        assert!(
            !state
                .set_override(selection("fixture/one", ThinkLevel::None))
                .unwrap()
        );
        assert_eq!(state.source(), ModelSource::Default);
        assert_eq!(state.override_selection(), None);
        assert_eq!(
            state.resolve().unwrap(),
            selection("fixture/one", ThinkLevel::None)
        );
    }

    #[test]
    fn save_default_keeps_or_clears_override() {
        let (_dir, state) = fixture();
        assert!(
            state
                .set_override(selection("fixture/two", ThinkLevel::Low))
                .unwrap()
        );
        // 新默认与覆盖不同：覆盖保留并继续生效。
        state.set_default("fixture/one").unwrap();
        assert_eq!(state.source(), ModelSource::Override);
        assert_eq!(
            state.resolve().unwrap(),
            selection("fixture/two", ThinkLevel::Low)
        );

        // 新默认与覆盖相同：覆盖被清除，来源回到配置默认。
        state.set_default("fixture/two").unwrap();
        assert_eq!(state.source(), ModelSource::Default);
        assert_eq!(
            state.resolve().unwrap(),
            selection("fixture/two", ThinkLevel::Low)
        );
    }

    #[test]
    fn unavailable_override_is_dropped_by_reload() {
        let (dir, state) = fixture();
        assert!(
            state
                .set_override(selection("fixture/two", ThinkLevel::Low))
                .unwrap()
        );
        fs::write(
            dir.path().join("models.yml"),
            r#"providers:
  fixture:
    api_key: fixture-key
    models:
      one: {model: one}
"#,
        )
        .unwrap();
        state.reload_models().unwrap();
        assert_eq!(state.override_selection(), None);
        assert_eq!(
            state.resolve().unwrap(),
            selection("fixture/one", ThinkLevel::None)
        );
    }

    #[test]
    fn failed_reload_keeps_previous_snapshot_and_override() {
        let (dir, state) = fixture();
        assert!(
            state
                .set_override(selection("fixture/two", ThinkLevel::Low))
                .unwrap()
        );
        fs::write(dir.path().join("models.yml"), "providers: {}\n").unwrap();
        assert!(state.reload_models().is_err());
        // 失败不改内存快照：覆盖继续生效，不会被静默替换。
        assert_eq!(
            state.resolve().unwrap(),
            selection("fixture/two", ThinkLevel::Low)
        );
        assert_eq!(state.source(), ModelSource::Override);
    }

    #[test]
    fn revision_tracks_writes_and_is_shared() {
        let (_dir, state) = fixture();
        let clone = Arc::clone(&state);
        let start = state.revision();
        assert!(
            clone
                .set_override(selection("fixture/two", ThinkLevel::High))
                .unwrap()
        );
        assert_eq!(state.revision(), start + 1);
        clone.set_override_effort(ThinkLevel::Low).unwrap();
        assert_eq!(state.revision(), start + 2);
        clone.clear_override().unwrap();
        assert_eq!(state.revision(), start + 3);
        // 无变化不递增。
        clone.clear_override().unwrap();
        assert_eq!(state.revision(), start + 3);
    }

    #[test]
    fn concurrent_readers_observe_consistent_snapshots() {
        let (_dir, state) = fixture();
        let writer = Arc::clone(&state);
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    for _ in 0..64 {
                        let _ = state.resolve().unwrap();
                        let _ = state.catalog_snapshot().default_reference().to_owned();
                    }
                })
            })
            .collect();
        for _ in 0..16 {
            writer
                .set_override(selection("fixture/two", ThinkLevel::Low))
                .unwrap();
            writer.clear_override().unwrap();
        }
        for worker in workers {
            worker.join().unwrap();
        }
    }

    #[test]
    fn shared_models_install_once_and_reuse_state() {
        let (dir, state) = fixture();
        let shared = SharedModels::default();
        assert!(shared.current().is_none());
        let installed = shared
            .get_or_load(
                &dir.path().join("settings.yml"),
                &dir.path().join("models.yml"),
            )
            .unwrap();
        installed
            .set_override(selection("fixture/two", ThinkLevel::Low))
            .unwrap();
        let again = shared
            .get_or_load(
                &dir.path().join("settings.yml"),
                &dir.path().join("models.yml"),
            )
            .unwrap();
        assert!(Arc::ptr_eq(&installed, &again));
        assert_eq!(
            again.resolve().unwrap(),
            selection("fixture/two", ThinkLevel::Low)
        );
        shared.install(Arc::clone(&state));
        assert!(Arc::ptr_eq(&shared.current().unwrap(), &state));
    }
}

fn dbg_file(message: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("/tmp/pp-dbg.log")
    {
        let _ = writeln!(file, "{message}");
    }
}
