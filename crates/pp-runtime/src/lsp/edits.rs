//! LSP 修改建议的只读验证与虚拟预览，不包含提交或回滚入口。

mod readonly;
mod rename;
mod sync;
mod text;

use crate::lsp::types::CreateFile;
use crate::lsp::types::DeleteFile;
use crate::lsp::types::RenameFile;
use crate::lsp::types::TextDocumentEdit;
use crate::lsp::types::TextEdit;
use crate::lsp::types::WorkspaceEdit;
use crate::lsp::uri::uri_to_path;
use crate::tools::Tools;
use crate::tools::changes::Original;
use crate::tools::save::DirectoryEntry;
use crate::tools::save::EntryKind;
use crate::tools::save::RESOURCE_FILE_LIMIT;
use crate::tools::save::Workspace;
use anyhow::{Context, Result, bail, ensure};
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::check;
use serde_json::Value;
use std::ffi::OsStr;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub(crate) use self::readonly::{suggest, validate_move};
pub(crate) use self::sync::sync_file_events;
pub(crate) use self::text::apply_text_edits;

fn clone_original(original: &Original) -> Original {
    match original {
        Original::Missing => Original::Missing,
        Original::Bytes(bytes) => Original::Bytes(bytes.clone()),
        Original::Unavailable => Original::Unavailable,
    }
}

fn net_unchanged(before: &Original, after: Option<&[u8]>) -> bool {
    match (before, after) {
        (Original::Missing, None) => true,
        (Original::Bytes(before), Some(after)) => before.as_slice() == after,
        _ => false,
    }
}

/// 查询时的磁盘基线与最终虚拟内容，仅用于建议和 diff。
struct Record {
    before: Original,
    after: Option<Vec<u8>>,
    resource: bool,
}

struct Plan {
    records: BTreeMap<PathBuf, Record>,
    summary: Vec<String>,
    warnings: Vec<String>,
}

/// 规范化路径与用于摘要/记录的相对路径。
struct Resolved {
    path: PathBuf,
    relative: PathBuf,
}

enum NodeState {
    Missing,
    File(Vec<u8>, Option<u32>),
    Directory(Option<u32>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    File,
    Directory { permissions: Option<u32> },
    Missing,
}

/// 虚拟文件状态：`before` 是事务开始前的磁盘快照，`current` 是按声明顺序
/// 推进后的内容。`before` 从不被后续操作覆盖，撤销基线因此保持稳定。
struct Virtual {
    relative: PathBuf,
    before: Original,
    current: Option<Vec<u8>>,
    permissions: Option<u32>,
    resource: bool,
    /// 版本证明使用的原始内容：文本编辑只推进 `current`，`proof` 始终是
    /// 该文件在事务开始前的磁盘字节（重命名目标沿用来源文件的证明）。
    proof: Original,
    /// 版本证明对应的客户端文档路径：事务内重命名后，新路径的文件仍由
    /// 发起服务器打开的原路径文档证明，不能要求服务器已经打开新路径。
    origin: Option<PathBuf>,
}

struct Planner<'a> {
    client: Option<&'a crate::lsp::Client>,
    workspace: &'a Workspace,
    cancellation: &'a CancellationToken,
    files: BTreeMap<PathBuf, Virtual>,
    /// 每个路径当前虚拟类型；文件删除/目录删除/重命名都通过它覆盖磁盘状态，
    /// 因此虚拟目录遍历不会把本事务已删除或移走的磁盘节点算回来。
    kinds: BTreeMap<PathBuf, Kind>,
    summary: Vec<String>,
    warnings: Vec<String>,
}

fn plan(
    tools: &Tools,
    client: Option<&crate::lsp::Client>,
    edit: &WorkspaceEdit,
    cancellation: &CancellationToken,
) -> Result<Plan> {
    let mut planner = Planner {
        client,
        workspace: tools.workspace(),
        cancellation,
        files: BTreeMap::new(),
        kinds: BTreeMap::new(),
        summary: Vec::new(),
        warnings: Vec::new(),
    };
    if let Some(changes) = &edit.document_changes {
        if edit.changes.as_ref().is_some_and(|map| !map.is_empty()) {
            planner.warnings.push(
                "the workspace edit contained both changes and documentChanges; documentChanges took precedence and changes were ignored"
                    .to_owned(),
            );
        }
        planner.plan_document_changes(changes)?;
    } else if let Some(changes) = &edit.changes {
        planner.plan_changes(changes)?;
    }
    Ok(planner.finish())
}

impl<'a> Planner<'a> {
    fn resolve(&self, uri: &str) -> Result<Resolved> {
        let path = uri_to_path(uri).with_context(|| {
            format!("unsupported workspace edit URI (only absolute file: URIs are allowed): {uri}")
        })?;
        let relative = self
            .workspace
            .components(&path)?
            .into_iter()
            .collect::<PathBuf>();
        Ok(Resolved {
            path: self.workspace.path.join(&relative),
            relative,
        })
    }

    /// 只读判断当前（虚拟优先）节点状态。
    fn node(&self, path: &Path) -> Result<NodeState> {
        if let Some(kind) = self.kinds.get(path) {
            return match kind {
                Kind::File => {
                    let state = self
                        .files
                        .get(path)
                        .context("virtual file state is missing")?;
                    match &state.current {
                        Some(bytes) => Ok(NodeState::File(bytes.clone(), state.permissions)),
                        None => Ok(NodeState::Missing),
                    }
                }
                Kind::Directory { permissions } => Ok(NodeState::Directory(*permissions)),
                Kind::Missing => Ok(NodeState::Missing),
            };
        }
        // 祖先虚拟类型优先：被删除/移走的目录下的路径必须立即消失，不能被
        // 磁盘上尚未提交的旧节点重新看见。
        let parts = self.workspace.components(path)?;
        let mut prefix = self.workspace.path.clone();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            prefix.push(part);
            match self.kinds.get(&prefix) {
                Some(Kind::Missing) => return Ok(NodeState::Missing),
                Some(Kind::File) => bail!(
                    "workspace edit path {} has a file at an ancestor component",
                    path.display()
                ),
                Some(Kind::Directory { .. }) => {}
                None => {
                    if self
                        .files
                        .get(&prefix)
                        .is_some_and(|state| state.current.is_some())
                    {
                        bail!(
                            "workspace edit path {} has a file at an ancestor component",
                            path.display()
                        );
                    }
                }
            }
        }
        self.disk_node(path)
    }

    fn disk_node(&self, path: &Path) -> Result<NodeState> {
        match self.workspace.resource_target(path, false)? {
            None => Ok(NodeState::Missing),
            Some(target) => match target.kind()? {
                EntryKind::Missing => Ok(NodeState::Missing),
                EntryKind::Directory => Ok(NodeState::Directory(target.permissions()?)),
                EntryKind::File => {
                    let permissions = target.permissions()?;
                    Ok(NodeState::File(target.read()?, permissions))
                }
            },
        }
    }

    /// 首次接触某个文件时保存事务开始前的磁盘基线；之后只推进虚拟内容。
    fn ensure_file(&mut self, path: &Path, relative: &Path) -> Result<()> {
        if self.files.contains_key(path) {
            if matches!(self.kinds.get(path), Some(Kind::Directory { .. })) {
                bail!(
                    "workspace edit targets {} as a file, but a directory exists there",
                    relative.display()
                );
            }
            return Ok(());
        }
        let (before, current, permissions) = match self.node(path)? {
            NodeState::Missing => (Original::Missing, None, None),
            NodeState::File(bytes, permissions) => {
                (Original::Bytes(bytes.clone()), Some(bytes), permissions)
            }
            NodeState::Directory(_) => bail!(
                "workspace edit targets {} as a file, but a directory exists there",
                relative.display()
            ),
        };
        let kind = if current.is_some() {
            Kind::File
        } else {
            Kind::Missing
        };
        self.files.insert(
            path.to_path_buf(),
            Virtual {
                relative: relative.to_path_buf(),
                before: clone_original(&before),
                current,
                permissions,
                resource: false,
                proof: before,
                origin: None,
            },
        );
        self.kinds.insert(path.to_path_buf(), kind);
        Ok(())
    }

    fn mark_file(&mut self, path: &Path, current: Option<Vec<u8>>, resource: bool) {
        let present = current.is_some();
        if let Some(state) = self.files.get_mut(path) {
            state.current = current;
            state.resource |= resource;
        }
        let kind = if present { Kind::File } else { Kind::Missing };
        self.kinds.insert(path.to_path_buf(), kind);
    }

    fn set_file_permissions(&mut self, path: &Path, permissions: Option<u32>) {
        if let Some(state) = self.files.get_mut(path) {
            state.permissions = permissions;
        }
    }

    /// 文件操作会隐式创建父目录：虚拟状态先登记为目录，提交时由
    /// `resource_target_tracked` 真正创建并跟踪身份。
    fn ensure_virtual_parents(&mut self, path: &Path) -> Result<()> {
        let parts = self.workspace.components(path)?;
        let mut prefix = self.workspace.path.clone();
        for part in parts.iter().take(parts.len().saturating_sub(1)) {
            prefix.push(part);
            match self.kinds.get(&prefix) {
                Some(Kind::File) => bail!(
                    "workspace edit path {} has a file at an ancestor component",
                    path.display()
                ),
                Some(Kind::Directory { .. }) => continue,
                Some(Kind::Missing) => {
                    // 本事务先删除了该路径上的文件/目录，随后在旧路径下创建
                    // 子节点：磁盘上的旧节点会先被删除，这里直接重建成目录。
                    self.kinds
                        .insert(prefix.clone(), Kind::Directory { permissions: None });
                }
                None => {
                    if matches!(self.disk_node(&prefix)?, NodeState::File(..)) {
                        bail!(
                            "workspace edit path {} has a file at an ancestor component",
                            path.display()
                        );
                    }
                    self.kinds
                        .insert(prefix.clone(), Kind::Directory { permissions: None });
                }
            }
        }
        Ok(())
    }

    fn plan_changes(
        &mut self,
        changes: &std::collections::HashMap<String, Vec<TextEdit>>,
    ) -> Result<()> {
        let mut uris: Vec<&String> = changes.keys().collect();
        uris.sort();
        for uri in uris {
            let edits = &changes[uri];
            if edits.is_empty() {
                continue;
            }
            let resolved = self.resolve(uri)?;
            self.plan_text(resolved, edits)?;
        }
        Ok(())
    }

    fn plan_document_changes(&mut self, changes: &[Value]) -> Result<()> {
        let mut pending: BTreeMap<String, Vec<TextEdit>> = BTreeMap::new();
        for change in changes {
            if change.get("textDocument").is_some() {
                let document: TextDocumentEdit = serde_json::from_value(change.clone())
                    .context("invalid textDocument edit in workspace edit")?;
                self.check_version(&document)?;
                if !document.edits.is_empty() {
                    pending
                        .entry(document.text_document.uri.clone())
                        .or_default()
                        .extend(document.edits.iter().cloned());
                }
                continue;
            }
            let Some(kind) = change.get("kind").and_then(Value::as_str) else {
                bail!("unsupported workspace edit documentChanges entry");
            };
            match kind {
                "create" => {
                    let create: CreateFile = serde_json::from_value(change.clone())
                        .context("invalid createFile entry in workspace edit")?;
                    self.flush_subtree(&mut pending, &create.uri)?;
                    self.plan_create(&create)?;
                }
                "rename" => {
                    let rename: RenameFile = serde_json::from_value(change.clone())
                        .context("invalid renameFile entry in workspace edit")?;
                    // 先推进两端的虚拟文本，再模拟移动。
                    self.flush_subtree(&mut pending, &rename.old_uri)?;
                    self.flush_subtree(&mut pending, &rename.new_uri)?;
                    self.plan_rename(&rename)?;
                }
                "delete" => {
                    let delete: DeleteFile = serde_json::from_value(change.clone())
                        .context("invalid deleteFile entry in workspace edit")?;
                    self.flush_subtree(&mut pending, &delete.uri)?;
                    self.plan_delete(&delete)?;
                }
                other => bail!("unsupported workspace edit documentChanges kind: {other}"),
            }
        }
        for uri in pending.keys().cloned().collect::<Vec<_>>() {
            self.flush_uri(&mut pending, &uri)?;
        }
        Ok(())
    }

    fn flush_uri(
        &mut self,
        pending: &mut BTreeMap<String, Vec<TextEdit>>,
        uri: &str,
    ) -> Result<()> {
        if let Some(edits) = pending.remove(uri)
            && !edits.is_empty()
        {
            let resolved = self.resolve(uri)?;
            self.plan_text(resolved, &edits)?;
        }
        Ok(())
    }

    fn flush_subtree(
        &mut self,
        pending: &mut BTreeMap<String, Vec<TextEdit>>,
        uri: &str,
    ) -> Result<()> {
        let prefix = format!("{}/", uri.trim_end_matches('/'));
        let targets: Vec<String> = pending
            .keys()
            .filter(|candidate| candidate.as_str() == uri || candidate.starts_with(&prefix))
            .cloned()
            .collect();
        for target in targets {
            self.flush_uri(pending, &target)?;
        }
        Ok(())
    }

    fn plan_text(&mut self, resolved: Resolved, edits: &[TextEdit]) -> Result<()> {
        if edits.is_empty() {
            return Ok(());
        }
        check(self.cancellation)?;
        self.ensure_file(&resolved.path, &resolved.relative)?;
        let current = self
            .files
            .get(&resolved.path)
            .and_then(|state| state.current.clone());
        let display = resolved.relative.display();
        let Some(current) = current else {
            bail!("cannot apply LSP text edits to {display}: the file does not exist");
        };
        let source = std::str::from_utf8(&current).with_context(|| {
            format!("cannot apply LSP text edits to {display}: the file is not valid UTF-8")
        })?;
        let content = apply_text_edits(source, edits)?;
        if content.as_bytes() == current.as_slice() {
            self.summary.push(format!(
                "No change from {} edit(s) to {display}",
                edits.len()
            ));
            return Ok(());
        }
        let bytes = content.into_bytes();

        self.mark_file(&resolved.path, Some(bytes), false);
        self.summary
            .push(format!("Suggest {} edit(s) to {display}", edits.len()));
        Ok(())
    }

    /// 虚拟目录快照：以磁盘目录为底，按声明顺序叠加文件内容、删除/移动类型
    /// 与隐式创建的父目录。这样文本编辑、文件删除之后再重命名/递归删除同一
    /// 棵树时，快照与此前的虚拟操作一致。
    fn walk_virtual(&self, path: &Path) -> Result<Vec<DirectoryEntry>> {
        check(self.cancellation)?;
        if !matches!(self.node(path)?, NodeState::Directory(_)) {
            bail!("resource path must be a directory");
        }
        let mut entries: BTreeMap<PathBuf, (Option<Vec<u8>>, Option<u32>)> = BTreeMap::new();
        if matches!(self.disk_node(path)?, NodeState::Directory(_)) {
            for entry in self.workspace.walk_directory_cancellable(
                path,
                RESOURCE_FILE_LIMIT,
                self.cancellation,
            )? {
                check(self.cancellation)?;
                entries.insert(entry.relative, (entry.content, entry.permissions));
            }
        }
        for (absolute, kind) in &self.kinds {
            check(self.cancellation)?;
            let Some(relative) = relative_under(path, absolute) else {
                continue;
            };
            match kind {
                Kind::Missing => remove_tree_entries(&mut entries, &relative),
                Kind::Directory { permissions } => {
                    let existing = entries.get(&relative).and_then(|entry| entry.1);
                    entries.insert(relative, (None, permissions.or(existing)));
                }
                Kind::File => remove_descendants(&mut entries, &relative),
            }
        }
        for (absolute, state) in &self.files {
            check(self.cancellation)?;
            let Some(relative) = relative_under(path, absolute) else {
                continue;
            };
            if matches!(self.kinds.get(absolute), Some(Kind::Directory { .. })) {
                continue;
            }
            match &state.current {
                Some(bytes) => {
                    entries.insert(relative, (Some(bytes.clone()), state.permissions));
                }
                None => {
                    entries.remove(&relative);
                }
            }
        }
        // 被祖先 Missing/File 覆盖的节点不能从磁盘旧状态复活。
        let keys: Vec<PathBuf> = entries.keys().cloned().collect();
        for relative in keys {
            check(self.cancellation)?;
            if self.virtual_hidden(path, &relative) {
                entries.remove(&relative);
            }
        }
        // 为嵌套文件/目录补齐父目录条目；磁盘已有的权限保持不变。
        let keys: Vec<PathBuf> = entries.keys().cloned().collect();
        for relative in keys {
            check(self.cancellation)?;
            let components: Vec<&OsStr> = relative.iter().collect();
            let mut prefix = PathBuf::new();
            for component in components.iter().take(components.len().saturating_sub(1)) {
                prefix.push(component);
                entries.entry(prefix.clone()).or_insert((None, None));
            }
        }
        // 上限只约束普通文件；目录本身不消耗文件额度。
        let files = entries
            .values()
            .filter(|(content, _)| content.is_some())
            .count();
        ensure!(
            files <= RESOURCE_FILE_LIMIT,
            "directory contains more than {RESOURCE_FILE_LIMIT} files; resource operation refused"
        );
        Ok(entries
            .into_iter()
            .map(|(relative, (content, permissions))| DirectoryEntry {
                relative,
                content,
                permissions,
            })
            .collect())
    }

    fn virtual_hidden(&self, root: &Path, relative: &Path) -> bool {
        let components: Vec<&OsStr> = relative.iter().collect();
        let mut prefix = PathBuf::new();
        for (index, component) in components.iter().enumerate() {
            prefix.push(component);
            match self.kinds.get(&root.join(&prefix)) {
                Some(Kind::Missing) => return true,
                Some(Kind::File) if index + 1 < components.len() => return true,
                _ => {}
            }
        }
        false
    }

    fn finish(self) -> Plan {
        let mut records = BTreeMap::new();
        for (_, state) in self.files {
            if !net_unchanged(&state.before, state.current.as_deref()) {
                records.insert(
                    state.relative,
                    Record {
                        before: state.before,
                        after: state.current,
                        resource: state.resource,
                    },
                );
            }
        }
        Plan {
            records,
            summary: self.summary,
            warnings: self.warnings,
        }
    }
}

fn relative_under(root: &Path, absolute: &Path) -> Option<PathBuf> {
    absolute
        .strip_prefix(root)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(PathBuf::from)
}

fn remove_tree_entries(
    entries: &mut BTreeMap<PathBuf, (Option<Vec<u8>>, Option<u32>)>,
    relative: &Path,
) {
    entries.retain(|key, _| key != relative && !key.starts_with(relative));
}

fn remove_descendants(
    entries: &mut BTreeMap<PathBuf, (Option<Vec<u8>>, Option<u32>)>,
    relative: &Path,
) {
    entries.retain(|key, _| key == relative || !key.starts_with(relative));
}

#[cfg(test)]
mod tests;
