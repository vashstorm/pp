//! 项目计划身份登记表（`.pp/plan-index.json`）与计划级辅助锁。
//!
//! 登记表是跨会话的稳定身份来源：只保存 `{id, file_name, location, deleted,
//! deleted_file_identity}`，不保存正文、标题、进度或会话信息；它不是第二套
//! TODO 数据库，也不授予执行权限。所有读写都经 `Workspace`/`Target` 的
//! NOFOLLOW 安全路径；并发变更通过读取基线与 `save_if_unchanged` 拒绝，
//! 绝不自动重试或覆盖。
//!
//! 锁约定：同一计划 ID 的模型任务与管理动作互斥使用 `.pp/plan-locks/<id>.lock`
//! （`PlanLease`）；登记表的短事务另用 `.pp/plan-index.lock`
//! （`PlanCatalogLock`）。同时持有时顺序固定为计划 lease → 登记表锁。
//! `load_catalog`/`save_catalog` 不自行加锁，调用方必须已持有
//! `PlanCatalogLock`。

mod entries;
mod io;
mod lock;
mod moves;

pub(crate) use entries::{
    CatalogSnapshot, PlanCatalog, PlanCatalogEntry, PlanFileIdentity, validate_catalog,
};
pub(crate) use io::{load_catalog, save_catalog};
pub(crate) use lock::{PlanCatalogLock, PlanLease};
pub(crate) use moves::{
    cleanup_confirmed_leftover, commit_plan_move, file_identity, publish_plan_move,
};

/// 登记表读取上限 16 MiB；损坏或超大的登记文件必须显式报错，禁止当空表覆盖。
pub(crate) const CATALOG_MAX_BYTES: usize = 16 * 1024 * 1024;

/// 当前登记表 schema 版本；加载时不匹配即拒绝。
pub(crate) const CATALOG_VERSION: u32 = 1;

#[cfg(test)]
mod fixtures {
    use super::*;
    use crate::tools::save::Workspace;
    use pp_core::plan::PlanLocation;
    use std::path::PathBuf;

    pub(super) fn workspace(dir: &tempfile::TempDir) -> Workspace {
        Workspace::open(dir.path()).unwrap()
    }

    pub(super) fn sample_id(number: u8) -> String {
        format!("{number:032x}")
    }

    pub(super) fn entry(
        id: &str,
        file_name: &str,
        location: PlanLocation,
        deleted: bool,
    ) -> PlanCatalogEntry {
        PlanCatalogEntry {
            id: id.to_owned(),
            file_name: file_name.to_owned(),
            location,
            deleted,
            deleted_file_identity: None,
        }
    }

    pub(super) fn index_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join(".pp/plan-index.json")
    }
}
