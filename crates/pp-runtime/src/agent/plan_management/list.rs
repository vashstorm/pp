use super::{
    PP_DIR_NAME, PlanListEntry, PlanListing, REGISTER_CONFLICT, ScannedFile, is_missing,
    plan_display_title, read_plan_bytes,
};
use crate::agent::Agent;
use crate::tools::plan_catalog::{self, PlanCatalogEntry, PlanCatalogLock, PlanFileIdentity};
use crate::tools::save::Workspace;
use anyhow::{Context, Result, anyhow};
use pp_core::plan::PlanLocation;
use pp_core::plan::path_for;
use pp_core::plan::task_progress;
use pp_core::plan::valid_id;
use pp_core::plan::validate_stored_file_name;
use rustix::fs::Dir;
use std::ffi::OsString;
use std::path::Path;
use std::time::SystemTime;

impl Agent {
    /// 项目计划列表：只扫描三个生命周期目录的直接 `.md` 普通文件，缺失
    /// 目录为空，不递归、不跨项目。正文仍是唯一内容/进度来源；列表同时
    /// 完成未登记文件的纳管（仅取登记表锁，不加计划 lease）。CAS 冲突
    /// 向上报告，不自动重试或覆盖。
    pub fn list_plans(&mut self) -> Result<PlanListing> {
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

fn generate_id() -> Result<String> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| anyhow!("Could not allocate a plan ID."))?;
    Ok(format!("{:032x}", u128::from_be_bytes(bytes)))
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{entry, fixture_agent, registered, write_plan};
    use super::*;
    use std::time::Duration;

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

        // 纳管已持久化；重复刷新冗等且不产生重复 ID/条目。
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
