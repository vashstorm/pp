use super::{
    PP_DIR_NAME, PlanListEntry, PlanListing, REGISTER_CONFLICT, ScannedFile, is_missing,
    plan_display_title, read_plan_bytes,
};
use crate::agent::Agent;
use anyhow::{Context, Result, anyhow};
use pp_core::plan::PlanLocation;
use pp_core::plan::path_for;
use pp_core::plan::task_progress;
use pp_core::plan::valid_id;
use pp_core::plan::validate_stored_file_name;
use pp_tools::plan_catalog::{self, PlanCatalogEntry, PlanCatalogLock, PlanFileIdentity};
use pp_workspace::save::Workspace;
use rustix::fs::Dir;
use std::ffi::OsString;
use std::path::Path;
use std::time::SystemTime;

impl Agent {
    /// Project plan list: only scans the direct `.md` regular files of the three lifecycle
    /// directories, a missing directory counts as empty, and it never recurses or crosses
    /// projects. The body is still the only source of content/progress; the listing also adopts
    /// unregistered files (taking only the registry lock, not a plan lease). A CAS conflict is
    /// reported upward and never retried or overwritten automatically.
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

            // Registered entries: shown per the scan when a file exists at the registered
            // location, an error entry otherwise.
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
                    // Unregistered file that cannot be stat'ed safely: error entry, operations
                    // disabled, not adopted.
                    if file.stat.is_err() {
                        entries.push(entry_from_scan("", file, workspace));
                        continue;
                    }
                    // Deletion leftover: same path and same inode as the tombstone; keep the old ID
                    // for an explicit retry.
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
                    // Same-name files already adopted by this refresh: same-name files across
                    // directories are shown as a conflict.
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
                    // New adoption: a native 32hex file name keeps its ID when no record (including
                    // tombstones) exists for that ID; otherwise a new ID is generated. A new file at
                    // a tombstone path is adopted under a new ID and the old ID is not revived.
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
                    // Already shown as the registered entry.
                    continue;
                }
                // Same ID on multiple paths: a same-inode handoff leftover is shown only at the
                // registered location; a same-name file with a different inode is a conflict,
                // reported as an error entry with operations disabled.
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
        // modified descending, file_name ascending; entries with a missing file sink to the
        // bottom by earliest time.
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

/// Scan the direct `.md` children of one lifecycle directory through a NOFOLLOW directory fd;
/// a missing directory is empty, symlinks are not followed and nothing is recursive. Non-UTF-8
/// names and invalid plan file names go to diagnostics and stay out of the list; each valid name
/// keeps one stat (symlinks/special files fail at stat).
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

/// Build a list entry from the scan result; a failed body/identity read degrades to an error
/// entry with operations disabled, keeping the file name and directory category and inventing no
/// progress or title.
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
        // A native 32hex file name keeps that ID.
        let native_id = "0123456789abcdef0123456789abcdef";
        write_plan(
            &root,
            PlanLocation::Plans,
            &format!("{native_id}.md"),
            "# Native\n\n- [ ] task\n",
        );
        // Legacy Unicode names are preserved and adopted under a new ID.
        write_plan(&root, PlanLocation::Done, "旧计划.md", "# 旧计划\n");
        // Invalid names, non-plan files, directories and fenced examples stay out of the list.
        // A name the stored rules reject (containing a backslash) is written to diagnostics and
        // stays out of the list.
        write_plan(&root, PlanLocation::Drafts, "back\\slash.md", "# bad\n");
        write_plan(&root, PlanLocation::Drafts, "notes.txt", "hello");
        std::fs::create_dir_all(root.join(".pp/drafts/nested.md")).unwrap();
        std::fs::write(root.join(".pp/drafts/nested.md/inner.md"), "# inner\n").unwrap();
        // Symlink: error entry, operations disabled, not followed.
        std::os::unix::fs::symlink(
            root.join(".pp/drafts/fix-config-loading.md"),
            root.join(".pp/drafts/link.md"),
        )
        .unwrap();
        // Non-UTF-8 name: diagnostics only. The macOS filesystem rejects invalid UTF-8 names
        // (EILSEQ), in which case this scenario is skipped.
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

        // Adoption is persisted; a repeated refresh is idempotent and produces no duplicate IDs or
        // entries.
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

        // The registered file disappeared: an error entry, not an empty-list illusion.
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

        // A same-name file with a different inode appears in another directory for a registered
        // name: conflict entry.
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

        // Same-inode handoff leftover: only the registered location is shown and nothing is
        // double-counted.
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

        // Commit a tombstone by hand without unlinking: the list keeps the entry with the old ID
        // for an explicit deletion retry.
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

        // A same-inode leftover is not adopted as a new ID; a genuinely new file with a different
        // inode created later does get a new ID.
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
        // The first refresh performs the adoption; modify times are then set separately to verify
        // ordering.
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
