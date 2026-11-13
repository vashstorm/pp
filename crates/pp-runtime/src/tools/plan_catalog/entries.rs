//! 登记表数据类型、活动条目查找与结构校验。
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::CATALOG_VERSION;
use pp_core::plan::PlanLocation;
use pp_core::plan::valid_id;
use pp_core::plan::validate_stored_file_name;

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
}
