//! 创建／重命名／删除条目的虚拟推进与版本证明继承。

use crate::lsp::types::CreateFile;
use crate::lsp::types::CreateFileOptions;
use crate::lsp::types::DeleteFile;
use crate::lsp::types::DeleteFileOptions;
use crate::lsp::types::RenameFile;
use crate::lsp::types::RenameFileOptions;
use crate::tools::changes::Original;
use anyhow::{Result, bail, ensure};
use pp_core::cancellation::check;
use std::path::Path;

use super::{Kind, NodeState, Planner, clone_original};

impl<'a> Planner<'a> {
    /// 重命名目标沿用来源文件的版本证明（原始字节与发起服务器的文档路径）。
    /// 新建/截断的文件不再是原始文档：非 null 版本不能借用旧内容证明。
    pub(super) fn reset_version_proof(&mut self, path: &Path) {
        if let Some(state) = self.files.get_mut(path) {
            state.proof = Original::Missing;
            state.origin = None;
        }
    }

    pub(super) fn inherit_version_proof(&mut self, source: &Path, destination: &Path) {
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

    pub(super) fn plan_create(&mut self, create: &CreateFile) -> Result<()> {
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

    pub(super) fn plan_rename(&mut self, rename: &RenameFile) -> Result<()> {
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

    pub(super) fn plan_delete(&mut self, delete: &DeleteFile) -> Result<()> {
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
}
