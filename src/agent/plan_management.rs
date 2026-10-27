//! 项目级计划管理：三个生命周期目录的列表、纳管、重命名与确认删除。
//!
//! 所有 I/O 在 worker 侧完成；正文仍是唯一的内容/进度来源，登记表只提供
//! 稳定身份与位置，绝不授予执行权限。UI 只持有 `PlanSelection` 操作令牌，
//! worker 按 ID 重查登记并核对当前路径/精确正文，不按列表下标操作磁盘。

use anyhow::{Context, Result, anyhow, bail, ensure};
use rustix::{fs::Dir, io::Errno};
use std::{ffi::OsString, path::Path, time::SystemTime};

use super::Agent;
use crate::plan::{
    PLAN_MAX_BYTES, PlanLocation, path_for, task_progress, valid_id, validate_file_name,
    validate_stored_file_name,
};
use crate::tools::plan_catalog::{
    self, PlanCatalogEntry, PlanCatalogLock, PlanFileIdentity, PlanLease,
};
use crate::tools::save::Workspace;

/// 列表中的一个计划条目；`error` 非空时禁用进入/重命名/删除。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanListEntry {
    pub id: String,
    pub file_name: String,
    pub location: PlanLocation,
    /// 首个 H1 文本，缺失时为文件名；约束为合法的 1..120 字符显示标题。
    pub title: String,
    pub completed: usize,
    pub total: usize,
    pub modified: SystemTime,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PlanListing {
    pub entries: Vec<PlanListEntry>,
    pub diagnostics: Vec<String>,
}

/// 操作令牌：ID、当时的项目相对路径与精确正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlanSelection {
    pub id: String,
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanOpenIntent {
    ContinueDraft,
    Execute,
}

const DELETED_PLAN: &str = "Plan was deleted; choose another plan from /plan-list.";
const CHANGED: &str = "Plan changed; reopen it from /plan-list.";
const NOT_FOUND: &str = "Plan was not found; refresh the list.";
const NAME_EXISTS: &str = "Plan file name already exists; choose another file_name.";
const REGISTER_CONFLICT: &str =
    "Plan file conflicts with the registered plan of the same name; resolve it in /plan-list.";
const PP_DIR_NAME: &str = ".pp";

/// `verify_selection` 的核验结果：登记表解析出的稳定身份、canonical 路径与
/// 已核对字节的正文，以及调用方须持有到操作结束的计划 lease（Drop 即释放，
/// 不存回 `Agent::plan_lease`）。
pub(crate) struct VerifiedSelection {
    pub(crate) id: String,
    pub(crate) file_name: String,
    pub(crate) location: PlanLocation,
    /// 登记表解析出的 canonical 项目相对路径，已核对与 `selection.path` 一致。
    pub(crate) path: String,
    /// 当前磁盘上的精确字节（已验证与 `selection.content` 一致）。
    pub(crate) content: String,
    pub(crate) _lease: PlanLease,
}

/// 目录扫描到的单个合法 `.md` 直接子项；stat 失败（符号链接、特殊文件、
/// 身份读取错误）保留为错误，由列表降级为禁用操作的条目。
struct ScannedFile {
    file_name: String,
    location: PlanLocation,
    stat: Result<(PlanFileIdentity, SystemTime)>,
}

impl Agent {
    /// 项目计划列表：只扫描三个生命周期目录的直接 `.md` 普通文件，缺失
    /// 目录为空，不递归、不跨项目。正文仍是唯一内容/进度来源；列表同时
    /// 完成未登记文件的纳管（仅取登记表锁，不加计划 lease）。CAS 冲突
    /// 向上报告，不自动重试或覆盖。
    pub(crate) fn list_plans(&mut self) -> Result<PlanListing> {
        let workspace = self.tools.plan_workspace();
        let mut diagnostics: Vec<String> = Vec::new();
        let mut scanned: Vec<ScannedFile> = Vec::new();
        for location in [
            PlanLocation::Drafts,
            PlanLocation::Plans,
            PlanLocation::Done,
        ] {
            match scan_location(workspace, location, &mut diagnostics) {
                Ok(files) => scanned.extend(files),
                Err(error) => diagnostics.push(format!(
                    "Could not scan the {} directory: {error:#}",
                    location.dir_name()
                )),
            }
        }
        let mut entries: Vec<PlanListEntry> = Vec::new();
        {
            let _lock = PlanCatalogLock::acquire(workspace)?;
            let snapshot = plan_catalog::load_catalog(workspace)?;
            let mut catalog = snapshot.catalog;
            let by_path: std::collections::HashMap<String, &ScannedFile> = scanned
                .iter()
                .map(|file| (path_for(&file.file_name, file.location), file))
                .collect();

            // 登记条目：登记位置存在文件时按扫描展示；缺失时错误条目。
            for registered in catalog.entries.iter().filter(|entry| !entry.deleted) {
                let path = path_for(&registered.file_name, registered.location);
                entries.push(match by_path.get(&path) {
                    Some(file) => entry_from_scan(&registered.id, file, workspace),
                    None => PlanListEntry {
                        id: registered.id.clone(),
                        file_name: registered.file_name.clone(),
                        location: registered.location,
                        title: plan_display_title("", &registered.file_name),
                        completed: 0,
                        total: 0,
                        modified: SystemTime::UNIX_EPOCH,
                        error: Some("Plan file is missing at its registered path.".to_owned()),
                    },
                });
            }

            struct Adoption<'a> {
                file: &'a ScannedFile,
                entry: PlanCatalogEntry,
            }
            let mut adoptions: Vec<Adoption<'_>> = Vec::new();
            for file in &scanned {
                let Some(active) = catalog.active_by_file_name(&file.file_name) else {
                    // 无法安全 stat 的未登记文件：错误条目，禁用操作，不纳管。
                    if file.stat.is_err() {
                        entries.push(entry_from_scan("", file, workspace));
                        continue;
                    }
                    // 删除残留：tombstone 同路径且同 inode，保留旧 ID 供显式重试。
                    let identity = file.stat.as_ref().ok().map(|(identity, _)| *identity);
                    if let Some(tombstone) = identity.and_then(|identity| {
                        catalog.entries.iter().find(|entry| {
                            entry.deleted
                                && entry.file_name == file.file_name
                                && entry.location == file.location
                                && entry.deleted_file_identity == Some(identity)
                        })
                    }) {
                        entries.push(entry_from_scan(&tombstone.id, file, workspace));
                        continue;
                    }
                    // 同一刷新内已纳管同名文件：跨目录同名按冲突展示。
                    if adoptions
                        .iter()
                        .any(|adoption| adoption.entry.file_name == file.file_name)
                    {
                        let mut entry = entry_from_scan("", file, workspace);
                        entry.error = Some(REGISTER_CONFLICT.to_owned());
                        entries.push(entry);
                        diagnostics.push(format!(
                            "Plan file {} conflicts with another unregistered plan of the same name.",
                            path_for(&file.file_name, file.location)
                        ));
                        continue;
                    }
                    // 新纳管：原生 32hex 文件名在无任何同 ID 记录（含
                    // tombstone）时沿用该 ID；其余生成新 ID。tombstone 路径
                    // 上的新文件按新 ID 纳管，旧 ID 不复活。
                    let stem = file
                        .file_name
                        .strip_suffix(".md")
                        .unwrap_or(&file.file_name);
                    let id = if valid_id(stem)
                        && !catalog.entries.iter().any(|entry| entry.id == stem)
                    {
                        stem.to_owned()
                    } else {
                        loop {
                            let candidate = generate_id()?;
                            if !catalog.entries.iter().any(|entry| entry.id == candidate)
                                && !adoptions
                                    .iter()
                                    .any(|adoption| adoption.entry.id == candidate)
                            {
                                break candidate;
                            }
                        }
                    };
                    adoptions.push(Adoption {
                        file,
                        entry: PlanCatalogEntry {
                            id,
                            file_name: file.file_name.clone(),
                            location: file.location,
                            deleted: false,
                            deleted_file_identity: None,
                        },
                    });
                    continue;
                };
                if active.location == file.location {
                    // 已按登记条目展示。
                    continue;
                }
                // 同 ID 多路径：同 inode 的交接残留只展示登记位置一条；
                // 不同 inode 的同名文件是冲突，错误条目并禁用操作。
                let registered_path = path_for(&active.file_name, active.location);
                let stray = file.stat.as_ref().ok().map(|(identity, _)| *identity);
                let registered_identity =
                    plan_catalog::file_identity(workspace, Path::new(&registered_path))
                        .ok()
                        .flatten();
                if stray.is_some() && stray == registered_identity {
                    continue;
                }
                let mut entry = entry_from_scan("", file, workspace);
                entry.error = Some(REGISTER_CONFLICT.to_owned());
                entries.push(entry);
                diagnostics.push(format!(
                    "Plan file {} conflicts with the registered plan of the same name.",
                    path_for(&file.file_name, file.location)
                ));
            }
            if !adoptions.is_empty() {
                for adoption in &adoptions {
                    entries.push(entry_from_scan(
                        &adoption.entry.id,
                        adoption.file,
                        workspace,
                    ));
                }
                catalog
                    .entries
                    .extend(adoptions.into_iter().map(|adoption| adoption.entry));
                plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog)?;
            }
        }
        // modified 降序、file_name 升序；缺失文件的条目按最早时间沉底。
        entries.sort_by(|left, right| {
            right
                .modified
                .cmp(&left.modified)
                .then_with(|| left.file_name.cmp(&right.file_name))
        });
        Ok(PlanListing {
            entries,
            diagnostics,
        })
    }

    /// 安全读取列表选中的计划正文，作为后续操作的 selection 令牌；只读，
    /// 不发审批 token，不进入模型历史。
    pub(crate) fn read_listed_plan(&mut self, id: &str) -> Result<PlanSelection> {
        ensure!(valid_id(id), "Invalid plan id.");
        let workspace = self.tools.plan_workspace();
        let _lock = PlanCatalogLock::acquire(workspace)?;
        let snapshot = plan_catalog::load_catalog(workspace)?;
        let entry = snapshot
            .catalog
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .ok_or_else(|| anyhow!("{NOT_FOUND}"))?;
        let path = path_for(&entry.file_name, entry.location);
        if entry.deleted {
            // 只允许读取同 inode 的删除残留，供再次确认删除；进入/重命名
            // 仍经 resolve_active 拒绝墓碑，不恢复旧身份或执行权限。
            let expected = entry
                .deleted_file_identity
                .ok_or_else(|| anyhow!("{DELETED_PLAN}"))?;
            ensure!(
                plan_catalog::file_identity(workspace, Path::new(&path))? == Some(expected),
                "{DELETED_PLAN}"
            );
        }
        let content = read_plan_bytes(workspace, Path::new(&path))?;
        Ok(PlanSelection {
            id: id.to_owned(),
            path,
            content,
        })
    }

    /// 操作令牌核验：先非阻塞取得计划 lease（占用即 busy），再在登记表锁内
    /// 按 ID 解析 canonical 名称/位置（墓碑报已删除），最后安全读取并核对
    /// 精确字节。锁顺序固定 lease → 登记表锁；`publish_plan_move` 等登记
    /// 事务由调用方持有的 lease 保证互斥。不触碰 workflow/journal。
    pub(crate) fn verify_selection(
        &mut self,
        selection: &PlanSelection,
    ) -> Result<VerifiedSelection> {
        ensure!(valid_id(&selection.id), "Invalid plan selection.");
        let workspace = self.tools.plan_workspace();
        let lease = PlanLease::acquire(workspace, &selection.id)?;
        let (file_name, location) = resolve_active(workspace, &selection.id)?;
        let path = path_for(&file_name, location);
        ensure!(path == selection.path, "{CHANGED}");
        let content = read_plan_bytes(workspace, Path::new(&path))?;
        ensure!(content == selection.content, "{CHANGED}");
        Ok(VerifiedSelection {
            id: selection.id.clone(),
            file_name,
            location,
            path,
            content,
            _lease: lease,
        })
    }

    /// 重命名：核验 selection 后检查新名（严格 kebab 规则、三个目录磁盘
    /// 重名与登记表活动重名），同名直接成功；否则 link→CAS 更新登记→同步
    /// 当前绑定→清理旧名链接。只改文件名，不改 H1/正文/checkbox/ID/正文
    /// revision；旧 path token 因路径变化自然失效。
    pub(crate) fn rename_listed_plan(
        &mut self,
        selection: PlanSelection,
        file_name: String,
    ) -> Result<PlanListing> {
        self.journal.check()?;
        let verified = self.verify_selection(&selection)?;
        if file_name == verified.file_name {
            return self.list_plans();
        }
        validate_file_name(&file_name)?;
        let workspace = self.tools.plan_workspace();
        let verified_identity = plan_catalog::file_identity(workspace, Path::new(&verified.path))?
            .context("Plan file vanished; refresh the list.")?;
        {
            let _lock = PlanCatalogLock::acquire(workspace)?;
            let snapshot = plan_catalog::load_catalog(workspace)?;
            ensure!(
                snapshot.catalog.active_by_file_name(&file_name).is_none(),
                "{NAME_EXISTS}"
            );
            // 三个生命周期目录的磁盘重名预检；缺失目录视为无重名，预检后
            // 的并发碰撞由 `link_plan` 的同 inode 确认兜底，绝不覆盖。
            for location in [
                PlanLocation::Drafts,
                PlanLocation::Plans,
                PlanLocation::Done,
            ] {
                let candidate = path_for(&file_name, location);
                match plan_catalog::file_identity(workspace, Path::new(&candidate)) {
                    Ok(Some(identity)) => {
                        ensure!(identity == verified_identity, "{NAME_EXISTS}");
                    }
                    Ok(None) => {}
                    Err(error) if is_missing(&error) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        // 发布新名链接并 CAS 更新登记；失败保留源名，返回真实错误。
        plan_catalog::publish_plan_move(
            workspace,
            &verified.id,
            (verified.file_name.as_str(), verified.location),
            (file_name.as_str(), verified.location),
            verified.content.as_bytes(),
        )?;
        // 同步当前绑定：仅文件名；登记已提交，journal 失败返回真实错误。
        // 先结束上面的 workspace 借用再改 workflow，避免借用冲突。
        if self
            .workflow
            .plan
            .as_ref()
            .is_some_and(|document| document.id == verified.id)
        {
            let previous = self.workflow.clone();
            let document = self.workflow.plan.as_mut().expect("bound plan");
            document.file_name = Some(file_name.clone());
            if let Err(error) = self.journal.write_workflow(&self.workflow) {
                self.workflow = previous;
                return Err(error);
            }
        }
        // 清理旧名链接；失败交给按 ID 协调的同 inode 残留清理。
        let workspace = self.tools.plan_workspace();
        if let Err(error) = plan_catalog::commit_plan_move(
            workspace,
            (verified.file_name.as_str(), verified.location),
            (file_name.as_str(), verified.location),
            verified.content.as_bytes(),
        ) {
            tracing::warn!(event = "plan_rename_cleanup_failed", error = %error);
        }
        self.list_plans()
    }

    /// 确认删除：核验 selection 后先以 CAS 提交记录删除身份的 tombstone，
    /// 再仅删除选中普通文件及同 ID 同 inode 的生命周期残留链接；登记失败
    /// 或正文不匹配不 unlink。删除当前绑定只解除绑定并保留模式；其他会话
    /// 随后按 ID 得到 tombstone 错误，不从快照补回。已提交 tombstone 后的
    /// unlink 失败报 `Delete incomplete`，可显式重试且残留不被纳管成新 ID。
    pub(crate) fn delete_listed_plan(&mut self, selection: PlanSelection) -> Result<PlanListing> {
        self.journal.check()?;
        ensure!(valid_id(&selection.id), "Invalid plan selection.");
        let workspace = self.tools.plan_workspace();
        let lease = PlanLease::acquire(workspace, &selection.id)?;
        let (identity, primary) = {
            let _lock = PlanCatalogLock::acquire(workspace)?;
            let snapshot = plan_catalog::load_catalog(workspace)?;
            if let Some(entry) = snapshot
                .catalog
                .entries
                .iter()
                .find(|entry| entry.id == selection.id && !entry.deleted)
            {
                let path = path_for(&entry.file_name, entry.location);
                ensure!(path == selection.path, "{CHANGED}");
                let target = workspace.target(Path::new(&path), false)?;
                let bytes = target.read_bounded(PLAN_MAX_BYTES)?;
                ensure!(bytes.len() <= PLAN_MAX_BYTES, "Plan file is too large.");
                let content = std::str::from_utf8(&bytes)
                    .map_err(|_| anyhow!("Plan file is not valid UTF-8."))?;
                ensure!(content == selection.content, "{CHANGED}");
                // CAS：先提交墓碑（记录删除身份），再 unlink；登记失败不删文件。
                let identity = target
                    .stat_optional()?
                    .context("Plan file vanished; refresh the list.")
                    .map(|(device, inode)| PlanFileIdentity { device, inode })?;
                let mut catalog = snapshot.catalog;
                let entry = catalog
                    .entries
                    .iter_mut()
                    .find(|entry| entry.id == selection.id && !entry.deleted)
                    .expect("active entry still present");
                entry.deleted = true;
                entry.deleted_file_identity = Some(identity);
                plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog)?;
                (identity, path)
            } else if let Some(entry) = snapshot
                .catalog
                .entries
                .iter()
                .find(|entry| entry.id == selection.id && entry.deleted)
            {
                // 删除残留重试：仅当路径上的文件身份与墓碑一致。
                let identity = entry
                    .deleted_file_identity
                    .ok_or_else(|| anyhow!("{DELETED_PLAN}"))?;
                let path = path_for(&entry.file_name, entry.location);
                ensure!(path == selection.path, "{CHANGED}");
                match workspace
                    .target(Path::new(&path), false)?
                    .stat_optional()?
                    .map(|(device, inode)| PlanFileIdentity { device, inode })
                {
                    Some(current) if current == identity => {
                        let content = read_plan_bytes(workspace, Path::new(&path))?;
                        ensure!(content == selection.content, "{CHANGED}");
                        (identity, path)
                    }
                    _ => bail!("{DELETED_PLAN}"),
                }
            } else {
                bail!("{NOT_FOUND}");
            }
        };
        let mut failures: Vec<String> = Vec::new();
        remove_matching(workspace, Path::new(&primary), identity, &mut failures);
        // 同 inode 的生命周期残留链接：只清理与被删文件确认同 inode 的名字。
        for location in [
            PlanLocation::Drafts,
            PlanLocation::Plans,
            PlanLocation::Done,
        ] {
            let names = stored_names(workspace, location).unwrap_or_default();
            for name in names {
                let path = path_for(&name, location);
                if path == primary {
                    continue;
                }
                if let Ok(Some(current)) = plan_catalog::file_identity(workspace, Path::new(&path))
                    && current == identity
                {
                    remove_matching(workspace, Path::new(&path), identity, &mut failures);
                }
            }
        }
        ensure!(
            failures.is_empty(),
            "Delete incomplete; retry deleting this plan from /plan-list."
        );
        // 删除当前绑定：只解除 plan/review 绑定并保留模式；旧会话按 ID
        // 报已删除，不从快照补回。
        if self
            .plan_lease
            .as_ref()
            .is_some_and(|lease| lease.id() == selection.id)
        {
            self.plan_lease = None;
        }
        if self
            .workflow
            .plan
            .as_ref()
            .is_some_and(|document| document.id == selection.id)
        {
            let previous = self.workflow.clone();
            self.workflow.plan = None;
            self.workflow.review = None;
            if let Err(error) = self.journal.write_workflow(&self.workflow) {
                self.workflow = previous;
                return Err(error);
            }
        }
        drop(lease);
        self.list_plans()
    }
}

/// 按 ID 解析登记表中的活动条目；墓碑与缺失分别报错，绝不按猜测路径操作。
fn resolve_active(workspace: &Workspace, id: &str) -> Result<(String, PlanLocation)> {
    let _lock = PlanCatalogLock::acquire(workspace)?;
    let snapshot = plan_catalog::load_catalog(workspace)?;
    if snapshot
        .catalog
        .entries
        .iter()
        .any(|entry| entry.deleted && entry.id == id)
    {
        bail!("{DELETED_PLAN}");
    }
    let entry = snapshot
        .catalog
        .active_by_id(id)
        .ok_or_else(|| anyhow!("{NOT_FOUND}"))?;
    Ok((entry.file_name.clone(), entry.location))
}

/// NOFOLLOW 目录 fd 扫描单个生命周期目录的直接 `.md` 子项；缺失目录为空，
/// 不跟随符号链接、不递归。非 UTF-8 名称与非法计划文件名写入 diagnostics，
/// 不进入列表；每个合法名称保留一次 stat（符号链接/特殊文件在 stat 报错）。
fn scan_location(
    workspace: &Workspace,
    location: PlanLocation,
    diagnostics: &mut Vec<String>,
) -> Result<Vec<ScannedFile>> {
    let dir = match workspace.directory(
        &[
            OsString::from(PP_DIR_NAME),
            OsString::from(location.dir_name()),
        ],
        false,
    ) {
        Ok(dir) => dir,
        Err(error) if is_missing(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for entry in Dir::new(dir)
        .map_err(std::io::Error::from)
        .context("opening plan directory")?
    {
        let entry = entry
            .map_err(std::io::Error::from)
            .context("reading plan directory entries")?;
        let raw = entry.file_name();
        if raw.to_bytes() == b"." || raw.to_bytes() == b".." {
            continue;
        }
        let Ok(name) = raw.to_str() else {
            diagnostics.push(format!(
                "Skipped a non-UTF-8 file name in the {} directory.",
                location.dir_name()
            ));
            continue;
        };
        if !name.ends_with(".md") || validate_stored_file_name(name).is_err() {
            diagnostics.push(format!(
                "Skipped {}/{}: not a valid plan file name.",
                location.dir_name(),
                name
            ));
            continue;
        }
        let stat = match workspace
            .target(Path::new(&path_for(name, location)), false)
            .and_then(|target| target.stat_optional_detailed())
        {
            Ok(Some((device, inode, modified))) => {
                Ok((PlanFileIdentity { device, inode }, modified))
            }
            Ok(None) => Err(anyhow!(
                "Plan file vanished while listing; refresh the list."
            )),
            Err(error) => Err(error),
        };
        files.push(ScannedFile {
            file_name: name.to_owned(),
            location,
            stat,
        });
    }
    Ok(files)
}

/// 目录直接子项中通过 stored 命名规则的 `.md` 名称；供删除残留清扫使用，
/// 缺失目录为空。
fn stored_names(workspace: &Workspace, location: PlanLocation) -> Result<Vec<String>> {
    let dir = match workspace.directory(
        &[
            OsString::from(PP_DIR_NAME),
            OsString::from(location.dir_name()),
        ],
        false,
    ) {
        Ok(dir) => dir,
        Err(error) if is_missing(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut names = Vec::new();
    for entry in Dir::new(dir)
        .map_err(std::io::Error::from)
        .context("opening plan directory")?
    {
        let entry = entry
            .map_err(std::io::Error::from)
            .context("reading plan directory entries")?;
        let raw = entry.file_name();
        if raw.to_bytes() == b"." || raw.to_bytes() == b".." {
            continue;
        }
        let Ok(name) = raw.to_str() else {
            continue;
        };
        if validate_stored_file_name(name).is_ok() {
            names.push(name.to_owned());
        }
    }
    Ok(names)
}

/// 身份确认后移除单个文件；缺失视为已完成，不同 inode 绝不动，其他失败
/// 记入 failures 由调用方报 `Delete incomplete`。
fn remove_matching(
    workspace: &Workspace,
    path: &Path,
    identity: PlanFileIdentity,
    failures: &mut Vec<String>,
) {
    let outcome = (|| -> Result<()> {
        let target = workspace.target(path, false)?;
        match target
            .stat_optional()?
            .map(|(device, inode)| PlanFileIdentity { device, inode })
        {
            Some(current) if current == identity => target.unlink(),
            Some(_) => Ok(()),
            None => Ok(()),
        }
    })();
    if let Err(error) = outcome {
        failures.push(format!("{}: {error:#}", path.display()));
    }
}

/// NOFOLLOW 安全读取单个计划正文；上限 `PLAN_MAX_BYTES`，要求 UTF-8。
fn read_plan_bytes(workspace: &Workspace, path: &Path) -> Result<String> {
    let target = workspace.target(path, false)?;
    let bytes = target.read_bounded(PLAN_MAX_BYTES)?;
    ensure!(bytes.len() <= PLAN_MAX_BYTES, "Plan file is too large.");
    let content =
        std::str::from_utf8(&bytes).map_err(|_| anyhow!("Plan file is not valid UTF-8."))?;
    Ok(content.to_owned())
}

/// 由扫描结果构建列表条目；正文/身份读取失败降级为禁用操作的错误条目，
/// 保留文件名与目录分类，不虚构进度或标题。
fn entry_from_scan(id: &str, file: &ScannedFile, workspace: &Workspace) -> PlanListEntry {
    let build = |title: String,
                 completed: usize,
                 total: usize,
                 modified: SystemTime,
                 error: Option<String>| PlanListEntry {
        id: id.to_owned(),
        file_name: file.file_name.clone(),
        location: file.location,
        title,
        completed,
        total,
        modified,
        error,
    };
    let modified = match &file.stat {
        Ok((_, modified)) => *modified,
        Err(error) => {
            return build(
                plan_display_title("", &file.file_name),
                0,
                0,
                SystemTime::UNIX_EPOCH,
                Some(format!("{error:#}")),
            );
        }
    };
    let relative = path_for(&file.file_name, file.location);
    let path = Path::new(&relative);
    match read_plan_bytes(workspace, path) {
        Ok(content) => {
            let (completed, total) = task_progress(&content);
            build(
                plan_display_title(&content, &file.file_name),
                completed,
                total,
                modified,
                None,
            )
        }
        Err(error) => build(
            plan_display_title("", &file.file_name),
            0,
            0,
            modified,
            Some(format!("{error:#}")),
        ),
    }
}

/// 首个 H1 文本；缺失/空白时回退文件名（去掉 `.md` 后缀）。显示标题修剪
/// 后约束在 1..=120 字符，不改正文。
pub(crate) fn plan_display_title(content: &str, file_name: &str) -> String {
    let heading = first_h1(content)
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    let fallback = file_name.strip_suffix(".md").unwrap_or(file_name);
    let title = heading.unwrap_or_else(|| fallback.trim().to_owned());
    if title.is_empty() {
        return fallback.chars().take(120).collect();
    }
    title.chars().take(120).collect()
}

/// 提取首个一级标题的纯文本（Text/Code 事件拼接）；没有则返回 `None`。
fn first_h1(content: &str) -> Option<String> {
    use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
    let mut inside = false;
    let mut text = String::new();
    for event in Parser::new_ext(content, Options::empty()) {
        match event {
            Event::Start(Tag::Heading {
                level: HeadingLevel::H1,
                ..
            }) => inside = true,
            Event::End(TagEnd::Heading(HeadingLevel::H1)) if inside => return Some(text),
            Event::Text(value) | Event::Code(value) if inside => text.push_str(&value),
            _ => {}
        }
    }
    None
}

fn generate_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| anyhow!("Could not allocate a plan ID."))?;
    Ok(format!("{:032x}", u128::from_be_bytes(bytes)))
}

/// 判断错误链中是否只是“路径不存在”；生命周期目录缺失表示该分类为空。
fn is_missing(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<Errno>() == Some(&Errno::NOENT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::tests::{err_message, seed_session};
    use crate::config::{EditMode, ModelCatalog};
    use crate::plan::{PlanDocument, PlanStatus, WorkMode, WorkflowState};
    use crate::tools::Tools;
    use serde_json::json;
    use std::time::Duration;

    /// An agent journaled into the fixture's global sessions directory, as
    /// production does, with its own working directory.
    fn fixture_agent(cwd: &std::path::Path, sessions: &std::path::Path) -> Agent {
        std::fs::create_dir_all(cwd).unwrap();
        let global = sessions.parent().unwrap();
        // 生产会在启动时建立全局 sessions 目录；fixture 先建好再写配置。
        std::fs::create_dir_all(sessions).unwrap();
        let settings = global.join("settings.yml");
        let models = global.join("models.yml");
        std::fs::write(&settings, "default_model: fixture/one\n").unwrap();
        std::fs::write(
            &models,
            json!({"providers":{"fixture": { "api_key":"fixture-key", "protocol":"completion",
                "models":{"one": {"model":"one"}}}}})
            .to_string(),
        )
        .unwrap();
        Agent::new(
            ModelCatalog::from_files(&settings, &models).unwrap(),
            Tools::new(cwd.to_owned(), EditMode::Replace).unwrap(),
            sessions.to_owned(),
        )
        .unwrap()
    }

    fn write_plan(dir: &std::path::Path, location: PlanLocation, name: &str, content: &str) {
        let path = dir.join(".pp").join(location.dir_name()).join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn entry<'a>(listing: &'a PlanListing, file_name: &str) -> &'a PlanListEntry {
        listing
            .entries
            .iter()
            .find(|entry| entry.file_name == file_name)
            .unwrap()
    }

    fn registered(agent: &Agent, file_name: &str) -> crate::tools::plan_catalog::PlanCatalogEntry {
        let workspace = agent.tools.plan_workspace();
        let _lock = PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = plan_catalog::load_catalog(workspace).unwrap();
        snapshot
            .catalog
            .entries
            .iter()
            .find(|entry| !entry.deleted && entry.file_name == file_name)
            .cloned()
            .unwrap()
    }

    fn bound_workflow(id: &str, file_name: &str, content: &str) -> WorkflowState {
        let mut document = PlanDocument::new().unwrap();
        document.id = id.to_owned();
        document.revision = 1;
        document.title = plan_display_title(content, file_name);
        document.content = content.to_owned();
        document.status = PlanStatus::Draft;
        document.origin_prompt = "list fixture".to_owned();
        document.file_name = Some(file_name.to_owned());
        document.location = PlanLocation::Drafts;
        WorkflowState {
            mode: WorkMode::Plan,
            plan: Some(document),
            review: None,
        }
    }

    #[test]
    fn list_scans_classifies_and_adopts_unregistered_files() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        write_plan(
            &root,
            PlanLocation::Drafts,
            "fix-config-loading.md",
            "# Fix config\n\n- [x] done task\n- [ ] pending task\n",
        );
        // 原生 32hex 文件名沿用该 ID。
        let native_id = "0123456789abcdef0123456789abcdef";
        write_plan(
            &root,
            PlanLocation::Plans,
            &format!("{native_id}.md"),
            "# Native\n\n- [ ] task\n",
        );
        // 旧 Unicode 名称保留并按新 ID 纳管。
        write_plan(&root, PlanLocation::Done, "旧计划.md", "# 旧计划\n");
        // 非法名称、非计划文件、目录与围栏示例不进入列表。
        // stored 规则非法的名称（含反斜杠）写入诊断，不进入列表。
        write_plan(&root, PlanLocation::Drafts, "back\\slash.md", "# bad\n");
        write_plan(&root, PlanLocation::Drafts, "notes.txt", "hello");
        std::fs::create_dir_all(root.join(".pp/drafts/nested.md")).unwrap();
        std::fs::write(root.join(".pp/drafts/nested.md/inner.md"), "# inner\n").unwrap();
        // 符号链接：错误条目、禁用操作、不跟随。
        std::os::unix::fs::symlink(
            root.join(".pp/drafts/fix-config-loading.md"),
            root.join(".pp/drafts/link.md"),
        )
        .unwrap();
        // 非 UTF-8 名称：仅诊断。macOS 文件系统拒绝非法 UTF-8 名称
        // （EILSEQ），此时跳过该场景。
        use std::os::unix::ffi::OsStrExt;
        let weird = std::ffi::OsStr::from_bytes(b"bad\xff.md");
        let weird_written =
            std::fs::write(root.join(".pp/drafts").join(weird), "# weird\n").is_ok();

        let listing = agent.list_plans().unwrap();
        let named = entry(&listing, "fix-config-loading.md");
        assert_eq!(named.title, "Fix config");
        assert_eq!(named.location, PlanLocation::Drafts);
        assert_eq!(named.completed, 1);
        assert_eq!(named.total, 2);
        assert!(named.error.is_none());

        let native = entry(&listing, &format!("{native_id}.md"));
        assert_eq!(native.id, native_id);
        assert_eq!(native.location, PlanLocation::Plans);

        let unicode = entry(&listing, "旧计划.md");
        assert!(valid_id(&unicode.id) && unicode.id != native_id);
        assert_eq!(unicode.location, PlanLocation::Done);
        assert_eq!(unicode.title, "旧计划");

        let link = entry(&listing, "link.md");
        assert_eq!(link.id, "");
        assert!(link.error.is_some());

        assert!(
            listing
                .diagnostics
                .iter()
                .any(|line| line.contains("back\\slash.md"))
        );
        if weird_written {
            assert!(
                listing
                    .diagnostics
                    .iter()
                    .any(|line| line.contains("non-UTF-8"))
            );
        }
        assert!(
            listing
                .entries
                .iter()
                .all(|entry| entry.file_name != "inner.md")
        );
        assert!(
            listing
                .entries
                .iter()
                .all(|entry| entry.file_name != "notes.txt")
        );

        // 纳管已持久化；重复刷新幂等且不产生重复 ID/条目。
        assert_eq!(registered(&agent, "fix-config-loading.md").id, named.id);
        let again = agent.list_plans().unwrap();
        assert_eq!(again.entries.len(), listing.entries.len());
        assert_eq!(entry(&again, "fix-config-loading.md").id, named.id);
    }

    #[test]
    fn list_reports_missing_registered_files_and_name_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        write_plan(
            &root,
            PlanLocation::Drafts,
            "registered-plan.md",
            "# Registered\n\n- [ ] task\n",
        );
        let listing = agent.list_plans().unwrap();
        let id = entry(&listing, "registered-plan.md").id.clone();

        // 登记文件消失：错误条目，不是空表假象。
        std::fs::remove_file(root.join(".pp/drafts/registered-plan.md")).unwrap();
        let listing = agent.list_plans().unwrap();
        let missing = entry(&listing, "registered-plan.md");
        assert_eq!(missing.id, id);
        assert!(
            missing
                .error
                .as_deref()
                .unwrap()
                .contains("missing at its registered path")
        );

        // 登记名称在另一目录出现不同 inode 的同名文件：冲突条目。
        write_plan(
            &root,
            PlanLocation::Plans,
            "registered-plan.md",
            "# Stray\n",
        );
        let listing = agent.list_plans().unwrap();
        let stray = listing
            .entries
            .iter()
            .find(|entry| entry.error.as_deref() == Some(REGISTER_CONFLICT))
            .unwrap();
        assert_eq!(stray.id, "");
        assert_eq!(stray.file_name, "registered-plan.md");

        // 同 inode 的交接残留：只展示登记位置一条，不重复计数。
        std::fs::remove_file(root.join(".pp/plans/registered-plan.md")).unwrap();
        write_plan(
            &root,
            PlanLocation::Drafts,
            "registered-plan.md",
            "# Registered\n\n- [ ] task\n",
        );
        std::fs::create_dir_all(root.join(".pp/plans")).unwrap();
        std::fs::hard_link(
            root.join(".pp/drafts/registered-plan.md"),
            root.join(".pp/plans/registered-plan.md"),
        )
        .unwrap();
        let listing = agent.list_plans().unwrap();
        let same = listing
            .entries
            .iter()
            .filter(|entry| entry.file_name == "registered-plan.md")
            .count();
        assert_eq!(same, 1);
        assert_eq!(entry(&listing, "registered-plan.md").id, id);
        assert!(entry(&listing, "registered-plan.md").error.is_none());
    }

    #[test]
    fn list_keeps_tombstone_leftover_and_adopts_new_file_as_new_id() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        write_plan(&root, PlanLocation::Drafts, "gone-plan.md", "# Gone\n");
        let listing = agent.list_plans().unwrap();
        let id = entry(&listing, "gone-plan.md").id.clone();

        // 手工提交墓碑但不 unlink：列表保留旧 ID 的条目供显式重试删除。
        let workspace = agent.tools.plan_workspace();
        let identity = {
            let _lock = PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let mut catalog = snapshot.catalog;
            let entry = catalog
                .entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .unwrap();
            entry.deleted = true;
            let identity = plan_catalog::file_identity(
                workspace,
                Path::new(&path_for("gone-plan.md", PlanLocation::Drafts)),
            )
            .unwrap()
            .unwrap();
            entry.deleted_file_identity = Some(identity);
            plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
            identity
        };
        let listing = agent.list_plans().unwrap();
        let leftover = entry(&listing, "gone-plan.md");
        assert_eq!(leftover.id, id);
        assert!(leftover.error.is_none());

        // 同 inode 的残留不被纳管成新 ID；真正后来另建的不同 inode 文件
        // 得到新 ID。
        let listing = agent.list_plans().unwrap();
        assert_eq!(entry(&listing, "gone-plan.md").id, id);
        std::fs::remove_file(root.join(".pp/drafts/gone-plan.md")).unwrap();
        write_plan(&root, PlanLocation::Drafts, "gone-plan.md", "# Reborn\n");
        let listing = agent.list_plans().unwrap();
        let reborn = entry(&listing, "gone-plan.md");
        assert_ne!(reborn.id, id);
        assert!(valid_id(&reborn.id));
        let workspace = agent.tools.plan_workspace();
        assert_ne!(
            plan_catalog::file_identity(
                workspace,
                Path::new(&path_for("gone-plan.md", PlanLocation::Drafts))
            )
            .unwrap()
            .unwrap(),
            identity
        );
    }

    #[test]
    fn read_and_verify_selection_reject_stale_bytes_and_tombstones() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        write_plan(
            &root,
            PlanLocation::Drafts,
            "verify-plan.md",
            "# Verify\n\n- [ ] task\n",
        );
        let listing = agent.list_plans().unwrap();
        let id = entry(&listing, "verify-plan.md").id.clone();

        let selection = agent.read_listed_plan(&id).unwrap();
        assert_eq!(selection.path, ".pp/drafts/verify-plan.md");
        assert!(selection.content.contains("# Verify"));
        assert!(agent.verify_selection(&selection).is_ok());

        // 精确字节不匹配：拒绝操作，不沿用旧正文。
        let mut stale = selection.clone();
        stale.content = "# Stale\n".to_owned();
        assert!(err_message(agent.verify_selection(&stale)).contains("Plan changed"));

        // 墓碑：verify/read 都拒绝，绝不复活。
        let workspace = agent.tools.plan_workspace();
        {
            let _lock = PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let mut catalog = snapshot.catalog;
            let entry = catalog
                .entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .unwrap();
            entry.deleted = true;
            entry.deleted_file_identity = Some(
                plan_catalog::file_identity(workspace, Path::new(&selection.path))
                    .unwrap()
                    .unwrap(),
            );
            plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
        }
        assert_eq!(agent.read_listed_plan(&id).unwrap(), selection);
        assert!(err_message(agent.verify_selection(&selection)).contains("Plan was deleted"));
        std::fs::remove_file(root.join(&selection.path)).unwrap();
        assert!(err_message(agent.read_listed_plan(&id)).contains("Plan was deleted"));
    }

    #[test]
    fn rename_updates_catalog_binding_and_keeps_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        let content = "# Rename\n\n- [ ] task\n";
        write_plan(&root, PlanLocation::Drafts, "rename-me.md", content);
        let listing = agent.list_plans().unwrap();
        let old = entry(&listing, "rename-me.md").clone();
        agent.workflow = bound_workflow(&old.id, "rename-me.md", content);
        seed_session(&mut agent, "rename fixture");

        let selection = agent.read_listed_plan(&old.id).unwrap();

        // 非法名称被拒绝，原文件不变。
        let error =
            err_message(agent.rename_listed_plan(selection.clone(), "Bad Name.md".to_owned()));
        assert!(error.contains("kebab-case"));
        assert!(root.join(".pp/drafts/rename-me.md").exists());

        // 重名被拒绝。
        write_plan(&root, PlanLocation::Plans, "taken-name.md", "# taken\n");
        agent.list_plans().unwrap();
        let error =
            err_message(agent.rename_listed_plan(selection.clone(), "taken-name.md".to_owned()));
        assert!(error.contains("already exists"));
        assert!(root.join(".pp/drafts/rename-me.md").exists());
        std::fs::remove_file(root.join(".pp/plans/taken-name.md")).unwrap();

        // 同名：无操作成功。
        agent
            .rename_listed_plan(selection.clone(), "rename-me.md".to_owned())
            .unwrap();

        // 正常重命名：旧路径消失，字节与 ID 保持，登记与绑定同步。
        let listing = agent
            .rename_listed_plan(selection.clone(), "renamed-plan.md".to_owned())
            .unwrap();
        assert!(!root.join(".pp/drafts/rename-me.md").exists());
        let renamed = root.join(".pp/drafts/renamed-plan.md");
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), content);
        assert_eq!(entry(&listing, "renamed-plan.md").id, old.id);
        let registered = registered(&agent, "renamed-plan.md");
        assert_eq!(registered.id, old.id);
        assert_eq!(registered.location, PlanLocation::Drafts);
        assert_eq!(
            agent.workflow.plan.as_ref().unwrap().file_name.as_deref(),
            Some("renamed-plan.md")
        );
        assert_eq!(agent.workflow.plan.as_ref().unwrap().content, content);
        let session_id = agent.session_id().unwrap().to_owned();
        assert_eq!(
            crate::agent::tests::journal_workflow(&dir.path().join(".pp/agent"), &session_id)
                .plan
                .unwrap()
                .file_name
                .as_deref(),
            Some("renamed-plan.md")
        );

        // 旧 selection 的路径已失效，不能用相同正文授权新路径上的操作。
        assert!(err_message(agent.verify_selection(&selection)).contains("Plan changed"));
        assert!(err_message(agent.delete_listed_plan(selection.clone())).contains("Plan changed"));
        assert!(
            err_message(agent.rename_listed_plan(selection, "stale-rename.md".to_owned()))
                .contains("Plan changed")
        );
        let refreshed = agent.read_listed_plan(&old.id).unwrap();
        assert_eq!(refreshed.path, ".pp/drafts/renamed-plan.md");
        assert!(agent.verify_selection(&refreshed).is_ok());
    }

    #[test]
    fn delete_commits_tombstone_before_unlink_and_clears_binding() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        let content = "# Delete\n\n- [ ] task\n";
        write_plan(&root, PlanLocation::Drafts, "delete-me.md", content);
        write_plan(&root, PlanLocation::Plans, "leftover-name.md", content);
        let listing = agent.list_plans().unwrap();
        let target = entry(&listing, "delete-me.md").clone();
        agent.workflow = bound_workflow(&target.id, "delete-me.md", content);
        seed_session(&mut agent, "delete fixture");
        let selection = agent.read_listed_plan(&target.id).unwrap();

        // 精确字节不匹配：既不提交墓碑也不删除（registered() 仅匹配活动条目）。
        let mut stale = selection.clone();
        stale.content = "# Stale\n".to_owned();
        assert!(err_message(agent.delete_listed_plan(stale)).contains("Plan changed"));
        assert!(root.join(".pp/drafts/delete-me.md").exists());
        let _active = registered(&agent, "delete-me.md");

        // 删除当前绑定：文件与同 inode 残留链接消失，绑定解除且模式保留。
        let leftover_path = root.join(".pp/plans/leftover-name.md");
        std::fs::remove_file(&leftover_path).unwrap();
        std::fs::hard_link(root.join(".pp/drafts/delete-me.md"), &leftover_path).unwrap();
        let listing = agent.delete_listed_plan(selection.clone()).unwrap();
        assert!(listing.entries.iter().all(|entry| entry.id != target.id));
        assert!(!root.join(".pp/drafts/delete-me.md").exists());
        assert!(!leftover_path.exists());
        assert!(agent.workflow.plan.is_none());
        assert_eq!(agent.workflow.mode, WorkMode::Plan);
        let workspace = agent.tools.plan_workspace();
        {
            let _lock = PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let tombstone = snapshot
                .catalog
                .entries
                .iter()
                .find(|entry| entry.id == target.id)
                .unwrap();
            assert!(tombstone.deleted);
            assert!(tombstone.deleted_file_identity.is_some());
        }
        // 后续访问按 ID 报已删除，不从快照补回。
        assert!(err_message(agent.read_listed_plan(&target.id)).contains("Plan was deleted"));
        assert!(err_message(agent.verify_selection(&selection)).contains("Plan was deleted"));
    }

    #[test]
    fn delete_retries_confirmed_leftover_without_readopting() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        let content = "# Retry\n\n- [ ] task\n";
        write_plan(&root, PlanLocation::Drafts, "retry-me.md", content);
        let listing = agent.list_plans().unwrap();
        let id = entry(&listing, "retry-me.md").id.clone();
        let selection = agent.read_listed_plan(&id).unwrap();

        // 手工提交墓碑但不 unlink（模拟首次删除的登记成功/删除失败）。
        let workspace = agent.tools.plan_workspace();
        {
            let _lock = PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let mut catalog = snapshot.catalog;
            let entry = catalog
                .entries
                .iter_mut()
                .find(|entry| entry.id == id)
                .unwrap();
            entry.deleted = true;
            entry.deleted_file_identity = Some(
                plan_catalog::file_identity(workspace, Path::new(&selection.path))
                    .unwrap()
                    .unwrap(),
            );
            plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
        }
        // 残留仍能重新读取以确认删除；同 inode 的外部正文修改使旧确认失效。
        assert_eq!(agent.read_listed_plan(&id).unwrap(), selection);
        std::fs::write(root.join(&selection.path), "# Updated retry\n").unwrap();
        assert!(err_message(agent.delete_listed_plan(selection.clone())).contains("Plan changed"));
        assert!(root.join(&selection.path).exists());
        let selection = agent.read_listed_plan(&id).unwrap();
        // 重试删除：同 inode 残留被清理，登记保持墓碑，不纳管成新 ID。
        agent.delete_listed_plan(selection).unwrap();
        assert!(!root.join(".pp/drafts/retry-me.md").exists());
        let workspace = agent.tools.plan_workspace();
        {
            let _lock = PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let tombstone = snapshot
                .catalog
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .unwrap();
            assert!(tombstone.deleted);
        }
    }

    #[test]
    fn management_respects_live_plan_lease() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        write_plan(&root, PlanLocation::Drafts, "busy-plan.md", "# Busy\n");
        let listing = agent.list_plans().unwrap();
        let id = entry(&listing, "busy-plan.md").id.clone();
        let selection = agent.read_listed_plan(&id).unwrap();

        // 另一个真实 PlanLease 持有者：重命名/删除非阻塞拒绝；列表与预览仍可读。
        let workspace = agent.tools.plan_workspace();
        let holder = PlanLease::acquire(workspace, &id).unwrap();
        // 同一 ID 的第二个 lease 立即冲突（非阻塞，不等待）。
        assert!(err_message(PlanLease::acquire(workspace, &id)).contains("Plan is in use"));
        let rename_error =
            err_message(agent.rename_listed_plan(selection.clone(), "renamed-plan.md".to_owned()));
        assert!(rename_error.contains("Plan is in use"));
        let delete_error = err_message(agent.delete_listed_plan(selection));
        assert!(delete_error.contains("Plan is in use"));
        assert!(root.join(".pp/drafts/busy-plan.md").exists());
        assert!(agent.list_plans().is_ok());
        assert!(agent.read_listed_plan(&id).is_ok());
        drop(holder);
        // 释放后可再管理。
        let selection = agent.read_listed_plan(&id).unwrap();
        agent
            .rename_listed_plan(selection, "renamed-plan.md".to_owned())
            .unwrap();
        assert!(root.join(".pp/drafts/renamed-plan.md").exists());
    }

    #[test]
    fn list_orders_by_modified_desc_then_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let sessions = dir.path().join(".pp/agent/sessions");
        let mut agent = fixture_agent(&dir.path().join("work"), &sessions);
        let root = dir.path().join("work");
        write_plan(&root, PlanLocation::Drafts, "aaa-first.md", "# A\n");
        write_plan(&root, PlanLocation::Drafts, "zzz-second.md", "# Z\n");
        // 首次刷新完成纳管，再单独设置修改时间验证排序。
        agent.list_plans().unwrap();
        let set_modified = |name: &str, seconds: u64| {
            let path = root.join(".pp/drafts").join(name);
            let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
            file.set_times(
                std::fs::FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
            )
            .unwrap();
        };
        set_modified("aaa-first.md", 20);
        set_modified("zzz-second.md", 10);
        let listing = agent.list_plans().unwrap();
        assert_eq!(listing.entries[0].file_name, "aaa-first.md");
        assert_eq!(listing.entries[1].file_name, "zzz-second.md");
    }
}
