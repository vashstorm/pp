//! 项目计划身份登记表（`.pp/plan-index.json`）与计划级辅助锁。
//!
//! 登记表是跨会话的稳定身份来源：只保存 `{id, file_name, location, deleted,
//! deleted_file_identity}`，不保存正文、标题、进度或会话信息；它不是第二套
//! TODO 数据库，也不授予执行权限。所有读写都经 `Workspace`/`Target` 的
//! NOFOLLOW 安全路径；并发变更通过读取基线与 `save_if_unchanged` 拒绝，
//! 绝不自动重试或覆盖。
//!
//! 锁约定：同一计划 ID 的模型任务与管理动作互斥使用 `.pp/plan-locks/<id>.lock`
//! （`PlanLease`）；登记表的短事务另用 `.pp/plan-index.lock`
//! （`PlanCatalogLock`）。同时持有时顺序固定为计划 lease → 登记表锁。
//! `load_catalog`/`save_catalog` 不自行加锁，调用方必须已持有
//! `PlanCatalogLock`。

use anyhow::{Context, Result, bail, ensure};
use rustix::{
    fs::{FlockOperation, Mode, OFlags, flock},
    io::Errno,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, ffi::OsString, fs::File, path::Path};

use super::save::Workspace;
use crate::plan::{PlanLocation, valid_id, validate_stored_file_name};

/// 登记表读取上限 16 MiB；损坏或超大的登记文件必须显式报错，禁止当空表覆盖。
pub(crate) const CATALOG_MAX_BYTES: usize = 16 * 1024 * 1024;

/// 当前登记表 schema 版本；加载时不匹配即拒绝。
pub(crate) const CATALOG_VERSION: u32 = 1;

const INDEX_PATH: &str = ".pp/plan-index.json";
const PP_DIR_NAME: &str = ".pp";
const INDEX_LOCK_NAME: &str = "plan-index.lock";
const LOCKS_DIR_NAME: &str = "plan-locks";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanCatalog {
    pub(crate) version: u32,
    pub(crate) entries: Vec<PlanCatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanCatalogEntry {
    pub(crate) id: String,
    pub(crate) file_name: String,
    pub(crate) location: PlanLocation,
    pub(crate) deleted: bool,
    /// 删除前记录的文件身份；仅 tombstone 允许携带，用于区分删除残留与
    /// 后来在相同路径新建的不同文件。为 `None` 时不写入序列化结果。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) deleted_file_identity: Option<PlanFileIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanFileIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

/// 登记表读取快照：解析后的目录与磁盘上的精确原始字节。
/// `raw` 提供下一次 `save_catalog` 的基线；`None` 表示登记文件尚不存在。
#[derive(Debug)]
pub(crate) struct CatalogSnapshot {
    pub(crate) catalog: PlanCatalog,
    pub(crate) raw: Option<Vec<u8>>,
}

impl PlanCatalog {
    pub(crate) fn empty() -> Self {
        Self {
            version: CATALOG_VERSION,
            entries: Vec::new(),
        }
    }

    /// 按 ID 查找活动（未删除）条目；tombstone 不参与身份解析。
    pub(crate) fn active_by_id(&self, id: &str) -> Option<&PlanCatalogEntry> {
        self.entries
            .iter()
            .find(|entry| !entry.deleted && entry.id == id)
    }

    /// 按活动文件名查找条目；三个生命周期目录共享全局唯一命名空间。
    pub(crate) fn active_by_file_name(&self, file_name: &str) -> Option<&PlanCatalogEntry> {
        self.entries
            .iter()
            .find(|entry| !entry.deleted && entry.file_name == file_name)
    }
}

/// 校验登记表结构：版本、ID/文件名合法性、活动条目的全局唯一性，
/// 以及墓碑身份字段只允许出现在已删除条目上。任何违规都返回明确错误；
/// 调用方不得把违规登记表当空表覆盖。
pub(crate) fn validate_catalog(catalog: &PlanCatalog) -> Result<()> {
    ensure!(
        catalog.version == CATALOG_VERSION,
        "Plan index version {} is unsupported; refusing to load it.",
        catalog.version
    );
    let mut ids = HashSet::with_capacity(catalog.entries.len());
    let mut active_names = HashSet::with_capacity(catalog.entries.len());
    for entry in &catalog.entries {
        ensure!(
            valid_id(&entry.id),
            "Plan index entry has an invalid id; refusing to load it."
        );
        validate_stored_file_name(&entry.file_name).with_context(|| {
            format!(
                "Plan index entry {} has an invalid file name; refusing to load it.",
                entry.id
            )
        })?;
        ensure!(
            ids.insert(entry.id.as_str()),
            "Plan index has duplicate id {}; refusing to load it.",
            entry.id
        );
        if entry.deleted {
            continue;
        }
        ensure!(
            entry.deleted_file_identity.is_none(),
            "Plan index entry {} is active but records a deleted-file identity; refusing to load it.",
            entry.id
        );
        ensure!(
            active_names.insert(entry.file_name.as_str()),
            "Plan index has duplicate active file name {}; refusing to load it.",
            entry.file_name
        );
    }
    Ok(())
}

/// 读取登记表；调用方必须已持有 `PlanCatalogLock`。
/// `.pp` 或登记文件缺失表示尚未登记，返回空目录且 `raw` 为 `None`；
/// 损坏、超限或非法字段返回明确错误，绝不返回可覆盖的“空表”假象。
pub(crate) fn load_catalog(workspace: &Workspace) -> Result<CatalogSnapshot> {
    let target = match workspace.target(Path::new(INDEX_PATH), false) {
        Ok(target) => target,
        Err(error) if is_missing(&error) => {
            return Ok(CatalogSnapshot {
                catalog: PlanCatalog::empty(),
                raw: None,
            });
        }
        Err(error) => return Err(error),
    };
    let raw = match target.read_bounded(CATALOG_MAX_BYTES) {
        Ok(raw) => raw,
        Err(error) if is_missing(&error) => {
            return Ok(CatalogSnapshot {
                catalog: PlanCatalog::empty(),
                raw: None,
            });
        }
        Err(error) => return Err(error),
    };
    ensure!(
        raw.len() <= CATALOG_MAX_BYTES,
        "Plan index exceeds the 16 MiB read limit; refusing to load it."
    );
    ensure!(
        !raw.is_empty(),
        "Plan index is empty; refusing to treat it as an empty catalog."
    );
    let catalog = parse_catalog(&raw)?;
    Ok(CatalogSnapshot {
        catalog,
        raw: Some(raw),
    })
}

/// 以 `previous` 提供的精确字节为基线提交登记表；调用方必须已持有
/// `PlanCatalogLock`。`previous` 为 `None` 表示登记文件尚未存在，用排他
/// 创建发布；并发创建或基线变化都会显式失败，不自动重试。返回值为父目录
/// 同步警告（成功提交后可能出现，不代表失败）。
pub(crate) fn save_catalog(
    workspace: &Workspace,
    previous: Option<&[u8]>,
    catalog: &PlanCatalog,
) -> Result<Option<String>> {
    validate_catalog(catalog)?;
    let mut bytes = serde_json::to_vec_pretty(catalog).context("serializing plan index")?;
    bytes.push(b'\n');
    let target = workspace
        .target(Path::new(INDEX_PATH), true)
        .context("opening plan index for saving")?;
    match previous {
        None => target
            .save_new_with_conflict(
                &bytes,
                "Plan index was created concurrently; reload it before saving.",
            )
            .context("saving plan index"),
        Some(expected) => target
            .save_if_unchanged_custom(
                expected,
                &bytes,
                "Plan index changed concurrently; reload it before saving.",
            )
            .context("saving plan index"),
    }
}

fn parse_catalog(raw: &[u8]) -> Result<PlanCatalog> {
    let catalog: PlanCatalog = serde_json::from_slice(raw).map_err(|_| {
        // 不透出 serde/parser 细节，避免把内部格式当作可依赖的诊断。
        anyhow::anyhow!("Plan index is corrupted; refusing to load or overwrite it.")
    })?;
    validate_catalog(&catalog)?;
    Ok(catalog)
}

/// 判断错误链中是否只是“路径不存在”；目录或文件缺失表示尚未登记。
/// 其他错误（例如 `.pp` 被替换成普通文件）必须暴露，不能当空表。
fn is_missing(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<Errno>() == Some(&Errno::NOENT))
}

/// 非阻塞独占 `flock`；占用时报错不等待，`EINTR` 重试。
fn acquire_flock(file: &File, busy: &'static str) -> Result<()> {
    loop {
        match flock(file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(()),
            Err(Errno::WOULDBLOCK) => bail!("{busy}"),
            Err(Errno::INTR) => continue,
            Err(error) => return Err(error).context("acquiring plan lock"),
        }
    }
}

fn release_flock(file: &File) {
    let _ = flock(file, FlockOperation::Unlock);
}

/// 计划级辅助锁：同一计划 ID 的模型任务与管理动作互斥。fd-safe 创建
/// （NOFOLLOW、0600），`flock` 独占非阻塞；持有普通 `File`，Drop 即关闭
/// 解锁。锁文件常驻 `.pp/plan-locks/`，不随计划正文重命名或删除。占用时
/// 返回错误，不阻塞等待；注意同一进程内不同 fd 也会冲突，已持有 lease 的
/// 调用方应复用而不是重复 acquire。
#[derive(Debug)]
pub(crate) struct PlanLease {
    id: String,
    file: File,
}

impl PlanLease {
    pub(crate) fn acquire(workspace: &Workspace, id: &str) -> Result<Self> {
        ensure!(valid_id(id), "plan id must be 32 hexadecimal characters");
        let locks = workspace
            .directory(
                &[OsString::from(PP_DIR_NAME), OsString::from(LOCKS_DIR_NAME)],
                true,
            )
            .context("opening plan lock directory (symlink paths are forbidden)")?;
        let name = format!("{id}.lock");
        let file = File::from(
            rustix::fs::openat(
                &locks,
                name.as_str(),
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .context("opening plan lock file (symlinks are forbidden)")?,
        );
        ensure!(
            file.metadata()
                .context("checking plan lock file")?
                .is_file(),
            "plan lock path must be a regular file"
        );
        acquire_flock(
            &file,
            "Plan is in use; stop the active task before managing it.",
        )?;
        Ok(Self {
            id: id.to_owned(),
            file,
        })
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for PlanLease {
    fn drop(&mut self) {
        release_flock(&self.file);
    }
}

/// 登记表短事务锁 `.pp/plan-index.lock`：只保护登记表的读改写事务，
/// 列表纳管也只取此锁。锁顺序固定：先取得目标计划的 `PlanLease`，再取本锁。
#[derive(Debug)]
pub(crate) struct PlanCatalogLock(File);

impl PlanCatalogLock {
    pub(crate) fn acquire(workspace: &Workspace) -> Result<Self> {
        let pp = workspace
            .directory(&[OsString::from(PP_DIR_NAME)], true)
            .context("opening .pp directory (symlink paths are forbidden)")?;
        let file = File::from(
            rustix::fs::openat(
                &pp,
                INDEX_LOCK_NAME,
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .context("opening plan index lock file (symlinks are forbidden)")?,
        );
        ensure!(
            file.metadata()
                .context("checking plan index lock file")?
                .is_file(),
            "plan index lock path must be a regular file"
        );
        acquire_flock(
            &file,
            "Plan index is busy; retry the catalog update shortly.",
        )?;
        Ok(Self(file))
    }
}

impl Drop for PlanCatalogLock {
    fn drop(&mut self) {
        release_flock(&self.0);
    }
}

/// 安全读取相对路径目标的文件身份；缺失返回 `None`，符号链接与非普通
/// 文件报错。供登记墓碑记录与同 inode 残留确认使用，不能替代操作前的
/// 精确字节读取。
pub(crate) fn file_identity(
    workspace: &Workspace,
    path: &Path,
) -> Result<Option<PlanFileIdentity>> {
    let target = workspace.target(path, false)?;
    Ok(target
        .stat_optional()?
        .map(|(device, inode)| PlanFileIdentity { device, inode }))
}

/// 生命周期搬迁的发布步骤：核对当前登记（墓碑、源身份、目标重名）后，
/// 在目标位置发布不覆盖的同 inode 链接，并以 CAS 原子更新登记表路径。
/// 失败保留源链接；同 inode 的既有目标允许幂等重入。锁顺序：调用方须
/// 先持有计划 `PlanLease`，再由本函数获取 `PlanCatalogLock`。发布成功
/// 后由调用方提交 workflow/handoff，再调用 `commit_plan_move` 移除源名。
pub(crate) fn publish_plan_move(
    workspace: &Workspace,
    id: &str,
    source: (&str, PlanLocation),
    target: (&str, PlanLocation),
    expected: &[u8],
) -> Result<Option<String>> {
    let _lock = PlanCatalogLock::acquire(workspace)?;
    let snapshot = load_catalog(workspace)?;
    if snapshot
        .catalog
        .entries
        .iter()
        .any(|entry| entry.deleted && entry.id == id)
    {
        bail!("Plan was deleted; choose another plan from /plan-list.");
    }
    let (source_name, source_location) = source;
    let (target_name, target_location) = target;
    match snapshot.catalog.active_by_id(id) {
        // 已在目标身份：幂等重入，只核对同 inode 链接，登记无需更新。
        Some(entry) if entry.file_name == target_name && entry.location == target_location => {
            return workspace.link_plan(
                Path::new(&path_for_name(source_name, source_location)),
                Path::new(&path_for_name(target_name, target_location)),
                expected,
            );
        }
        // 正常搬迁：登记条目必须仍指向源身份；否则登记表已被其他会话
        // 更改，fail-closed 等待按 ID 协调。
        Some(entry) => {
            ensure!(
                entry.file_name == source_name && entry.location == source_location,
                "Plan registration moved; reopen the plan from /plan-list."
            );
        }
        None => {}
    }
    ensure!(
        snapshot
            .catalog
            .entries
            .iter()
            .all(|entry| entry.deleted || entry.id == id || entry.file_name != target_name),
        "Plan file name is registered to another plan."
    );
    let warning = workspace.link_plan(
        Path::new(&path_for_name(source_name, source_location)),
        Path::new(&path_for_name(target_name, target_location)),
        expected,
    )?;
    let mut catalog = snapshot.catalog;
    match catalog
        .entries
        .iter_mut()
        .find(|entry| entry.id == id && !entry.deleted)
    {
        Some(entry) => {
            entry.file_name = target_name.to_owned();
            entry.location = target_location;
        }
        None => catalog.entries.push(PlanCatalogEntry {
            id: id.to_owned(),
            file_name: target_name.to_owned(),
            location: target_location,
            deleted: false,
            deleted_file_identity: None,
        }),
    }
    save_catalog(workspace, snapshot.raw.as_deref(), &catalog)
        .map(|registration_warning| warning.or(registration_warning))
}

/// 生命周期搬迁的提交步骤：调用方已提交 workflow/handoff 后移除源链接。
/// 失败保留源链接，交由按 ID 协调做同 inode 残留清理。
pub(crate) fn commit_plan_move(
    workspace: &Workspace,
    source: (&str, PlanLocation),
    target: (&str, PlanLocation),
    expected: &[u8],
) -> Result<Option<String>> {
    workspace.finish_plan_move(
        Path::new(&path_for_name(source.0, source.1)),
        Path::new(&path_for_name(target.0, target.1)),
        expected,
    )
}

/// 恢复协调的残留清理：仅当旧生命周期路径与新登记路径确认为同一 inode
/// 时移除旧名；缺失、不同 inode、符号链接或任何读取异常都保留现状，
/// 绝不越权删除，也不能让未确认的残留挡住计划访问。
pub(crate) fn cleanup_confirmed_leftover(
    workspace: &Workspace,
    current: &str,
    leftover: &str,
) -> Result<()> {
    if current == leftover {
        return Ok(());
    }
    let current_target = workspace.target(Path::new(current), false)?;
    let Some(current_identity) = current_target.stat_optional()? else {
        return Ok(());
    };
    let Ok(leftover_target) = workspace.target(Path::new(leftover), false) else {
        // 符号链接/特殊文件/缺失目录等异常残留不触碰，留给显式管理。
        return Ok(());
    };
    let Ok(Some(leftover_identity)) = leftover_target.stat_optional() else {
        return Ok(());
    };
    if leftover_identity != current_identity {
        return Ok(());
    }
    leftover_target.unlink()?;
    Ok(())
}

fn path_for_name(file_name: &str, location: PlanLocation) -> String {
    crate::plan::path_for(file_name, location)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{MetadataExt, symlink},
    };

    fn workspace(dir: &tempfile::TempDir) -> Workspace {
        Workspace::open(dir.path()).unwrap()
    }

    fn sample_id(number: u8) -> String {
        format!("{number:032x}")
    }

    fn entry(id: &str, file_name: &str, location: PlanLocation, deleted: bool) -> PlanCatalogEntry {
        PlanCatalogEntry {
            id: id.to_owned(),
            file_name: file_name.to_owned(),
            location,
            deleted,
            deleted_file_identity: None,
        }
    }

    fn index_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join(".pp/plan-index.json")
    }

    #[test]
    fn missing_index_loads_as_empty_and_first_save_creates_exclusively() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let snapshot = load_catalog(&ws).unwrap();
        assert!(snapshot.raw.is_none());
        assert!(snapshot.catalog.entries.is_empty());

        let warning = save_catalog(&ws, snapshot.raw.as_deref(), &snapshot.catalog).unwrap();
        assert_eq!(warning, None);
        assert!(index_path(&dir).exists());

        let again = load_catalog(&ws).unwrap();
        assert!(again.raw.is_some());
        assert_eq!(again.catalog.entries.len(), 0);

        // 已存在的登记文件不能再以 None 基线排他创建。
        let error = save_catalog(&ws, None, &again.catalog).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("Plan index was created concurrently; reload it before saving.")
        );
    }

    #[test]
    fn catalog_roundtrip_preserves_entries_and_rejects_stale_baseline() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let mut catalog = PlanCatalog::empty();
        catalog.entries.push(entry(
            &sample_id(1),
            "fix-config-loading.md",
            PlanLocation::Drafts,
            false,
        ));
        save_catalog(&ws, None, &catalog).unwrap();

        let snapshot = load_catalog(&ws).unwrap();
        assert_eq!(snapshot.catalog, catalog);
        assert_eq!(
            snapshot
                .catalog
                .active_by_id(&sample_id(1))
                .unwrap()
                .file_name,
            "fix-config-loading.md"
        );
        assert_eq!(
            snapshot
                .catalog
                .active_by_file_name("fix-config-loading.md")
                .unwrap()
                .id,
            sample_id(1)
        );
        assert!(snapshot.catalog.active_by_file_name("missing.md").is_none());

        let mut changed = snapshot.catalog.clone();
        changed.entries.push(entry(
            &sample_id(2),
            "second-plan.md",
            PlanLocation::Plans,
            false,
        ));
        // 另一个写者先提交，使 snapshot.raw 成为过期基线。
        let mut other = snapshot.catalog.clone();
        other.entries.push(entry(
            &sample_id(3),
            "third-plan.md",
            PlanLocation::Drafts,
            false,
        ));
        save_catalog(&ws, snapshot.raw.as_deref(), &other).unwrap();
        let error = save_catalog(&ws, snapshot.raw.as_deref(), &changed).unwrap_err();
        assert!(
            format!("{error:#}")
                .contains("Plan index changed concurrently; reload it before saving.")
        );
        // 拒绝不覆盖磁盘：旧内容原样保留。
        assert_eq!(load_catalog(&ws).unwrap().catalog, other);

        let fresh = load_catalog(&ws).unwrap();
        save_catalog(&ws, fresh.raw.as_deref(), &changed).unwrap();
        assert_eq!(load_catalog(&ws).unwrap().catalog, changed);
    }

    #[test]
    fn corrupted_index_is_never_treated_as_empty_or_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        assert!(load_catalog(&ws).unwrap().raw.is_none());
        fs::create_dir_all(dir.path().join(".pp")).unwrap();

        let broken_samples = [
            "",
            "not json",
            "{\"version\":2,\"entries\":[]}",
            "{\"version\":1,\"entries\":[],\"extra\":1}",
            "{}",
            "{\"version\":1,\"entries\":[{\"id\":\"zz\",\"file_name\":\"a.md\",\"location\":\"drafts\",\"deleted\":false}]}",
        ];
        let mut last = Vec::new();
        for broken in broken_samples {
            fs::write(index_path(&dir), broken).unwrap();
            let error = load_catalog(&ws).unwrap_err();
            let message = error.to_string();
            assert!(message.contains("refusing"), "{broken:?} => {message}");
            // 登记失败也不覆盖既有坏文件。
            assert!(save_catalog(&ws, None, &PlanCatalog::empty()).is_err());
            last = broken.as_bytes().to_vec();
        }
        assert_eq!(fs::read(index_path(&dir)).unwrap(), last);
    }

    #[test]
    fn oversize_index_is_rejected_within_bounded_read() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        fs::create_dir_all(dir.path().join(".pp")).unwrap();
        fs::write(index_path(&dir), vec![b'a'; CATALOG_MAX_BYTES + 1]).unwrap();
        let error = load_catalog(&ws).unwrap_err();
        assert!(error.to_string().contains("16 MiB"), "{error}");
    }

    #[test]
    fn validate_rejects_duplicate_ids_names_and_misplaced_identity() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);

        let mut duplicate_id = PlanCatalog::empty();
        duplicate_id.entries.push(entry(
            &sample_id(1),
            "first.md",
            PlanLocation::Drafts,
            false,
        ));
        duplicate_id.entries.push(entry(
            &sample_id(1),
            "second.md",
            PlanLocation::Drafts,
            false,
        ));
        let error = validate_catalog(&duplicate_id).unwrap_err();
        assert!(error.to_string().contains("duplicate id"), "{error}");
        assert!(save_catalog(&ws, None, &duplicate_id).is_err());
        // 校验先于任何磁盘动作，不会留下部分写入。
        assert!(!index_path(&dir).exists());

        let mut duplicate_name = PlanCatalog::empty();
        duplicate_name
            .entries
            .push(entry(&sample_id(1), "same.md", PlanLocation::Drafts, false));
        duplicate_name
            .entries
            .push(entry(&sample_id(2), "same.md", PlanLocation::Plans, false));
        assert!(
            validate_catalog(&duplicate_name)
                .unwrap_err()
                .to_string()
                .contains("duplicate active file name")
        );

        let mut active_identity = PlanCatalog::empty();
        let mut active = entry(&sample_id(3), "active.md", PlanLocation::Drafts, false);
        active.deleted_file_identity = Some(PlanFileIdentity {
            device: 1,
            inode: 2,
        });
        active_identity.entries.push(active);
        assert!(
            validate_catalog(&active_identity)
                .unwrap_err()
                .to_string()
                .contains("deleted-file identity")
        );

        for invalid_id in ["", "zz", "0123"] {
            let mut catalog = PlanCatalog::empty();
            catalog.entries.push(entry(
                invalid_id,
                "valid-name.md",
                PlanLocation::Drafts,
                false,
            ));
            assert!(
                validate_catalog(&catalog)
                    .unwrap_err()
                    .to_string()
                    .contains("invalid id")
            );
        }

        for invalid_name in ["no-extension", "dir/a.md", ".", "..", ".md"] {
            let mut catalog = PlanCatalog::empty();
            catalog.entries.push(entry(
                &sample_id(4),
                invalid_name,
                PlanLocation::Drafts,
                false,
            ));
            assert!(
                validate_catalog(&catalog)
                    .unwrap_err()
                    .to_string()
                    .contains("invalid file name"),
                "{invalid_name}"
            );
        }
    }

    #[test]
    fn tombstone_identity_is_serialized_only_when_present() {
        let mut catalog = PlanCatalog::empty();
        catalog.entries.push(entry(
            &sample_id(1),
            "active.md",
            PlanLocation::Drafts,
            false,
        ));
        let plain = serde_json::to_string(&catalog).unwrap();
        assert!(!plain.contains("deleted_file_identity"));

        let mut tombstone = entry(&sample_id(2), "old.md", PlanLocation::Done, true);
        tombstone.deleted_file_identity = Some(PlanFileIdentity {
            device: 7,
            inode: 9,
        });
        catalog.entries.push(tombstone);
        let json = serde_json::to_string(&catalog).unwrap();
        assert!(json.contains("deleted_file_identity"));

        let parsed: PlanCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, catalog);
        // tombstone 不参与活动身份解析。
        assert!(parsed.active_by_file_name("old.md").is_none());
        assert!(parsed.active_by_id(&sample_id(2)).is_none());
    }

    #[test]
    fn plan_lease_is_exclusive_per_id_and_persists_after_release() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let id = sample_id(1);

        let lease = PlanLease::acquire(&ws, &id).unwrap();
        assert_eq!(lease.id(), id);

        // 其他计划的 lease 不冲突。
        PlanLease::acquire(&ws, &sample_id(2)).unwrap();

        let error = PlanLease::acquire(&ws, &id).unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan is in use; stop the active task before managing it."
        );

        drop(lease);
        // 锁文件常驻，不随正文删除；释放后可重新取得。
        assert!(
            dir.path()
                .join(".pp/plan-locks")
                .join(format!("{id}.lock"))
                .exists()
        );
        PlanLease::acquire(&ws, &id).unwrap();

        assert!(PlanLease::acquire(&ws, "not-hex").is_err());
    }

    #[test]
    fn plan_lease_rejects_symlinked_lock_directory_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let outside = tempfile::tempdir().unwrap();
        let id = sample_id(1);

        fs::create_dir_all(dir.path().join(".pp")).unwrap();
        symlink(outside.path(), dir.path().join(".pp/plan-locks")).unwrap();
        let error = PlanLease::acquire(&ws, &id).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");

        fs::remove_file(dir.path().join(".pp/plan-locks")).unwrap();
        fs::create_dir_all(dir.path().join(".pp/plan-locks")).unwrap();
        symlink(
            outside.path().join("elsewhere.lock"),
            dir.path().join(".pp/plan-locks").join(format!("{id}.lock")),
        )
        .unwrap();
        let error = PlanLease::acquire(&ws, &id).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error}");
    }

    #[test]
    fn catalog_lock_is_exclusive_and_coexists_with_plan_lease() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let lease = PlanLease::acquire(&ws, &sample_id(1)).unwrap();

        let lock = PlanCatalogLock::acquire(&ws).unwrap();
        let error = PlanCatalogLock::acquire(&ws).unwrap_err();
        assert!(error.to_string().contains("Plan index is busy"), "{error}");
        drop(lock);
        PlanCatalogLock::acquire(&ws).unwrap();
        drop(lease);
    }

    #[test]
    fn file_identity_matches_metadata_and_refuses_missing_and_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        let path = dir.path().join(".pp/plans/a.md");
        fs::write(&path, b"# title").unwrap();

        let identity = file_identity(&ws, Path::new(".pp/plans/a.md"))
            .unwrap()
            .unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(identity.device, metadata.dev());
        assert_eq!(identity.inode, metadata.ino());

        assert!(
            file_identity(&ws, Path::new(".pp/plans/missing.md"))
                .unwrap()
                .is_none()
        );

        symlink("a.md", dir.path().join(".pp/plans/link.md")).unwrap();
        assert!(file_identity(&ws, Path::new(".pp/plans/link.md")).is_err());
    }

    #[test]
    fn publish_and_commit_move_update_registration_and_preserve_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let id = sample_id(1);
        fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        fs::write(dir.path().join(".pp/drafts/a.md"), b"body").unwrap();

        // 未登记身份：发布 + 登记到目标位置；提交移除源。
        let warning = publish_plan_move(
            &ws,
            &id,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap();
        assert_eq!(warning, None);
        let snapshot = load_catalog(&ws).unwrap();
        let entry = snapshot.catalog.active_by_id(&id).unwrap();
        assert_eq!(entry.file_name, "a.md");
        assert_eq!(entry.location, PlanLocation::Plans);
        assert_eq!(
            fs::read(dir.path().join(".pp/plans/a.md")).unwrap(),
            b"body"
        );

        commit_plan_move(
            &ws,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap();
        assert!(!dir.path().join(".pp/drafts/a.md").exists());
        assert_eq!(
            fs::read(dir.path().join(".pp/plans/a.md")).unwrap(),
            b"body"
        );

        // 已在目标身份：幂等重入（含同 inode 残留源）。
        fs::hard_link(
            dir.path().join(".pp/plans/a.md"),
            dir.path().join(".pp/drafts/a.md"),
        )
        .unwrap();
        publish_plan_move(
            &ws,
            &id,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap();
        commit_plan_move(
            &ws,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap();
        assert!(!dir.path().join(".pp/drafts/a.md").exists());

        // 登记指向其他身份（既非源也非目标）时 fail-closed；墓碑报已删除。
        let error = publish_plan_move(
            &ws,
            &id,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Done),
            b"body",
        )
        .unwrap_err();
        assert!(error.to_string().contains("registration moved"), "{error}");

        let _lock = PlanCatalogLock::acquire(&ws).unwrap();
        let mut snapshot = load_catalog(&ws).unwrap();
        snapshot.catalog.entries[0].deleted = true;
        save_catalog(&ws, snapshot.raw.as_deref(), &snapshot.catalog).unwrap();
        drop(_lock);
        let error = publish_plan_move(
            &ws,
            &id,
            ("a.md", PlanLocation::Drafts),
            ("a.md", PlanLocation::Done),
            b"body",
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan was deleted; choose another plan from /plan-list."
        );
    }

    #[test]
    fn publish_move_refuses_other_registered_name_and_keeps_source() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        let id = sample_id(1);
        let other = sample_id(2);
        fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        fs::write(dir.path().join(".pp/drafts/mine.md"), b"body").unwrap();

        let _lock = PlanCatalogLock::acquire(&ws).unwrap();
        let snapshot = load_catalog(&ws).unwrap();
        let mut catalog = snapshot.catalog;
        catalog.entries.push(PlanCatalogEntry {
            id: other.clone(),
            file_name: "taken.md".into(),
            location: PlanLocation::Plans,
            deleted: false,
            deleted_file_identity: None,
        });
        save_catalog(&ws, snapshot.raw.as_deref(), &catalog).unwrap();
        drop(_lock);

        let error = publish_plan_move(
            &ws,
            &id,
            ("mine.md", PlanLocation::Drafts),
            ("taken.md", PlanLocation::Plans),
            b"body",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("registered to another plan"),
            "{error}"
        );
        assert!(!dir.path().join(".pp/plans/taken-new.md").exists());
        assert_eq!(
            fs::read(dir.path().join(".pp/drafts/mine.md")).unwrap(),
            b"body"
        );
    }

    #[test]
    fn cleanup_leftover_only_removes_inode_confirmed_links() {
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace(&dir);
        fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        fs::write(dir.path().join(".pp/plans/a.md"), b"body").unwrap();

        // 同 inode 残留被移除；缺失残留静默完成。
        fs::hard_link(
            dir.path().join(".pp/plans/a.md"),
            dir.path().join(".pp/drafts/a.md"),
        )
        .unwrap();
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/drafts/a.md").unwrap();
        assert!(!dir.path().join(".pp/drafts/a.md").exists());
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/drafts/gone.md").unwrap();

        // 不同 inode、符号链接与特殊残留一律保留现状。
        fs::write(dir.path().join(".pp/drafts/other.md"), b"body").unwrap();
        symlink("a.md", dir.path().join(".pp/drafts/alias.md")).unwrap();
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/drafts/other.md").unwrap();
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/drafts/alias.md").unwrap();
        assert_eq!(
            fs::read(dir.path().join(".pp/drafts/other.md")).unwrap(),
            b"body"
        );
        assert!(dir.path().join(".pp/drafts/alias.md").is_symlink());

        // 相同路径与登记缺失均无动作。
        cleanup_confirmed_leftover(&ws, ".pp/plans/a.md", ".pp/plans/a.md").unwrap();
        cleanup_confirmed_leftover(&ws, ".pp/plans/missing.md", ".pp/drafts/other.md").unwrap();
        assert!(dir.path().join(".pp/plans/a.md").exists());
    }
}
