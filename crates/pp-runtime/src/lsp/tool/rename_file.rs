//! rename_file：文件／目录移动建议的枚举与聚合。

use crate::lsp::Registry;
use crate::lsp::edits;
use crate::lsp::uri;
use crate::tools::Tools;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Instant;

use pp_core::cancellation::CancellationToken;

use super::servers::{all_real_servers, real_servers_for, resolve_path};
use super::suggestion::{parse_workspace_edit, suggestion_conflicts, suggestion_result};
use super::{Action, Params, deny_service, done, is_control};

/// 一次目录重命名最多枚举的普通文件数。
const MAX_RENAME_PAIRS: usize = 1000;

struct RenamePair {
    old: PathBuf,
    new: PathBuf,
}

struct RenameEnumeration {
    pairs: Vec<RenamePair>,
    exceeded: bool,
}

fn enumerate_rename_pairs(
    source: &Path,
    dest: &Path,
    directory: bool,
) -> Result<RenameEnumeration> {
    if !directory {
        return Ok(RenameEnumeration {
            pairs: vec![RenamePair {
                old: source.to_path_buf(),
                new: dest.to_path_buf(),
            }],
            exceeded: false,
        });
    }
    let mut pairs = Vec::new();
    let mut stack = vec![source.to_path_buf()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current)
            .with_context(|| format!("reading directory {}", current.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() {
                bail!(
                    "{} is a symlink; a directory rename only covers regular files",
                    path.display()
                );
            }
            if metadata.is_dir() {
                stack.push(path);
                continue;
            }
            ensure!(
                metadata.is_file(),
                "{} is not a regular file; a directory rename only covers regular files",
                path.display()
            );
            let Ok(relative) = path.strip_prefix(source) else {
                continue;
            };
            if pairs.len() >= MAX_RENAME_PAIRS {
                return Ok(RenameEnumeration {
                    pairs,
                    exceeded: true,
                });
            }
            pairs.push(RenamePair {
                old: path.clone(),
                new: dest.join(relative),
            });
        }
    }
    pairs.sort_by(|left, right| left.old.cmp(&right.old));
    Ok(RenameEnumeration {
        pairs,
        exceeded: false,
    })
}

pub(super) fn rename_file(
    tools: &Tools,
    registry: &Registry,
    params: &Params,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<Value> {
    let source = resolve_path(registry.cwd(), params.file()?);
    let dest = resolve_path(
        registry.cwd(),
        params
            .new_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .context("rename_file requires new_name")?,
    );
    tools.workspace().components(&source)?;
    tools.workspace().components(&dest)?;
    ensure!(
        source != dest && !dest.starts_with(&source),
        "Invalid rename destination"
    );
    let meta = std::fs::symlink_metadata(&source)?;
    ensure!(
        !meta.file_type().is_symlink(),
        "Rename source must not be a symlink"
    );
    ensure!(
        meta.is_file() || meta.is_dir(),
        "Rename source must be a regular file or directory"
    );
    edits::validate_move(tools, &source, &dest, cancellation)?;
    let enumeration = enumerate_rename_pairs(&source, &dest, meta.is_dir())?;
    ensure!(
        !enumeration.exceeded,
        "Directory exceeds {MAX_RENAME_PAIRS} files; query smaller batches"
    );
    let pairs: Vec<Value> = enumeration
        .pairs
        .iter()
        .map(|pair| {
            json!({
        "oldUri":uri::path_to_uri(&pair.old),"newUri":uri::path_to_uri(&pair.new)})
        })
        .collect();
    let definitions = all_real_servers(registry);
    let mut suggestions = Vec::new();
    let mut failures = Vec::new();
    for definition in definitions {
        if !enumeration
            .pairs
            .iter()
            .any(|pair| definition_matches(registry, &definition.name, &pair.old))
        {
            continue;
        }
        let server_pairs: Vec<Value> = enumeration
            .pairs
            .iter()
            .zip(&pairs)
            .filter(|(pair, _)| definition_matches(registry, &definition.name, &pair.old))
            .map(|(_, value)| value.clone())
            .collect();
        let outcome = (|| -> Result<Value> {
            let client = registry.ensure_client(&definition.name, deadline, cancellation)?;
            ensure!(
                client
                    .capabilities
                    .raw
                    .pointer("/workspace/fileOperations/willRename")
                    .is_some_and(|v| v.is_object() || v == &Value::Bool(true)),
                "Server does not support willRenameFiles"
            );
            let mut deny = deny_service();
            let edit = client.operation(deadline, cancellation, &mut deny, |op| {
                for pair in &enumeration.pairs {
                    if definition_matches(registry, &definition.name, &pair.old) {
                        client.open_or_refresh(op, &pair.old)?;
                    }
                }
                parse_workspace_edit(
                    &op.request("workspace/willRenameFiles", json!({"files":server_pairs}))?,
                )
            })?;
            suggestion_result(tools, &client, Action::RenameFile, &edit, cancellation)
        })();
        match outcome {
            Ok(value) => suggestions.push(value),
            Err(error) if is_control(&error) => return Err(error),
            Err(error) => failures.push(json!({"server":definition.name,"error":pp_core::logging::diagnostic_text(&format!("{error:#}"))})),
        }
    }
    let conflicts = suggestion_conflicts(&suggestions);
    let complete = failures.is_empty() && !suggestions.is_empty() && conflicts.is_empty();
    let mut value = done(
        Action::RenameFile,
        complete,
        format!(
            "Rename suggestion only: {} -> {}. No files were moved. Server suggestions are independent; compare overlapping changes before editing.\n{}",
            source.display(),
            dest.display(),
            serde_json::to_string_pretty(&suggestions)?
        ),
    );
    value["details"]["applied"] = json!(false);
    value["details"]["complete"] = json!(complete);
    value["details"]["suggestions"] = json!(suggestions);
    value["details"]["failures"] = json!(failures);
    value["details"]["conflicts"] = json!(conflicts);
    value["details"]["move"] = json!({"kind":"rename","oldUri":uri::path_to_uri(&source),"newUri":uri::path_to_uri(&dest)});
    Ok(value)
}

fn definition_matches(registry: &Registry, name: &str, path: &Path) -> bool {
    real_servers_for(registry, path)
        .iter()
        .any(|definition| definition.name == name)
}
