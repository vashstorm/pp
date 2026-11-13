//! 登记表的安全读写：缺失即空表、损坏显式拒绝、基线对比提交。
use anyhow::{Context, Result, ensure};
use rustix::io::Errno;
use std::path::Path;

use super::{CATALOG_MAX_BYTES, CatalogSnapshot, PlanCatalog, validate_catalog};
use crate::tools::save::Workspace;

const INDEX_PATH: &str = ".pp/plan-index.json";

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
}
