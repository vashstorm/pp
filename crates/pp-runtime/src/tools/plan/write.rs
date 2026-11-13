//! Plan 模式的 `write`：首存登记、绑定目标与后续条件保存。
use anyhow::{Context, Result, bail, ensure};
use pp_core::plan;
use pp_core::plan::PLAN_MAX_BYTES;
use pp_core::plan::PlanDocument;
use pp_core::plan::PlanLocation;
use serde_json::{Value, json};
use std::path::Path;

use super::{MOVED_PLAN, NAME_COLLISION, PlanChange, RENAME_BOUND, save, verified_target};
use crate::tools::WriteArgs;
use crate::tools::plan_catalog;

pub(super) fn write(
    workspace: &save::Workspace,
    document: &PlanDocument,
    args: WriteArgs,
) -> Result<(Value, Option<PlanChange>)> {
    let components = workspace.components(Path::new(&args.path))?;
    let requested = components.iter().collect::<std::path::PathBuf>();
    // 路径校验先于目录创建；Plan 的 write 不能写任意项目文件或登记文件。
    let (file_name, path) = if document.revision == 0 {
        ensure!(
            components.len() == 3 && components[0] == ".pp" && components[1] == "drafts",
            "Tool is not available in Plan mode."
        );
        let file_name = components[2]
            .to_str()
            .context("Plan file name must be valid UTF-8.")?
            .to_owned();
        plan::validate_file_name(&file_name)?;
        let path = plan::path_for(&file_name, PlanLocation::Drafts);
        (file_name, path)
    } else {
        let (file_name, path) = bound_target(workspace, document)?;
        ensure!(
            requested.parent() == Path::new(&path).parent(),
            "Tool is not available in Plan mode."
        );
        ensure!(requested == Path::new(&path), "{RENAME_BOUND}");
        (file_name, path)
    };
    plan::validate_content(&args.content)?;
    let title = plan::plan_display_title(&args.content, &file_name);
    plan::validate_title(&title)?;
    if document.revision == 0 {
        let warning = first_save(workspace, &document.id, &file_name, args.content.as_bytes())?;
        return Ok((
            success(&path, 1, warning),
            Some(PlanChange::Written {
                title,
                file_name,
                content: args.content,
            }),
        ));
    }
    let target = verified_target(workspace, &path, document.content.as_bytes())?;
    if args.content == document.content {
        return Ok((success(&path, document.revision, None), None));
    }
    let revision = document.next_revision()?;
    let warning = target.save_if_unchanged(document.content.as_bytes(), args.content.as_bytes())?;
    Ok((
        success(&path, revision, warning),
        Some(PlanChange::Written {
            title,
            file_name,
            content: args.content,
        }),
    ))
}

/// 登记表活动条目中的计划身份（canonical 名称与位置）。
fn active_entry<'a>(
    catalog: &'a plan_catalog::PlanCatalog,
    id: &str,
) -> Option<&'a plan_catalog::PlanCatalogEntry> {
    catalog.active_by_id(id)
}

fn active_name(catalog: &plan_catalog::PlanCatalog, file_name: &str) -> bool {
    catalog.active_by_file_name(file_name).is_some()
}

/// 碰撞扫描：安全 NOFOLLOW 读取探测同名文件是否已存在于任一生命周期
/// 目录。缺失目录/文件返回 false；符号链接、超限或其他错误如实报错，
/// 绝不静默覆盖。
fn disk_name_exists(workspace: &save::Workspace, path: &str) -> Result<bool> {
    match workspace
        .target(Path::new(path), false)
        .and_then(|target| target.read_bounded(PLAN_MAX_BYTES))
    {
        Ok(_) => Ok(true),
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<rustix::io::Errno>()
                    .is_some_and(|errno| *errno == rustix::io::Errno::NOENT)
            }) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// 首存事务：登记表锁 → load → 活动 id/名称检查 → save_new 原子发布 →
/// 登记当前 ID/名称/位置 → 保存登记表。登记失败时正文已保留，返回
/// “文件已保存但登记失败”；重试仅在当前 ID 已登记同路径且字节匹配时
/// 接纳为首存完成。
fn first_save(
    workspace: &save::Workspace,
    id: &str,
    file_name: &str,
    content: &[u8],
) -> Result<Option<String>> {
    let _lock = plan_catalog::PlanCatalogLock::acquire(workspace)?;
    let snapshot = plan_catalog::load_catalog(workspace)?;
    let path = plan::path_for(file_name, PlanLocation::Drafts);
    if let Some(entry) = active_entry(&snapshot.catalog, id) {
        // 同一 ID 的重试：同路径且磁盘字节匹配才算首存完成。
        let same_target = entry.file_name == file_name && entry.location == PlanLocation::Drafts;
        ensure!(
            same_target,
            "Plan identity is already registered; choose another plan from /plan-list."
        );
        let target = workspace.target(Path::new(&path), false)?;
        let disk = target.read_bounded(PLAN_MAX_BYTES)?;
        ensure!(disk == content, "{NAME_COLLISION}");
        return Ok(None);
    }
    ensure!(
        !active_name(&snapshot.catalog, file_name),
        "{NAME_COLLISION}"
    );
    // 三个生命周期目录内的同名磁盘文件都是碰撞；永不覆盖、不自动换名。
    for location in [
        PlanLocation::Drafts,
        PlanLocation::Plans,
        PlanLocation::Done,
    ] {
        if disk_name_exists(workspace, &plan::path_for(file_name, location))? {
            bail!("{NAME_COLLISION}");
        }
    }
    let target = workspace.target(Path::new(&path), true)?;
    let warning = target.save_new(content)?;
    // save_new 成功后登记；失败保留正文并如实报告。
    let mut catalog = snapshot.catalog;
    catalog.entries.push(plan_catalog::PlanCatalogEntry {
        id: id.to_owned(),
        file_name: file_name.to_owned(),
        location: PlanLocation::Drafts,
        deleted: false,
        deleted_file_identity: None,
    });
    match plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog) {
        Ok(registration_warning) => Ok(warning.or(registration_warning)),
        Err(error) => {
            bail!("Plan file saved but registration failed; retry the same write. ({error:#})")
        }
    }
}

/// 后续写入的绑定目标：登记表条目优先；未登记的旧绑定保持原位。
/// 登记位置与当前绑定不一致时 fail-closed，等待按 ID 协调。
fn bound_target(workspace: &save::Workspace, document: &PlanDocument) -> Result<(String, String)> {
    let _lock = plan_catalog::PlanCatalogLock::acquire(workspace)?;
    let snapshot = plan_catalog::load_catalog(workspace)?;
    let entry = active_entry(&snapshot.catalog, &document.id);
    if let Some(entry) = entry {
        if let Some(bound) = &document.file_name {
            ensure!(
                bound == &entry.file_name && document.location == entry.location,
                "{MOVED_PLAN}"
            );
        } else {
            // 旧记录缺文件名：接受登记 canonical 名称，但位置必须与
            // 当前绑定一致，避免登记/正文路径分叉。
            ensure!(document.location == entry.location, "{MOVED_PLAN}");
        }
    }
    let (file_name, location) = match entry {
        Some(entry) => (entry.file_name.clone(), entry.location),
        None => match (&document.file_name, document.revision) {
            (Some(name), _) => (name.clone(), document.location),
            (None, _) => (format!("{}.md", document.id), document.location),
        },
    };
    plan::validate_stored_file_name(&file_name)?;
    let path = plan::path_for(&file_name, location);
    Ok((file_name, path))
}

fn success(path: &str, revision: u64, warning: Option<String>) -> Value {
    let mut value = json!({"ok":true,"path":path,"revision":revision});
    if let Some(warning) = warning {
        value["warning"] = json!(warning);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{plan_document, register, write_args};
    use super::super::{NAME_COLLISION, RENAME_BOUND, run};
    use super::*;

    #[test]
    fn write_first_revision_is_written_once_and_collision_never_overwrites() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let document = plan_document(0, "", "");
        let file_name = "create-canary.md";
        let path = plan::path_for(file_name, PlanLocation::Drafts);
        let (value, change) = write(
            &workspace,
            &document,
            write_args(file_name, "# Spaced\n\n- [ ] task\n"),
        )
        .unwrap();
        assert_eq!(value, json!({"ok":true,"path":path,"revision":1}));
        let PlanChange::Written {
            title,
            file_name: changed_name,
            content,
        } = change.unwrap()
        else {
            panic!("first write must produce Written");
        };
        assert_eq!(title, "Spaced");
        assert_eq!(changed_name, file_name);
        assert_eq!(content, "# Spaced\n\n- [ ] task\n");
        assert_eq!(
            std::fs::read(dir.path().join(&path)).unwrap(),
            b"# Spaced\n\n- [ ] task\n"
        );
        // 首存完成即登记当前 ID/名称/位置。
        let snapshot = plan_catalog::load_catalog(&workspace).unwrap();
        assert_eq!(snapshot.catalog.entries.len(), 1);
        assert_eq!(snapshot.catalog.entries[0].id, document.id);
        assert_eq!(snapshot.catalog.entries[0].file_name, file_name);
        assert_eq!(snapshot.catalog.entries[0].location, PlanLocation::Drafts);
        // The plan directory contains only the plan file; no temporaries.
        assert_eq!(
            std::fs::read_dir(dir.path().join(".pp/drafts"))
                .unwrap()
                .count(),
            1
        );

        // 三个生命周期目录内的同名文件都阻止首存，且原字节不变。
        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/done")).unwrap();
        for (location, bytes) in [
            (PlanLocation::Plans, b"plans-bytes".as_slice()),
            (PlanLocation::Done, b"done-bytes".as_slice()),
        ] {
            let collision = plan::path_for("dir-collision.md", location);
            std::fs::write(dir.path().join(&collision), bytes).unwrap();
            let other = plan_document(0, "", "");
            let error = write(
                &workspace,
                &other,
                write_args("dir-collision.md", "- [ ] task"),
            )
            .unwrap_err();
            assert_eq!(error.to_string(), NAME_COLLISION);
            assert_eq!(std::fs::read(dir.path().join(&collision)).unwrap(), bytes);
            std::fs::remove_file(dir.path().join(&collision)).unwrap();
        }

        // 活动登记重名阻止首存；磁盘上没有同名文件时同样不留下新文件。
        let registered = "registry-only.md";
        register(
            &workspace,
            &"f".repeat(32),
            registered,
            PlanLocation::Drafts,
        );
        let other = plan_document(0, "", "");
        let error = write(&workspace, &other, write_args(registered, "- [ ] task")).unwrap_err();
        assert_eq!(error.to_string(), NAME_COLLISION);
        assert!(
            !dir.path()
                .join(plan::path_for(registered, PlanLocation::Drafts))
                .exists()
        );

        // 非法命名（路径逃逸、非 kebab）在触碰磁盘前被拒绝。
        for bad in ["../escape.md", "Nested/name.md", "Not-Kebab.md", "ab.md.md"] {
            let other = plan_document(0, "", "");
            let error = write(&workspace, &other, write_args(bad, "- [ ] task")).unwrap_err();
            assert!(!error.to_string().is_empty(), "{bad}");
        }
    }

    #[test]
    fn first_save_retry_accepts_registered_identity_with_matching_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let document = plan_document(0, "", "");
        let file_name = "create-canary.md";
        let path = plan::path_for(file_name, PlanLocation::Drafts);
        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        std::fs::write(dir.path().join(&path), b"- [ ] task\n").unwrap();
        register(&workspace, &document.id, file_name, PlanLocation::Drafts);

        // 同 ID + 同路径 + 字节匹配：接纳为首存完成，不重复写入。
        let (value, change) = first_save(&workspace, &document.id, file_name, b"- [ ] task\n")
            .map(|warning| {
                (
                    success(&path, 1, warning),
                    Some(PlanChange::Written {
                        title: "t".into(),
                        file_name: file_name.to_owned(),
                        content: "- [ ] task\n".into(),
                    }),
                )
            })
            .unwrap();
        assert_eq!(value["ok"], true);
        assert!(change.is_some());
        let snapshot = plan_catalog::load_catalog(&workspace).unwrap();
        assert_eq!(snapshot.catalog.entries.len(), 1);

        // 同路径但字节不同：明确保留冲突，不覆盖。
        let error = first_save(&workspace, &document.id, file_name, b"different\n").unwrap_err();
        assert_eq!(error.to_string(), NAME_COLLISION);
        assert_eq!(
            std::fs::read(dir.path().join(&path)).unwrap(),
            b"- [ ] task\n"
        );

        // 同 ID 已登记到不同名称/位置：拒绝，等待按 ID 协调。
        let error =
            first_save(&workspace, &document.id, "other-name.md", b"- [ ] task\n").unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan identity is already registered; choose another plan from /plan-list."
        );
    }

    #[test]
    fn write_noop_keeps_revision_external_change_and_overflow_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let content = "- [ ] task\n";
        let document = plan_document(1, "Title", content);
        let path = document.path().unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        std::fs::write(dir.path().join(&path), content).unwrap();

        // Same title, name and content: success without a revision bump.
        let (value, change) = write(
            &workspace,
            &document,
            write_args("create-canary.md", content),
        )
        .unwrap();
        assert_eq!(value, json!({"ok":true,"path":path,"revision":1}));
        assert!(change.is_none());

        // 后续写入 file_name 必须等于绑定值；改名走 /plan-list。
        let error = write(&workspace, &document, write_args("renamed.md", content)).unwrap_err();
        assert_eq!(error.to_string(), RENAME_BOUND);

        // A distinct write increments the revision and reports the change.
        let (value, change) = write(
            &workspace,
            &document,
            write_args("create-canary.md", "- [ ] task\n- [ ] second\n"),
        )
        .unwrap();
        assert_eq!(value, json!({"ok":true,"path":path,"revision":2}));
        assert!(matches!(change, Some(PlanChange::Written { .. })));

        // External bytes are never clobbered.
        let external = plan_document(1, "Title", content);
        let external_path = external.path().unwrap();
        std::fs::write(dir.path().join(&external_path), b"external\n").unwrap();
        let error = write(
            &workspace,
            &external,
            write_args("create-canary.md", "- [ ] new\n"),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan file changed externally; review it before writing."
        );
        assert_eq!(
            std::fs::read(dir.path().join(&external_path)).unwrap(),
            b"external\n"
        );

        // Revision overflow is an error, not a wraparound.
        let last = plan_document(u64::MAX, "Title", content);
        std::fs::write(dir.path().join(last.path().unwrap()), content).unwrap();
        let error = write(
            &workspace,
            &last,
            write_args("create-canary.md", "- [ ] new\n"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Plan revision limit reached.");

        // NUL 正文不允许落盘。
        let current = plan_document(1, "Title", content);
        std::fs::write(dir.path().join(current.path().unwrap()), content).unwrap();
        let error = write(
            &workspace,
            &current,
            write_args("create-canary.md", "bad\0content"),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Plan content must not contain NUL.");
        assert_eq!(
            std::fs::read(dir.path().join(current.path().unwrap())).unwrap(),
            content.as_bytes()
        );
    }

    #[test]
    fn write_adopts_registered_names_for_legacy_documents_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let content = "- [ ] task\n";

        // 旧文档缺文件名：登记 canonical 名称优先，旧 <id>.md 仍需精确匹配。
        let mut legacy = plan_document(1, "Title", content);
        legacy.file_name = None;
        legacy.location = PlanLocation::Plans;
        let legacy_path = plan::path_for(&format!("{}.md", legacy.id), PlanLocation::Plans);
        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        std::fs::write(dir.path().join(&legacy_path), content).unwrap();
        register(
            &workspace,
            &legacy.id,
            &format!("{}.md", legacy.id),
            PlanLocation::Plans,
        );
        let (value, change) = write(
            &workspace,
            &legacy,
            WriteArgs {
                path: legacy_path.clone(),
                content: "- [ ] task\n- [ ] two\n".into(),
            },
        )
        .unwrap();
        assert_eq!(value, json!({"ok":true,"path":legacy_path,"revision":2}));
        let PlanChange::Written { file_name, .. } = change.unwrap() else {
            panic!("legacy write must report the adopted name");
        };
        assert_eq!(file_name, format!("{}.md", legacy.id));

        // 登记位置与文档位置分叉时 fail-closed。
        let moved = plan_document(1, "Title", content);
        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        let moved_path = plan::path_for("create-canary.md", PlanLocation::Drafts);
        std::fs::write(dir.path().join(&moved_path), content).unwrap();
        register(
            &workspace,
            &moved.id,
            "create-canary.md",
            PlanLocation::Drafts,
        );
        let mut conflicting = moved;
        conflicting.file_name = Some("create-canary.md".into());
        conflicting.location = PlanLocation::Plans;
        let error = write(
            &workspace,
            &conflicting,
            write_args("create-canary.md", content),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), MOVED_PLAN);
    }

    #[test]
    fn plan_plans_discussion_tools_require_draft_status() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let mut document = plan_document(1, "Selected plan", "- [ ] task\n");
        document.location = PlanLocation::Plans;
        let path = document.path().unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        std::fs::write(dir.path().join(&path), &document.content).unwrap();
        register(
            &workspace,
            &document.id,
            document.file_name.as_deref().unwrap(),
            PlanLocation::Plans,
        );
        let content = "- [ ] task\n\nClarified acceptance criteria.\n";
        let arguments = serde_json::json!({
            "path": path, "content": content
        })
        .to_string();
        for status in [plan::PlanStatus::Ready, plan::PlanStatus::Executed] {
            document.status = status;
            assert!(run(&workspace, &document, None, "write", &arguments).is_err());
            assert!(
                run(
                    &workspace,
                    &document,
                    None,
                    "plan_submit",
                    r#"{"expected_revision":1}"#
                )
                .is_err()
            );
        }
        document.status = plan::PlanStatus::Draft;
        let (result, change) = run(&workspace, &document, None, "write", &arguments).unwrap();
        assert_eq!(result["revision"], 2);
        assert!(matches!(change, Some(PlanChange::Written { .. })));
        document.revision = 2;
        document.content = content.into();
        let (result, change) = run(
            &workspace,
            &document,
            None,
            "plan_submit",
            r#"{"expected_revision":2}"#,
        )
        .unwrap();
        assert_eq!(result["ok"], true);
        assert!(matches!(change, Some(PlanChange::Submitted)));
        assert_eq!(
            std::fs::read_to_string(dir.path().join(path)).unwrap(),
            content
        );
    }
}
