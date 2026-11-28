//! `write` in Plan mode: first-save registration, target binding and subsequent conditional saves.
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
    // Path validation precedes directory creation; a Plan write must not write arbitrary project files or registry files.
    let (file_name, path) = if document.revision == 0 {
        ensure!(
            components.len() == 3
                && components[0] == ".pp"
                && (components[1] == "drafts" || components[1] == "plans"),
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
        let requested = components.iter().collect::<std::path::PathBuf>();
        let bound = Path::new(&path);
        // Legacy-path compatibility allows writing only the same-named bound draft; the save target always uses the registered path.
        let uses_legacy_path = bound.starts_with(".pp/drafts")
            && requested == Path::new(&plan::path_for(&file_name, PlanLocation::Plans));
        if !uses_legacy_path {
            ensure!(
                requested.parent() == bound.parent(),
                "Tool is not available in Plan mode."
            );
            ensure!(requested == bound, "{RENAME_BOUND}");
        }
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

/// Plan identity (canonical name and location) from an active registry entry.
fn active_entry<'a>(
    catalog: &'a plan_catalog::PlanCatalog,
    id: &str,
) -> Option<&'a plan_catalog::PlanCatalogEntry> {
    catalog.active_by_id(id)
}

fn active_name(catalog: &plan_catalog::PlanCatalog, file_name: &str) -> bool {
    catalog.active_by_file_name(file_name).is_some()
}

/// Collision scan: a safe NOFOLLOW read probes whether a same-named file already exists in any
/// lifecycle directory. A missing directory/file returns false; symlinks, over-limit sizes and other
/// errors report truthfully and are never silently overwritten.
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

/// First-save transaction: registry lock → load → active id/name checks → save_new atomic publish →
/// register the current ID/name/location → save the registry. If registration fails the body is kept and it returns
/// "file saved but registration failed"; a retry is accepted as a finished first save only when the current ID is
/// already registered on the same path with matching bytes.
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
        // Retry for the same ID: a first save is complete only on the same path with matching disk bytes.
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
    // A same-named disk file in any of the three lifecycle directories is a collision; never overwrite and never auto-rename.
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
    // Register after save_new succeeds; on failure keep the body and report truthfully.
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

/// Bound target for later writes: the registry entry wins; an unregistered legacy binding stays in place.
/// When the registered location disagrees with the current binding it fails closed, waiting for ID-based reconciliation.
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
            // Legacy record without a file name: accept the registered canonical name, but the location must match
            // the current binding so the registry and body paths do not diverge.
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
        // A finished first save registers the current ID/name/location.
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

        // A same-named file in any of the three lifecycle directories blocks the first save and leaves the original bytes untouched.
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

        // A duplicate active registration blocks the first save; with no same-named file on disk no new file is left behind either.
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

        // Invalid names (path escape, non-kebab) are rejected before touching the disk.
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

        // Same ID + same path + matching bytes: accepted as a finished first save without rewriting.
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

        // Same path but different bytes: keep the conflict explicit and do not overwrite.
        let error = first_save(&workspace, &document.id, file_name, b"different\n").unwrap_err();
        assert_eq!(error.to_string(), NAME_COLLISION);
        assert_eq!(
            std::fs::read(dir.path().join(&path)).unwrap(),
            b"- [ ] task\n"
        );

        // The same ID is already registered under a different name/location: reject and wait for ID-based reconciliation.
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

        // Later writes require file_name to equal the bound value; renaming goes through /plan-list.
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

        // A body containing NUL must not reach disk.
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

        // Legacy document without a file name: the registered canonical name wins, while the old <id>.md still needs an exact match.
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

        // Fail closed when the registered location and the document location diverge.
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
