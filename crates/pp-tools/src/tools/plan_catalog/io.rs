//! Safe registry I/O: missing means empty, corruption is rejected explicitly, commits compare baselines.
use anyhow::{Context, Result, ensure};
use rustix::io::Errno;
use std::path::Path;

use super::{CATALOG_MAX_BYTES, CatalogSnapshot, PlanCatalog, validate_catalog};
use pp_workspace::save::Workspace;

const INDEX_PATH: &str = ".pp/plan-index.json";

/// Read the registry; the caller must already hold `PlanCatalogLock`.
/// A missing `.pp` or registry file means nothing is registered yet, returning an empty catalog with `raw = None`;
/// corruption, oversized content or invalid fields return an explicit error, never a fake "empty table" that could be overwritten.
pub fn load_catalog(workspace: &Workspace) -> Result<CatalogSnapshot> {
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

/// Commit the registry against the exact bytes provided by `previous`; the caller must already hold
/// `PlanCatalogLock`. `previous = None` means the registry file does not exist yet, so it is published with an exclusive
/// create; concurrent creation or a changed baseline both fail explicitly, never retried automatically. The return value is a parent-directory
/// sync warning (it can appear after a successful commit and does not mean failure).
pub fn save_catalog(
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
        // Do not leak serde/parser details, so the internal format never becomes a dependable diagnostic.
        anyhow::anyhow!("Plan index is corrupted; refusing to load or overwrite it.")
    })?;
    validate_catalog(&catalog)?;
    Ok(catalog)
}

/// Whether the error chain is only "path does not exist"; a missing directory or file means nothing is registered yet.
/// Other errors (for example `.pp` replaced by a regular file) must surface and never pass as an empty table.
fn is_missing(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<Errno>() == Some(&Errno::NOENT))
}

#[cfg(test)]
mod tests {
    use super::super::fixtures::{entry, index_path, sample_id, workspace};
    use super::*;
    use pp_core::plan::PlanLocation;
    use std::fs;

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

        // An existing registry file cannot be exclusively created again from a None baseline.
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
        // Another writer commits first, making snapshot.raw a stale baseline.
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
        // Rejection does not overwrite the disk: the old content is kept as is.
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
            // A failed registration likewise does not overwrite the existing broken file.
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
}
