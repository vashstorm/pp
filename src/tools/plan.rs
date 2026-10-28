use crate::agent::input::InputQueue;
use crate::plan::{self, PLAN_MAX_BYTES, PlanDocument, PlanLocation};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;

use super::{plan_catalog, save};

/// Tool-visible outcome of the dedicated plan tools. The protocol-agnostic
/// caller applies it to the borrowed workflow state at the batch commit
/// boundary; executing a tool itself never mutates the workflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanChange {
    Written {
        title: String,
        file_name: String,
        content: String,
    },
    Submitted,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanWriteArgs {
    title: String,
    file_name: String,
    content: String,
    expected_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanSubmitArgs {
    expected_revision: u64,
}

const NAME_COLLISION: &str = "Plan file name already exists; choose another file_name.";
const RENAME_BOUND: &str = "Rename plans through /plan-list.";
const MOVED_PLAN: &str = "Plan file moved; reopen it from /plan-list.";

pub(crate) fn run(
    workspace: &save::Workspace,
    document: &PlanDocument,
    input: Option<&InputQueue>,
    name: &str,
    arguments: &str,
) -> Result<(Value, Option<PlanChange>)> {
    match name {
        "plan_write" => {
            let args: PlanWriteArgs =
                serde_json::from_str(arguments).context("invalid plan_write arguments")?;
            // 列表中 Plans 的计划接受讨论后降为 Draft，允许修改原文件；
            // 待审阅或已交接的 Plans 仍不可被规划工具直接修改。
            ensure!(
                document.location != PlanLocation::Plans
                    || document.status == plan::PlanStatus::Draft,
                "This plan is awaiting pre-execution review; choose a draft from /plan-list to plan again."
            );
            write(workspace, document, args)
        }
        "plan_submit" => {
            let args: PlanSubmitArgs =
                serde_json::from_str(arguments).context("invalid plan_submit arguments")?;
            ensure!(
                document.location != PlanLocation::Plans
                    || document.status == plan::PlanStatus::Draft,
                "This plan is awaiting pre-execution review; choose a draft from /plan-list to plan again."
            );
            submit(workspace, document, input, args)
        }
        _ => bail!("Tool is not available in Plan mode."),
    }
}

fn write(
    workspace: &save::Workspace,
    document: &PlanDocument,
    args: PlanWriteArgs,
) -> Result<(Value, Option<PlanChange>)> {
    ensure!(
        args.expected_revision == document.revision,
        "Plan revision changed; review the latest plan before writing."
    );
    let title = args.title.trim();
    plan::validate_title(title)?;
    plan::validate_content(&args.content)?;
    if document.revision == 0 {
        // 首存：AI 命名走严格 kebab 规则，并在登记事务内发布正文。
        plan::validate_file_name(&args.file_name)?;
        let file_name = args.file_name;
        let path = plan::path_for(&file_name, PlanLocation::Drafts);
        let warning = first_save(workspace, &document.id, &file_name, args.content.as_bytes())?;
        return Ok((
            success(&path, 1, warning),
            Some(PlanChange::Written {
                title: title.to_owned(),
                file_name,
                content: args.content,
            }),
        ));
    }
    // 已绑定名称的后续写入：以登记表 canonical 名称/位置校验，file_name
    // 必须与绑定值一致；改名只能通过 /plan-list。
    let (file_name, path) = bound_target(workspace, document)?;
    ensure!(args.file_name == file_name, "{RENAME_BOUND}");
    let target = verified_target(workspace, &path, document.content.as_bytes())?;
    if title == document.title && args.content == document.content {
        return Ok((success(&path, document.revision, None), None));
    }
    let revision = document.next_revision()?;
    let warning = target.save(args.content.as_bytes())?;
    Ok((
        success(&path, revision, warning),
        Some(PlanChange::Written {
            title: title.to_owned(),
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
            bail!("Plan file saved but registration failed; retry the same plan_write. ({error:#})")
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

fn submit(
    workspace: &save::Workspace,
    document: &PlanDocument,
    input: Option<&InputQueue>,
    args: PlanSubmitArgs,
) -> Result<(Value, Option<PlanChange>)> {
    ensure!(
        document.revision >= 1 && args.expected_revision == document.revision,
        "Plan revision changed; review the latest plan before writing."
    );
    // 提交前同样要求磁盘字节与快照一致；句柄仅用于这次校验。
    let path = document
        .path()
        .ok_or_else(|| anyhow::anyhow!("No saved plan."))?;
    verified_target(workspace, &path, document.content.as_bytes())?;
    let (_, total) = plan::task_progress(&document.content);
    ensure!(
        total > 0,
        "Plan must contain at least one Markdown TODO item."
    );
    // The submit boundary closes steering reception inside one queue lock;
    // pending steering input defers the submission without consuming it.
    if let Some(queue) = input {
        ensure!(
            queue.seal_for_plan_review()?,
            "Apply pending input before submitting the plan."
        );
    }
    Ok((
        json!({"ok":true,"revision":document.revision}),
        Some(PlanChange::Submitted),
    ))
}

fn success(path: &str, revision: u64, warning: Option<String>) -> Value {
    let mut value = json!({"ok":true,"path":path,"revision":revision});
    if let Some(warning) = warning {
        value["warning"] = json!(warning);
    }
    value
}

/// Opens the current plan file through the safe NOFOLLOW handle and verifies
/// the on-disk bytes still match the registered snapshot; an external edit is
/// surfaced, never clobbered. Shared by plan_write and plan_submit.
fn verified_target(
    workspace: &save::Workspace,
    path: &str,
    expected: &[u8],
) -> Result<save::Target> {
    let target = workspace.target(Path::new(path), false)?;
    let disk = target.read_bounded(PLAN_MAX_BYTES)?;
    ensure!(
        disk == expected,
        "Plan file changed externally; review it before writing."
    );
    Ok(target)
}

/// Safe read of the current plan document through the NOFOLLOW workspace
/// handle; oversize, non-UTF-8, symlink and special-type targets are errors,
/// never a fallback to the logged snapshot. A document without a saved
/// lifecycle path reports `No saved plan.` instead of a fake path.
pub(crate) fn read_plan(workspace: &save::Workspace, document: &PlanDocument) -> Result<String> {
    let path = document
        .path()
        .ok_or_else(|| anyhow::anyhow!("No saved plan."))?;
    let target = workspace.target(Path::new(&path), false)?;
    let bytes = target.read_bounded(PLAN_MAX_BYTES)?;
    ensure!(bytes.len() <= PLAN_MAX_BYTES, "Plan file is too large.");
    let content = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("Plan file is not valid UTF-8."))?;
    ensure!(!content.trim().is_empty(), "Plan file is empty.");
    Ok(content.to_owned())
}

/// True only when the workspace-normalized paths are exactly equal; used to
/// react to plan-file edits and to bind review tokens to the reviewed file
/// without canonicalizing in the UI.
pub(crate) fn is_plan_path(workspace: &save::Workspace, path: &str, expected_path: &str) -> bool {
    match (
        workspace.components(Path::new(path)),
        workspace.components(Path::new(expected_path)),
    ) {
        (Ok(changed), Ok(expected)) => changed == expected,
        _ => false,
    }
}

/// Conditional progress write-back for the plan todo adapter. The caller is
/// the Execute+Executed path that already holds the current task's PlanLease
/// (`ensure_task_plan_lease`); no lease is reacquired here and the lock order
/// stays lease → PlanCatalogLock. Within a short catalog lock the active
/// entry is re-checked by ID: non-tombstone, canonical file_name/location in
/// sync with the borrowed document, and located under Plans — progress
/// write-back only happens on handed-off executing plans. Old bytes are read
/// through the bounded NOFOLLOW handle and must match the synced document
/// before `save_if_unchanged_custom` commits; an external edit between the
/// sync and this call fails without overwriting anything. The returned
/// warning (if any) describes an already-committed catalog sync and is
/// passed through to the tool result unchanged.
pub(super) fn save_progress(
    workspace: &save::Workspace,
    document: &PlanDocument,
    content: &str,
) -> Result<Option<String>> {
    let _lock = plan_catalog::PlanCatalogLock::acquire(workspace)?;
    let snapshot = plan_catalog::load_catalog(workspace)?;
    let entry = snapshot
        .catalog
        .active_by_id(&document.id)
        .ok_or_else(|| anyhow::anyhow!("Plan is not registered; reopen it from /plan-list."))?;
    ensure!(
        entry.location == PlanLocation::Plans,
        "Plan progress can only be saved to an executing plan under .pp/plans/."
    );
    let bound_matches = match &document.file_name {
        Some(name) => name == &entry.file_name && document.location == entry.location,
        // 旧记录缺文件名：接受登记 canonical 名称，位置必须一致。
        None => document.location == entry.location,
    };
    ensure!(bound_matches, "{MOVED_PLAN}");
    let path = plan::path_for(&entry.file_name, entry.location);
    let target = workspace.target(Path::new(&path), false)?;
    let old = target.read_bounded(PLAN_MAX_BYTES)?;
    ensure!(
        old == document.content.as_bytes(),
        "Plan file changed externally; review it before writing."
    );
    target.save_if_unchanged_custom(
        document.content.as_bytes(),
        content.as_bytes(),
        "Plan file changed externally; review it before writing.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::PlanLocation;

    fn plan_document(revision: u64, title: &str, content: &str) -> PlanDocument {
        let mut document = PlanDocument::new().unwrap();
        document.revision = revision;
        document.title = title.to_owned();
        document.content = content.to_owned();
        document.origin_prompt = "original task".to_owned();
        if revision >= 1 {
            document.file_name = Some("create-canary.md".to_owned());
            document.location = PlanLocation::Drafts;
        }
        document
    }

    fn write_args(
        title: &str,
        file_name: &str,
        content: &str,
        expected_revision: u64,
    ) -> PlanWriteArgs {
        PlanWriteArgs {
            title: title.to_owned(),
            file_name: file_name.to_owned(),
            content: content.to_owned(),
            expected_revision,
        }
    }

    /// 把已登记条目写入登记表（模拟另一会话的登记）。
    fn register(
        workspace: &save::Workspace,
        id: &str,
        file_name: &str,
        location: PlanLocation,
    ) -> plan_catalog::PlanCatalog {
        let _lock = plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
        let snapshot = plan_catalog::load_catalog(workspace).unwrap();
        let mut catalog = snapshot.catalog;
        catalog.entries.push(plan_catalog::PlanCatalogEntry {
            id: id.to_owned(),
            file_name: file_name.to_owned(),
            location,
            deleted: false,
            deleted_file_identity: None,
        });
        plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
        catalog
    }

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
            write_args("  Spaced  ", file_name, "- [ ] task\n", 0),
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
        assert_eq!(content, "- [ ] task\n");
        assert_eq!(
            std::fs::read(dir.path().join(&path)).unwrap(),
            b"- [ ] task\n"
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
                write_args("Title", "dir-collision.md", "- [ ] task", 0),
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
        let error = write(
            &workspace,
            &other,
            write_args("Title", registered, "- [ ] task", 0),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), NAME_COLLISION);
        assert!(
            !dir.path()
                .join(plan::path_for(registered, PlanLocation::Drafts))
                .exists()
        );

        // 非法命名（路径逃逸、非 kebab）在触碰磁盘前被拒绝。
        for bad in ["../escape.md", "Nested/name.md", "Not-Kebab.md", "ab.md.md"] {
            let other = plan_document(0, "", "");
            let error = write(
                &workspace,
                &other,
                write_args("Title", bad, "- [ ] task", 0),
            )
            .unwrap_err();
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
            write_args("Title", "create-canary.md", content, 1),
        )
        .unwrap();
        assert_eq!(value, json!({"ok":true,"path":path,"revision":1}));
        assert!(change.is_none());

        // 后续写入 file_name 必须等于绑定值；改名走 /plan-list。
        let error = write(
            &workspace,
            &document,
            write_args("Title", "renamed.md", content, 1),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), RENAME_BOUND);

        // A distinct write increments the revision and reports the change.
        let (value, change) = write(
            &workspace,
            &document,
            write_args("Title", "create-canary.md", "- [ ] task\n- [ ] second\n", 1),
        )
        .unwrap();
        assert_eq!(value, json!({"ok":true,"path":path,"revision":2}));
        assert!(matches!(change, Some(PlanChange::Written { .. })));

        // Stale expected_revision is refused before any disk access.
        let error = write(
            &workspace,
            &document,
            write_args("Title", "create-canary.md", content, 0),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan revision changed; review the latest plan before writing."
        );

        // External bytes are never clobbered.
        let external = plan_document(1, "Title", content);
        let external_path = external.path().unwrap();
        std::fs::write(dir.path().join(&external_path), b"external\n").unwrap();
        let error = write(
            &workspace,
            &external,
            write_args("Title", "create-canary.md", "- [ ] new\n", 1),
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
            write_args("Title", "create-canary.md", "- [ ] new\n", u64::MAX),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "Plan revision limit reached.");

        // Invalid titles and NUL content are refused without side effects.
        let current = plan_document(1, "Title", content);
        std::fs::write(dir.path().join(current.path().unwrap()), content).unwrap();
        let error = write(
            &workspace,
            &current,
            write_args("bad\0title", "create-canary.md", content, 1),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan title must contain 1 to 120 characters without control characters."
        );
        let error = write(
            &workspace,
            &current,
            write_args("Title", "create-canary.md", "bad\0content", 1),
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
            write_args(
                "Title",
                &format!("{}.md", legacy.id),
                "- [ ] task\n- [ ] two\n",
                1,
            ),
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
            write_args("Title", "create-canary.md", content, 1),
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
            "title": "Selected plan", "file_name": "create-canary.md",
            "expected_revision": 1, "content": content
        })
        .to_string();
        for status in [plan::PlanStatus::Ready, plan::PlanStatus::Executed] {
            document.status = status;
            assert!(run(&workspace, &document, None, "plan_write", &arguments).is_err());
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
        let (result, change) = run(&workspace, &document, None, "plan_write", &arguments).unwrap();
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

    #[test]
    fn submit_requires_revision_tasklist_and_disk_match() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let tasks = "- [ ] task\n- [x] verified\n";

        // revision 0 cannot be submitted.
        let error = submit(
            &workspace,
            &plan_document(0, "", ""),
            None,
            PlanSubmitArgs {
                expected_revision: 0,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan revision changed; review the latest plan before writing."
        );

        let ready = plan_document(1, "Title", tasks);
        let ready_path = ready.path().unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();

        // A disk/snapshot mismatch is reported before the task-list gate.
        std::fs::write(dir.path().join(&ready_path), "- plain paragraph\n").unwrap();
        let error = submit(
            &workspace,
            &ready,
            None,
            PlanSubmitArgs {
                expected_revision: 1,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan file changed externally; review it before writing."
        );
        std::fs::write(dir.path().join(&ready_path), tasks).unwrap();

        // Fenced examples do not count as a task list.
        let mut fenced = plan_document(2, "Title", "```\n- [ ] not a task\n```\n");
        fenced.file_name = Some("fenced-example.md".into());
        std::fs::write(
            dir.path().join(fenced.path().unwrap()),
            "```\n- [ ] not a task\n```\n",
        )
        .unwrap();
        let error = submit(
            &workspace,
            &fenced,
            None,
            PlanSubmitArgs {
                expected_revision: 2,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Plan must contain at least one Markdown TODO item."
        );

        let (value, change) = submit(
            &workspace,
            &ready,
            None,
            PlanSubmitArgs {
                expected_revision: 1,
            },
        )
        .unwrap();
        assert_eq!(value, json!({"ok":true,"revision":1}));
        assert!(matches!(change, Some(PlanChange::Submitted)));
    }

    #[test]
    fn submit_seals_queue_and_pending_steering_defers_without_consuming() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let tasks = "- [ ] task\n";
        let ready = plan_document(1, "Title", tasks);
        let ready_path = ready.path().unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        std::fs::write(dir.path().join(&ready_path), tasks).unwrap();

        // Pending steering input defers the submission, unchanged and intact.
        let queue = InputQueue::default();
        queue.begin_run().unwrap();
        queue
            .enqueue(
                crate::agent::input::PendingKind::Steering,
                "steer".into(),
                None,
                false,
            )
            .unwrap();
        let before = queue.snapshot_since(None).unwrap().unwrap().revision;
        let error = submit(
            &workspace,
            &ready,
            Some(&queue),
            PlanSubmitArgs {
                expected_revision: 1,
            },
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Apply pending input before submitting the plan."
        );
        assert_eq!(
            queue.snapshot_since(None).unwrap().unwrap().revision,
            before
        );
        assert_eq!(
            queue.claim_steering(false).unwrap().len(),
            1,
            "steering input must survive a deferred submission"
        );

        // Follow-up input does not block submission and stays pending.
        let queue = InputQueue::default();
        queue.begin_run().unwrap();
        queue
            .enqueue(
                crate::agent::input::PendingKind::FollowUp,
                "later".into(),
                None,
                false,
            )
            .unwrap();
        let (value, change) = submit(
            &workspace,
            &ready,
            Some(&queue),
            PlanSubmitArgs {
                expected_revision: 1,
            },
        )
        .unwrap();
        assert_eq!(value, json!({"ok":true,"revision":1}));
        assert!(matches!(change, Some(PlanChange::Submitted)));
        assert!(queue.claim_next().unwrap().is_some());

        // A second submission after the seal still succeeds (idempotent seal).
        let (value, _) = submit(
            &workspace,
            &ready,
            Some(&queue),
            PlanSubmitArgs {
                expected_revision: 1,
            },
        )
        .unwrap();
        assert_eq!(value, json!({"ok":true,"revision":1}));
    }

    #[test]
    fn read_plan_rejects_missing_unsaved_and_special_files() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let document = plan_document(1, "Title", "- [ ] task\n");
        let path = document.path().unwrap();

        // 未保存的 revision-0 绑定没有路径，明确报 No saved plan.。
        let unsaved = plan_document(0, "", "");
        let error = read_plan(&workspace, &unsaved).unwrap_err();
        assert_eq!(error.to_string(), "No saved plan.");

        std::fs::create_dir_all(dir.path().join(".pp/drafts")).unwrap();
        std::fs::create_dir_all(dir.path().join(".pp/plans")).unwrap();
        std::fs::write(dir.path().join(&path), "- [ ] task\n").unwrap();
        assert_eq!(read_plan(&workspace, &document).unwrap(), "- [ ] task\n");

        // Missing, symlinked and non-UTF-8 targets are errors, not snapshots.
        let mut missing = plan_document(1, "Title", "");
        missing.file_name = None;
        missing.location = PlanLocation::Plans;
        missing.id = "4".repeat(32);
        assert!(read_plan(&workspace, &missing).is_err());
        let mut other = plan_document(1, "Title", "");
        other.file_name = None;
        other.location = PlanLocation::Plans;
        std::os::unix::fs::symlink(
            dir.path().join(&path),
            dir.path().join(other.path().unwrap()),
        )
        .unwrap();
        assert!(read_plan(&workspace, &other).is_err());
        let mut binary = plan_document(1, "Title", "");
        binary.file_name = None;
        binary.location = PlanLocation::Plans;
        binary.id = "2".repeat(32);
        std::fs::write(dir.path().join(binary.path().unwrap()), b"\xff\xfe").unwrap();
        assert!(read_plan(&workspace, &binary).is_err());
        let mut empty = plan_document(1, "Title", "");
        empty.file_name = None;
        empty.location = PlanLocation::Plans;
        empty.id = "3".repeat(32);
        std::fs::write(dir.path().join(empty.path().unwrap()), b"  \n").unwrap();
        assert!(read_plan(&workspace, &empty).is_err());
    }

    #[test]
    fn is_plan_path_matches_only_the_exact_plan_file() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let document = plan_document(1, "Title", "");
        let relative = document.path().unwrap();
        let absolute = dir.path().join(&relative);
        assert!(is_plan_path(&workspace, &relative, &relative));
        assert!(is_plan_path(
            &workspace,
            &absolute.to_string_lossy(),
            &relative
        ));
        assert!(is_plan_path(
            &workspace,
            &format!("./{relative}"),
            &relative
        ));
        assert!(!is_plan_path(&workspace, ".pp/drafts", &relative));
        assert!(!is_plan_path(&workspace, ".pp/drafts/other.md", &relative));
        assert!(!is_plan_path(&workspace, "canary.txt", &relative));
        assert!(!is_plan_path(
            &workspace,
            &relative,
            ".pp/drafts/other-name.md"
        ));
        assert!(!is_plan_path(&workspace, "../outside", &relative));
    }

    /// Registered Plans entry plus matching on-disk bytes for one document.
    fn plan_todo_registered(workspace: &save::Workspace, document: &PlanDocument, content: &str) {
        {
            let _lock = plan_catalog::PlanCatalogLock::acquire(workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(workspace).unwrap();
            let mut catalog = snapshot.catalog;
            catalog.entries.push(plan_catalog::PlanCatalogEntry {
                id: document.id.clone(),
                file_name: document.file_name.clone().unwrap(),
                location: PlanLocation::Plans,
                deleted: false,
                deleted_file_identity: None,
            });
            plan_catalog::save_catalog(workspace, snapshot.raw.as_deref(), &catalog).unwrap();
        }
        let target = workspace
            .target(std::path::Path::new(&document.path().unwrap()), true)
            .unwrap();
        target.save_new(content.as_bytes()).unwrap();
    }

    #[test]
    fn plan_todo_save_progress_commits_conditionally_and_refuses_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(dir.path()).unwrap();
        let content = "## Write\n- [ ] Write canary bytes.\n- [ ] Verify canary bytes.\n";
        let mut document = plan_document(1, "Progress", content);
        document.location = PlanLocation::Plans;
        plan_todo_registered(&workspace, &document, content);
        let updated = "## Write\n- [x] Write canary bytes.\n- [ ] Verify canary bytes.\n";
        assert_eq!(save_progress(&workspace, &document, updated).unwrap(), None);
        assert_eq!(
            std::fs::read(dir.path().join(document.path().unwrap())).unwrap(),
            updated.as_bytes()
        );

        // External modification after the sync: the document snapshot no
        // longer matches the disk, so the write-back refuses and the
        // external bytes stay untouched.
        let external = "## Write\n- [x] Write canary bytes.\n- [ ] Verify canary bytes.\n\nExternally appended evidence.\n";
        std::fs::write(dir.path().join(document.path().unwrap()), external).unwrap();
        let error = save_progress(&workspace, &document, updated).unwrap_err();
        assert!(error.to_string().contains("changed externally"), "{error}");
        assert_eq!(
            std::fs::read(dir.path().join(document.path().unwrap())).unwrap(),
            external.as_bytes()
        );

        // Unregistered plan: fail-closed instead of guessing a file name.
        let mut unregistered = plan_document(1, "Other", content);
        unregistered.location = PlanLocation::Plans;
        assert!(save_progress(&workspace, &unregistered, updated).is_err());

        // Tombstoned entry: tombstones never resolve identities.
        let mut tombstoned = plan_document(1, "Progress", content);
        tombstoned.location = PlanLocation::Plans;
        tombstoned.id = document.id.clone();
        {
            let _lock = plan_catalog::PlanCatalogLock::acquire(&workspace).unwrap();
            let snapshot = plan_catalog::load_catalog(&workspace).unwrap();
            let mut catalog = snapshot.catalog;
            catalog
                .entries
                .iter_mut()
                .filter(|entry| entry.id == document.id)
                .for_each(|entry| entry.deleted = true);
            plan_catalog::save_catalog(&workspace, snapshot.raw.as_deref(), &catalog).unwrap();
        }
        assert!(save_progress(&workspace, &tombstoned, updated).is_err());

        // Drafts location: progress write-back is limited to executing
        // plans under .pp/plans/.
        let mut draft = plan_document(1, "Draft", content);
        draft.location = PlanLocation::Drafts;
        plan_todo_registered(&workspace, &draft, content);
        assert!(save_progress(&workspace, &draft, updated).is_err());
    }
}
