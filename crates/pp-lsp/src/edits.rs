//! Read-only validation and virtual preview of LSP edit suggestions, with no commit or rollback entry point.

mod readonly;
mod rename;
mod sync;
mod text;

use crate::types::CreateFile;
use crate::types::DeleteFile;
use crate::types::RenameFile;
use crate::types::TextDocumentEdit;
use crate::types::TextEdit;
use crate::types::WorkspaceEdit;
use crate::uri::uri_to_path;
use anyhow::{Context, Result, bail, ensure};
use pp_core::cancellation::CancellationToken;
use pp_core::cancellation::check;
use pp_workspace::diff::Original;
use pp_workspace::save::DirectoryEntry;
use pp_workspace::save::EntryKind;
use pp_workspace::save::RESOURCE_BYTE_LIMIT;
use pp_workspace::save::RESOURCE_FILE_LIMIT;
use pp_workspace::save::Workspace;
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

/// On-disk baseline at query time and the final virtual content, used only for suggestions and diffs.
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

/// Normalized path plus the relative path used for summaries/records.
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

/// Virtual file state: `before` is the on-disk snapshot from before the transaction started, `current` is the content
/// advanced in declaration order. `before` is never overwritten by later operations, so the undo baseline stays stable.
struct Virtual {
    relative: PathBuf,
    before: Original,
    current: Option<Vec<u8>>,
    permissions: Option<u32>,
    resource: bool,
    /// Original content used for the version proof: text edits only advance `current`, `proof` always remains
    /// the file's on-disk bytes from before the transaction began (a rename target inherits the source file's proof).
    proof: Original,
    /// Client document path the version proof refers to: after a rename inside the transaction, the file at the new path is still
    /// proven by the originating server's document at the old path, and the server must not be required to have opened the new path.
    origin: Option<PathBuf>,
}

struct Planner<'a> {
    client: Option<&'a crate::Client>,
    workspace: &'a Workspace,
    cancellation: &'a CancellationToken,
    files: BTreeMap<PathBuf, Virtual>,
    /// Current virtual type per path; file deletion/directory deletion/rename all override the on-disk state through it,
    /// so a virtual directory walk never counts back disk nodes this transaction already deleted or moved away.
    kinds: BTreeMap<PathBuf, Kind>,
    summary: Vec<String>,
    warnings: Vec<String>,
}

fn plan(
    workspace: &Workspace,
    client: Option<&crate::Client>,
    edit: &WorkspaceEdit,
    cancellation: &CancellationToken,
) -> Result<Plan> {
    let mut planner = Planner {
        client,
        workspace,
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

    /// Read-only determination of the current (virtual-first) node state.
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
        // Ancestor virtual types win: paths under a deleted/moved-away directory must disappear immediately, and must not be
        // seen again through old nodes on disk that are not committed yet.
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

    /// Record the on-disk baseline from before the transaction on first contact with a file; afterwards only the virtual content advances.
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

    /// File operations implicitly create parent directories: the virtual state first records them as directories, and the commit
    /// really creates and tracks their identity through `resource_target_tracked`.
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
                    // This transaction first deleted the file/directory at this path and then created
                    // children under the old path: the old disk node is deleted first, so rebuild it directly as a directory.
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
        let mut pending: BTreeMap<PathBuf, (Resolved, Vec<TextEdit>)> = BTreeMap::new();
        for change in changes {
            if change.get("textDocument").is_some() {
                let document: TextDocumentEdit = serde_json::from_value(change.clone())
                    .context("invalid textDocument edit in workspace edit")?;
                self.check_version(&document)?;
                if !document.edits.is_empty() {
                    let resolved = self.resolve(&document.text_document.uri)?;
                    pending
                        .entry(resolved.path.clone())
                        .or_insert_with(|| (resolved, Vec::new()))
                        .1
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
                    // Advance the virtual text on both sides before simulating the move.
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
        for path in pending.keys().cloned().collect::<Vec<_>>() {
            self.flush_path(&mut pending, &path)?;
        }
        Ok(())
    }

    fn flush_path(
        &mut self,
        pending: &mut BTreeMap<PathBuf, (Resolved, Vec<TextEdit>)>,
        path: &Path,
    ) -> Result<()> {
        if let Some((resolved, edits)) = pending.remove(path)
            && !edits.is_empty()
        {
            self.plan_text(resolved, &edits)?;
        }
        Ok(())
    }

    fn flush_subtree(
        &mut self,
        pending: &mut BTreeMap<PathBuf, (Resolved, Vec<TextEdit>)>,
        uri: &str,
    ) -> Result<()> {
        let root = self.resolve(uri)?.path;
        let targets: Vec<PathBuf> = pending
            .keys()
            .filter(|candidate| candidate.starts_with(&root))
            .cloned()
            .collect();
        for target in targets {
            self.flush_path(pending, &target)?;
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

    /// Virtual directory snapshot: the disk directory as the base, with file contents, delete/move types
    /// and implicitly created parents layered on in declaration order. This way, renaming or recursively deleting the same
    /// tree after text edits and file deletions keeps the snapshot consistent with the earlier virtual operations.
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
                RESOURCE_BYTE_LIMIT,
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
        // Nodes covered by an ancestor Missing/File must not come back to life from stale disk state.
        let keys: Vec<PathBuf> = entries.keys().cloned().collect();
        for relative in keys {
            check(self.cancellation)?;
            if self.virtual_hidden(path, &relative) {
                entries.remove(&relative);
            }
        }
        // Fill in parent directory entries for nested files/directories; existing disk permissions stay unchanged.
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
        // The limit only constrains regular files; directories consume no file allowance. After virtual creation/overwrite/move
        // are combined, the total bytes and file count are re-validated, and exceeding a limit only rejects that suggestion.
        let files = entries
            .values()
            .filter(|(content, _)| content.is_some())
            .count();
        let bytes: u64 = entries
            .values()
            .filter_map(|(content, _)| content.as_ref().map(|c| c.len() as u64))
            .sum();
        ensure!(
            files <= RESOURCE_FILE_LIMIT,
            "directory contains more than {RESOURCE_FILE_LIMIT} files; resource operation refused"
        );
        ensure!(
            bytes <= RESOURCE_BYTE_LIMIT,
            "directory contains more than 256 MiB of regular files; resource operation refused"
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
