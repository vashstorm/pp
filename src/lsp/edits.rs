//! LSP 修改建议的只读验证与虚拟预览，不包含提交或回滚入口。

use crate::tools::save::RESOURCE_FILE_LIMIT;
use crate::{
    cancellation::{CancellationToken, check},
    lsp::{
        Registry,
        client::{position_offset, uri_to_path},
        types::{
            CreateFile, CreateFileOptions, DeleteFile, DeleteFileOptions, Position, Range,
            RenameFile, RenameFileOptions, TextDocumentEdit, TextEdit, WorkspaceEdit,
        },
    },
    tools::{
        Tools,
        changes::Original,
        save::{DirectoryEntry, EntryKind, Workspace},
    },
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::ffi::OsStr;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Instant,
};
const SNIPPET_EDIT: &str = "snippet-formatted LSP edits are unsupported";

/// 校验文件移动建议的工作区边界、节点类型和目标冲突，不创建目录。
pub(crate) fn validate_move(
    tools: &Tools,
    source: &Path,
    dest: &Path,
    cancellation: &CancellationToken,
) -> Result<()> {
    let edit = WorkspaceEdit {
        changes: None,
        document_changes: Some(vec![
            serde_json::json!({"kind":"rename","oldUri":crate::lsp::client::path_to_uri(source),"newUri":crate::lsp::client::path_to_uri(dest)}),
        ]),
    };
    plan(tools, None, &edit, cancellation).map(|_| ())
}

/// 只读建议：保留原始编辑顺序与版本，并返回基线和最终虚拟内容。
pub(crate) fn suggest(
    tools: &Tools,
    client: &crate::lsp::Client,
    edit: &WorkspaceEdit,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let plan = plan(tools, Some(client), edit, cancellation)?;
    check(cancellation)?;
    let files: Vec<Value> = plan
        .records
        .iter()
        .map(|(path, record)| {
            let before = match &record.before {
                Original::Bytes(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
                _ => None,
            };
            let after = record
                .after
                .as_ref()
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
            serde_json::json!({"path":path,"before":before,"after":after,
            "resource":record.resource,
            "diff":crate::tools::diff::committed(&record.before,record.after.as_deref())})
        })
        .collect();
    Ok(
        serde_json::json!({"applied":false,"workspace_edit":edit,"files":files,
        "summary":plan.summary,"warnings":plan.warnings,
        "coordinates":"Original LSP ranges are zero-based UTF-16 positions.",
        "instruction":"Advice only. Read target files again and use edit/write/bash. No files were changed."}),
    )
}

/// 一次 `WorkspaceEdit` 的文本编辑在内存中应用。
///
/// 位置是 UTF-16 编码的；越界、反向范围、落在代理对中间的位置、snippet
/// 编辑与真实重叠一律拒绝。范围与替换文本都完全相同的编辑（包括同一位置
/// 的相同插入）只保留第一次出现的一条；不同文本的同位置插入各自保留，
/// 并按声明顺序应用。
pub(crate) fn apply_text_edits(content: &str, edits: &[TextEdit]) -> Result<String> {
    if edits.is_empty() {
        return Ok(content.to_owned());
    }
    for edit in edits {
        if edit.insert_text_format == Some(2) {
            bail!("{SNIPPET_EDIT}");
        }
        ensure!(
            edit.range.start <= edit.range.end,
            "LSP edit range is reversed: {}",
            format_range(edit.range)
        );
    }
    // 先按声明顺序去重：完全相同的编辑只保留第一次出现的那一条。去重放在
    // 排序之前，避免同位置的其它插入把它们隔开后漏掉重复。
    let mut seen: BTreeSet<(Position, Position, &str)> = BTreeSet::new();
    let mut unique: Vec<(usize, &TextEdit)> = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        if seen.insert((edit.range.start, edit.range.end, edit.new_text.as_str())) {
            unique.push((index, edit));
        }
    }
    // 再从下到上应用；同一位置按原始下标降序，使数组声明顺序成为插入顺序。
    unique.sort_by(|(left_index, left), (right_index, right)| {
        right
            .range
            .start
            .cmp(&left.range.start)
            .then_with(|| right_index.cmp(left_index))
    });
    for pair in unique.windows(2) {
        let later = pair[0].1.range;
        let earlier = pair[1].1.range;
        ensure!(
            earlier.end <= later.start,
            "overlapping LSP edits: {} conflicts with {}; LSP produced inconsistent edits",
            format_range(earlier),
            format_range(later)
        );
    }
    let mut spans = Vec::with_capacity(unique.len());
    for (_, edit) in &unique {
        let start = position_offset(content, edit.range.start).with_context(|| {
            format!(
                "LSP edit start {} is outside the document or splits a surrogate pair",
                format_position(edit.range.start)
            )
        })?;
        let end = position_offset(content, edit.range.end).with_context(|| {
            format!(
                "LSP edit end {} is outside the document or splits a surrogate pair",
                format_position(edit.range.end)
            )
        })?;
        ensure!(start <= end, "LSP edit range is reversed");
        spans.push((start, end, edit));
    }
    let mut output = content.to_owned();
    for (start, end, edit) in spans {
        output.replace_range(start..end, &edit.new_text);
    }
    Ok(output)
}

pub(crate) fn sync_file_events(
    registry: &Registry,
    events: &[(PathBuf, u8)],
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for client in registry.started() {
        let events: Vec<_> = events
            .iter()
            .filter_map(|(path, kind)| {
                let path = if path.is_absolute() {
                    path.clone()
                } else {
                    registry.cwd().join(path)
                };
                (registry.covers(&client, &path) || client.document(&path).is_some())
                    .then_some((path, *kind))
            })
            .collect();
        if events.is_empty() {
            continue;
        }
        let mut deny = |request: crate::lsp::transport::ServerRequest| {
            request.respond(serde_json::json!({
                "applied": false,
                "failureReason": crate::lsp::transport::UNAUTHORIZED_EDIT
            }));
        };
        if let Err(error) = client.operation(deadline, cancellation, &mut deny, |op| {
            client.sync_file_events_in_operation(op, &events)
        }) {
            warnings.push(format!(
                "Failed to sync committed files with {}: {error}",
                client.name
            ));
        }
    }
    warnings
}

fn format_position(position: Position) -> String {
    format!(
        "{}:{}",
        position.line.saturating_add(1),
        position.character.saturating_add(1)
    )
}

fn format_range(range: Range) -> String {
    format!(
        "{}-{}",
        format_position(range.start),
        format_position(range.end)
    )
}

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

    /// 重命名目标沿用来源文件的版本证明（原始字节与发起服务器的文档路径）。
    /// 新建/截断的文件不再是原始文档：非 null 版本不能借用旧内容证明。
    fn reset_version_proof(&mut self, path: &Path) {
        if let Some(state) = self.files.get_mut(path) {
            state.proof = Original::Missing;
            state.origin = None;
        }
    }

    fn inherit_version_proof(&mut self, source: &Path, destination: &Path) {
        let Some((proof, origin)) = self.files.get(source).map(|state| {
            (
                clone_original(&state.proof),
                state.origin.clone().unwrap_or_else(|| source.to_path_buf()),
            )
        }) else {
            return;
        };
        if let Some(state) = self.files.get_mut(destination) {
            state.proof = proof;
            state.origin = Some(origin);
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

    /// 版本非 null 时只能由**本次编辑的发起客户端**证明：
    /// - 使用本次查询的发起服务器验证；
    /// - 该文档必须仍处于打开状态，版本精确匹配；
    /// - 文档文本必须等于事务开始前的磁盘基线（不是本批次已 staged 的虚拟内容，
    ///   否则基于旧版本计算的范围会落到新内容上）。
    ///
    /// `null`/缺失版本不要求匹配。
    fn check_version(&self, document: &TextDocumentEdit) -> Result<()> {
        let Some(Some(expected)) = document.text_document.version else {
            return Ok(());
        };
        let resolved = self.resolve(&document.text_document.uri)?;
        let display = resolved.relative.display();
        let client = self
            .client
            .context("Versioned suggestion requires the issuing client")?;
        let (proof_path, baseline) = self.version_baseline(&resolved.path, &display)?;
        let Some(state) = client.document(&proof_path) else {
            bail!(
                "text edit for {display} is based on document version {expected}, but the issuing server ({}) does not have the source document open",
                client.name
            );
        };
        ensure!(
            state.opened,
            "text edit for {display} is based on document version {expected}, but the issuing server ({}) closed the document",
            client.name
        );
        ensure!(
            i64::try_from(expected).ok() == Some(state.version),
            "text edit for {display} is based on document version {expected}, but the issuing server ({}) has version {}; the file changed or was closed",
            client.name,
            state.version
        );
        ensure!(
            state.text.as_bytes() == baseline.as_slice(),
            "text edit for {display} is based on document version {expected}, but the issuing server's document text does not match the file contents when the workspace edit began; the file changed outside this edit"
        );
        Ok(())
    }

    /// 版本证明：(发起服务器打开过的路径, 事务开始前的原始字节)。
    /// 同一批次早先的文本编辑只改虚拟 `current`，不能作为版本基线。
    fn version_baseline(
        &self,
        path: &Path,
        display: &dyn std::fmt::Display,
    ) -> Result<(PathBuf, Vec<u8>)> {
        if let Some(state) = self.files.get(path) {
            let proof_path = state.origin.clone().unwrap_or_else(|| path.to_path_buf());
            return match &state.proof {
                Original::Bytes(bytes) => Ok((proof_path, bytes.clone())),
                Original::Missing => bail!(
                    "text edit for {display} is versioned, but the file did not exist when the workspace edit began"
                ),
                Original::Unavailable => bail!(
                    "text edit for {display} is versioned, but the original file content could not be read"
                ),
            };
        }
        match self.disk_node(path)? {
            NodeState::File(bytes, _) => Ok((path.to_path_buf(), bytes)),
            _ => bail!(
                "text edit for {display} is versioned, but the file did not exist when the workspace edit began"
            ),
        }
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

    fn plan_create(&mut self, create: &CreateFile) -> Result<()> {
        check(self.cancellation)?;
        let resolved = self.resolve(&create.uri)?;
        let options: CreateFileOptions = create.options.clone().unwrap_or_default();
        let display = resolved.relative.display();
        match self.node(&resolved.path)? {
            NodeState::Directory(_) => {
                if options.ignore_if_exists && !options.overwrite {
                    self.summary
                        .push(format!("Skipped creating {display} (already exists)"));
                    return Ok(());
                }
                bail!("cannot create {display}: a directory already exists at that path")
            }
            NodeState::File(existing, _permissions) => {
                if options.ignore_if_exists && !options.overwrite {
                    self.summary
                        .push(format!("Skipped creating {display} (already exists)"));
                    return Ok(());
                }
                ensure!(
                    options.overwrite,
                    "cannot create {display}: the file already exists"
                );
                if existing.is_empty() {
                    self.summary
                        .push(format!("No change creating {display} (already empty)"));
                    return Ok(());
                }
                self.ensure_file(&resolved.path, &resolved.relative)?;
                self.ensure_virtual_parents(&resolved.path)?;

                self.mark_file(&resolved.path, Some(Vec::new()), false);
                self.reset_version_proof(&resolved.path);
                self.summary
                    .push(format!("Create {display} (overwritten with empty content)"));
            }
            NodeState::Missing => {
                self.ensure_file(&resolved.path, &resolved.relative)?;
                self.ensure_virtual_parents(&resolved.path)?;

                self.mark_file(&resolved.path, Some(Vec::new()), false);
                self.reset_version_proof(&resolved.path);
                self.summary.push(format!("Create {display}"));
            }
        }
        Ok(())
    }

    fn plan_rename(&mut self, rename: &RenameFile) -> Result<()> {
        check(self.cancellation)?;
        let from = self.resolve(&rename.old_uri)?;
        let to = self.resolve(&rename.new_uri)?;
        let options: RenameFileOptions = rename.options.clone().unwrap_or_default();
        ensure!(
            from.path != to.path,
            "rename source and target must be different paths"
        );
        ensure!(
            !to.path.starts_with(&from.path),
            "rename target must not be inside the source path"
        );
        let display_from = from.relative.display();
        let display_to = to.relative.display();
        match self.node(&from.path)? {
            NodeState::Missing => bail!("cannot rename {display_from}: the source does not exist"),
            NodeState::File(bytes, permissions) => {
                match self.node(&to.path)? {
                    NodeState::Directory(_) => {
                        if options.ignore_if_exists && !options.overwrite {
                            self.summary.push(format!(
                                "Skipped renaming {display_from} → {display_to} (target exists)"
                            ));
                            return Ok(());
                        }
                        bail!(
                            "cannot rename {display_from} to {display_to}: a directory already exists at the target"
                        );
                    }
                    NodeState::File(_existing, _existing_permissions) => {
                        if options.ignore_if_exists && !options.overwrite {
                            self.summary.push(format!(
                                "Skipped renaming {display_from} → {display_to} (target exists)"
                            ));
                            return Ok(());
                        }
                        ensure!(
                            options.overwrite,
                            "cannot rename {display_from} to {display_to}: the target already exists"
                        );
                    }
                    NodeState::Missing => (),
                };
                self.ensure_file(&from.path, &from.relative)?;
                self.mark_file(&from.path, None, true);
                self.ensure_file(&to.path, &to.relative)?;
                self.ensure_virtual_parents(&to.path)?;
                self.mark_file(&to.path, Some(bytes.clone()), true);
                self.set_file_permissions(&to.path, permissions);
                self.inherit_version_proof(&from.path, &to.path);

                self.summary
                    .push(format!("Rename {display_from} → {display_to}"));
            }
            NodeState::Directory(permissions) => {
                match self.node(&to.path)? {
                    NodeState::File(..) => {
                        if options.ignore_if_exists && !options.overwrite {
                            self.summary.push(format!(
                                "Skipped renaming {display_from}/ → {display_to}/ (target exists)"
                            ));
                            return Ok(());
                        }
                        bail!(
                            "cannot rename directory {display_from} to {display_to}: a file already exists at the target"
                        );
                    }
                    NodeState::Directory(_) => {
                        if options.ignore_if_exists && !options.overwrite {
                            self.summary.push(format!(
                                "Skipped renaming {display_from}/ → {display_to}/ (target exists)"
                            ));
                            return Ok(());
                        }
                        bail!(
                            "cannot rename directory {display_from} to {display_to}: the target directory already exists"
                        );
                    }
                    NodeState::Missing => {}
                }
                let entries = self.walk_virtual(&from.path)?;
                let count = entries
                    .iter()
                    .filter(|entry| entry.content.is_some())
                    .count();
                self.ensure_virtual_parents(&to.path)?;
                // 先处理文件，再标记/建立目录：若先标记来源子目录 Missing，
                // 后续文件会误以为自己已不存在而丢失原始基线。
                for entry in entries.iter().filter(|entry| entry.content.is_some()) {
                    let content = entry.content.as_deref().expect("filtered to files");
                    let source = from.path.join(&entry.relative);
                    let destination = to.path.join(&entry.relative);
                    self.ensure_file(&source, &from.relative.join(&entry.relative))?;
                    self.mark_file(&source, None, true);
                    self.ensure_file(&destination, &to.relative.join(&entry.relative))?;
                    self.mark_file(&destination, Some(content.to_vec()), true);
                    self.set_file_permissions(&destination, entry.permissions);
                    self.inherit_version_proof(&source, &destination);
                }
                for entry in entries.iter().filter(|entry| entry.content.is_none()) {
                    self.kinds
                        .insert(from.path.join(&entry.relative), Kind::Missing);
                    self.kinds.insert(
                        to.path.join(&entry.relative),
                        Kind::Directory {
                            permissions: entry.permissions,
                        },
                    );
                }
                self.kinds.insert(from.path.clone(), Kind::Missing);
                self.kinds
                    .insert(to.path.clone(), Kind::Directory { permissions });

                self.summary.push(format!(
                    "Renamed directory {display_from}/ → {display_to}/ ({count} file(s))"
                ));
            }
        }
        Ok(())
    }

    fn plan_delete(&mut self, delete: &DeleteFile) -> Result<()> {
        check(self.cancellation)?;
        let resolved = self.resolve(&delete.uri)?;
        let options: DeleteFileOptions = delete.options.clone().unwrap_or_default();
        let display = resolved.relative.display();
        match self.node(&resolved.path)? {
            NodeState::Missing => {
                if options.ignore_if_not_exists {
                    self.summary
                        .push(format!("Skipped deleting {display} (missing)"));
                    return Ok(());
                }
                bail!("cannot delete {display}: the target does not exist");
            }
            NodeState::File(_bytes, _permissions) => {
                self.ensure_file(&resolved.path, &resolved.relative)?;
                self.mark_file(&resolved.path, None, true);

                self.summary.push(format!("Delete {display}"));
            }
            NodeState::Directory(_permissions) => {
                let entries = self.walk_virtual(&resolved.path)?;
                if !options.recursive {
                    ensure!(
                        entries.is_empty(),
                        "cannot delete directory {display} without recursive: it is not empty"
                    );
                    self.kinds.insert(resolved.path.clone(), Kind::Missing);

                    self.summary.push(format!("Deleted directory {display}/"));
                    return Ok(());
                }
                let count = entries
                    .iter()
                    .filter(|entry| entry.content.is_some())
                    .count();
                // 先处理文件、再隐藏目录：否则先标记的父目录会让子文件
                // 的原始基线被当成 Missing，丢失删除快照与回滚内容。
                for entry in entries.iter().filter(|entry| entry.content.is_some()) {
                    let child = resolved.path.join(&entry.relative);
                    self.ensure_file(&child, &resolved.relative.join(&entry.relative))?;
                    self.mark_file(&child, None, true);
                }
                for entry in entries.iter().filter(|entry| entry.content.is_none()) {
                    self.kinds
                        .insert(resolved.path.join(&entry.relative), Kind::Missing);
                }
                self.kinds.insert(resolved.path.clone(), Kind::Missing);

                self.summary
                    .push(format!("Deleted directory {display}/ ({count} file(s))"));
            }
        }
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
