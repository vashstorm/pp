//! Registry data types, active entry lookups and structural validation.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::CATALOG_VERSION;
use pp_core::plan::PlanLocation;
use pp_core::plan::valid_id;
use pp_core::plan::validate_stored_file_name;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCatalog {
    pub version: u32,
    pub entries: Vec<PlanCatalogEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanCatalogEntry {
    pub id: String,
    pub file_name: String,
    pub location: PlanLocation,
    pub deleted: bool,
    /// File identity recorded before deletion; only tombstones may carry it, distinguishing delete leftovers
    /// from a different file later created at the same path. Not serialized when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted_file_identity: Option<PlanFileIdentity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanFileIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Registry read snapshot: the parsed catalog plus the exact raw bytes on disk.
/// `raw` provides the baseline for the next `save_catalog`; `None` means the registry file does not exist yet.
#[derive(Debug)]
pub struct CatalogSnapshot {
    pub catalog: PlanCatalog,
    pub raw: Option<Vec<u8>>,
}

impl PlanCatalog {
    pub fn empty() -> Self {
        Self {
            version: CATALOG_VERSION,
            entries: Vec::new(),
        }
    }

    /// Find an active (not deleted) entry by ID; tombstones take no part in identity resolution.
    pub fn active_by_id(&self, id: &str) -> Option<&PlanCatalogEntry> {
        self.entries
            .iter()
            .find(|entry| !entry.deleted && entry.id == id)
    }

    /// Find an entry by active file name; the three lifecycle directories share one global namespace.
    pub fn active_by_file_name(&self, file_name: &str) -> Option<&PlanCatalogEntry> {
        self.entries
            .iter()
            .find(|entry| !entry.deleted && entry.file_name == file_name)
    }
}

/// Validate the registry structure: version, ID/file-name validity, global uniqueness of active entries,
/// and tombstone identity fields allowed only on deleted entries. Any violation returns an explicit error;
/// the caller must not overwrite a violating registry as an empty table.
pub fn validate_catalog(catalog: &PlanCatalog) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::super::fixtures::{entry, index_path, sample_id, workspace};
    use super::super::save_catalog;
    use super::*;

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
        // Validation precedes any disk action, so no partial write is left behind.
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
        // Tombstones take no part in active identity resolution.
        assert!(parsed.active_by_file_name("old.md").is_none());
        assert!(parsed.active_by_id(&sample_id(2)).is_none());
    }
}
