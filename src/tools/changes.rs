//! In-memory edit/write changes for the latest task. Snapshots stay here rather
//! than entering UI events or session history; views contain only net diffs and
//! tokens that must be revalidated before an undo can commit.

use std::{
    collections::{BTreeMap, btree_map::Entry},
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};

use crate::events::FileDiff;

use super::{diff, save};

const STALE: &str = "Changes are stale; reopen /changes.";
const CONFLICT: &str = "File changed outside this task; undo refused.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UndoToken {
    pub(crate) task_id: u64,
    pub(crate) revision: u64,
    pub(crate) path: PathBuf,
}

#[derive(Debug)]
pub(crate) struct ChangesView {
    pub(crate) task_id: u64,
    pub(crate) revision: u64,
    pub(crate) files: Vec<ChangedFile>,
}

#[derive(Debug)]
pub(crate) struct ChangedFile {
    pub(crate) path: PathBuf,
    pub(crate) sources: Vec<String>,
    pub(crate) diff: FileDiff,
    pub(crate) undo: std::result::Result<UndoToken, String>,
}

#[derive(Debug)]
pub(crate) struct UndoResult {
    pub(crate) view: ChangesView,
    pub(crate) warning: Option<String>,
}

#[derive(Debug)]
pub(crate) enum Original {
    Missing,
    Bytes(Vec<u8>),
    Unavailable,
}

#[derive(Debug)]
struct FileChange {
    original: Original,
    latest: Vec<u8>,
    interleaved: bool,
    sources: std::collections::BTreeSet<String>,
}

#[derive(Default)]
pub(crate) struct TaskChanges {
    task_id: u64,
    revision: u64,
    files: BTreeMap<PathBuf, FileChange>,
}

impl TaskChanges {
    pub(super) fn begin(&mut self) {
        self.clear();
    }

    pub(super) fn clear(&mut self) {
        self.task_id = self.task_id.checked_add(1).expect("task id exhausted");
        self.advance_revision();
        self.files.clear();
    }

    fn advance_revision(&mut self) {
        self.revision = self
            .revision
            .checked_add(1)
            .expect("change revision exhausted");
    }

    /// Records only committed tool writes, taking ownership of their snapshots.
    /// The caller supplies a workspace-normalized relative path. Net-zero
    /// records remain stored so later writes cannot replace the first baseline
    /// or erase evidence of intervening changes.
    pub(crate) fn record(&mut self, path: PathBuf, before: Original, latest: Vec<u8>) {
        match self.files.entry(path) {
            Entry::Vacant(entry) => {
                entry.insert(FileChange {
                    original: before,
                    latest,
                    interleaved: false,
                    sources: Default::default(),
                });
            }
            Entry::Occupied(mut entry) => {
                let change = entry.get_mut();
                if !matches!(&before, Original::Bytes(bytes) if bytes == &change.latest) {
                    change.interleaved = true;
                }
                change.latest = latest;
            }
        }
        self.advance_revision();
    }

    pub(crate) fn source(&mut self, path: &std::path::Path, source: &str) {
        if let Some(change) = self.files.get_mut(path) {
            change.sources.insert(source.to_owned());
        }
    }
    /// Validates the confirmation before resolving a fresh safe target. The
    /// baseline remains borrowed and retained until the filesystem commits;
    /// only a committed restore invalidates the revision and consumes it.
    pub(super) fn undo(
        &mut self,
        workspace: &save::Workspace,
        token: &UndoToken,
    ) -> Result<UndoResult> {
        ensure!(
            token.task_id == self.task_id && token.revision == self.revision,
            STALE
        );
        let change = self.files.get(&token.path).context(STALE)?;
        let original = change.original_for_undo()?;
        let target = workspace.target(&token.path, false).context(CONFLICT)?;
        let warning = target.restore_if_unchanged(&change.latest, original)?;

        // A directory-sync warning still means the restore committed. Failed
        // restores return above without consuming the baseline or token.
        self.files.remove(&token.path);
        self.advance_revision();
        Ok(UndoResult {
            view: self.view(workspace),
            warning,
        })
    }

    pub(super) fn view(&self, workspace: &save::Workspace) -> ChangesView {
        let files = self
            .files
            .iter()
            .filter(|(_, change)| {
                change.interleaved
                    || !matches!(&change.original, Original::Bytes(bytes) if bytes == &change.latest)
            })
            .map(|(path, change)| {
                let undo = change
                    .original_for_undo()
                    .and_then(|_| {
                        workspace
                            .target(path, false)
                            .and_then(|target| target.ensure_matches(&change.latest))
                            .context(CONFLICT)
                    })
                    .map(|()| UndoToken {
                        task_id: self.task_id,
                        revision: self.revision,
                        path: path.clone(),
                    })
                    .map_err(|error| error.to_string());
                ChangedFile {
                    path: path.clone(),
                    sources: change.sources.iter().cloned().collect(),
                    diff: change.diff(),
                    undo,
                }
            })
            .collect();
        ChangesView {
            task_id: self.task_id,
            revision: self.revision,
            files,
        }
    }
}

impl FileChange {
    fn original_for_undo(&self) -> Result<Option<&[u8]>> {
        ensure!(
            !self.interleaved,
            "File changed outside this task between tool writes; undo is unavailable."
        );
        match &self.original {
            Original::Missing => Ok(None),
            Original::Bytes(bytes) => Ok(Some(bytes)),
            Original::Unavailable => {
                anyhow::bail!("Original content could not be read; undo is unavailable.");
            }
        }
    }

    fn diff(&self) -> FileDiff {
        let before = match &self.original {
            Original::Missing => None,
            Original::Bytes(bytes) => match std::str::from_utf8(bytes) {
                Ok(text) => Some(text),
                Err(_) => {
                    return FileDiff::Unavailable {
                        reason: "Previous content is not UTF-8; text diff unavailable.".into(),
                    };
                }
            },
            Original::Unavailable => {
                return FileDiff::Unavailable {
                    reason: "Previous content could not be read; text diff unavailable.".into(),
                };
            }
        };
        match std::str::from_utf8(&self.latest) {
            Ok(after) => diff::build(before, after),
            Err(_) => FileDiff::Unavailable {
                reason: "Latest content is not UTF-8; text diff unavailable.".into(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::events::DiffLineKind;

    use super::*;

    #[test]
    fn changes_net_zero_is_hidden_but_keeps_the_first_baseline() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(directory.path()).unwrap();
        let mut changes = TaskChanges::default();
        changes.record(
            "file".into(),
            Original::Bytes(b"original".to_vec()),
            b"middle".to_vec(),
        );
        changes.record(
            "file".into(),
            Original::Bytes(b"middle".to_vec()),
            b"original".to_vec(),
        );
        assert!(changes.view(&workspace).files.is_empty());
        assert_eq!(changes.files.len(), 1);

        fs::write(directory.path().join("file"), b"final").unwrap();
        changes.record(
            "file".into(),
            Original::Bytes(b"original".to_vec()),
            b"final".to_vec(),
        );
        let view = changes.view(&workspace);
        assert_eq!(view.files.len(), 1);
        assert!(view.files[0].undo.is_ok());
        let Original::Bytes(original) = &changes.files[&PathBuf::from("file")].original else {
            panic!("original bytes expected");
        };
        assert_eq!(original, b"original");
    }

    #[test]
    fn changes_missing_empty_file_is_not_an_existing_empty_file() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(directory.path()).unwrap();
        fs::write(directory.path().join("created"), b"").unwrap();
        fs::write(directory.path().join("existing"), b"").unwrap();
        let mut changes = TaskChanges::default();
        changes.record("created".into(), Original::Missing, Vec::new());
        changes.record("existing".into(), Original::Bytes(Vec::new()), Vec::new());

        let view = changes.view(&workspace);
        assert_eq!(view.files.len(), 1);
        assert_eq!(view.files[0].path, PathBuf::from("created"));
        assert!(view.files[0].undo.is_ok());
        assert!(matches!(
            &view.files[0].diff,
            FileDiff::Text { created: true, added: 0, removed: 0, hunks } if hunks.is_empty()
        ));
    }

    #[test]
    fn changes_pollution_is_permanent_even_when_original_bytes_return() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(directory.path()).unwrap();
        fs::write(directory.path().join("file"), b"original").unwrap();
        for before in [
            Original::Bytes(b"external".to_vec()),
            Original::Missing,
            Original::Unavailable,
        ] {
            let mut changes = TaskChanges::default();
            changes.record(
                "file".into(),
                Original::Bytes(b"original".to_vec()),
                b"first".to_vec(),
            );
            changes.record("file".into(), before, b"second".to_vec());
            changes.record(
                "file".into(),
                Original::Bytes(b"second".to_vec()),
                b"original".to_vec(),
            );

            let view = changes.view(&workspace);
            assert_eq!(view.files.len(), 1);
            assert!(
                view.files[0]
                    .undo
                    .as_ref()
                    .unwrap_err()
                    .contains("between tool writes")
            );
            assert!(changes.files[&PathBuf::from("file")].interleaved);
        }
    }

    #[test]
    fn changes_view_sorts_paths_and_diffs_first_baseline_to_latest() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(directory.path()).unwrap();
        let mut changes = TaskChanges::default();
        fs::write(directory.path().join("z"), b"created\n").unwrap();
        changes.record("z".into(), Original::Missing, b"created\n".to_vec());
        changes.record(
            "a".into(),
            Original::Bytes(b"user-local\r\nkeep".to_vec()),
            b"middle\r\nkeep".to_vec(),
        );
        fs::write(directory.path().join("a"), b"final\r\nkeep").unwrap();
        changes.record(
            "a".into(),
            Original::Bytes(b"middle\r\nkeep".to_vec()),
            b"final\r\nkeep".to_vec(),
        );

        let view = changes.view(&workspace);
        assert_eq!(view.files.len(), 2);
        assert_eq!(view.files[0].path, PathBuf::from("a"));
        assert_eq!(view.files[1].path, PathBuf::from("z"));
        let FileDiff::Text {
            created,
            added,
            removed,
            hunks,
        } = &view.files[0].diff
        else {
            panic!("text diff expected");
        };
        assert!(!created);
        assert_eq!((*added, *removed), (1, 1));
        let lines = &hunks[0].lines;
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].kind, DiffLineKind::Removed);
        assert_eq!(lines[0].old_line, Some(1));
        assert_eq!(lines[0].text, "user-local\r\n");
        assert_eq!(lines[1].kind, DiffLineKind::Added);
        assert_eq!(lines[1].new_line, Some(1));
        assert_eq!(lines[1].text, "final\r\n");
        assert_eq!(lines[2].text, "keep");
    }

    #[test]
    fn changes_binary_baseline_has_no_text_diff_but_allows_undo() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(directory.path()).unwrap();
        fs::write(directory.path().join("file"), b"text").unwrap();
        let mut changes = TaskChanges::default();
        changes.record(
            "file".into(),
            Original::Bytes(vec![0xff, 0, 0xfe]),
            b"text".to_vec(),
        );

        let view = changes.view(&workspace);
        assert!(matches!(view.files[0].diff, FileDiff::Unavailable { .. }));
        assert!(view.files[0].undo.is_ok());
    }

    #[test]
    fn changes_unknown_baseline_and_missing_target_block_only_their_files() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(directory.path()).unwrap();
        fs::write(directory.path().join("unknown"), b"latest").unwrap();
        fs::write(directory.path().join("ready"), b"latest").unwrap();
        let mut changes = TaskChanges::default();
        changes.record("unknown".into(), Original::Unavailable, b"latest".to_vec());
        changes.record("missing".into(), Original::Missing, b"latest".to_vec());
        changes.record("ready".into(), Original::Missing, b"latest".to_vec());

        let view = changes.view(&workspace);
        assert_eq!(view.files.len(), 3);
        assert!(view.files[0].undo.is_err());
        assert!(view.files[1].undo.is_ok());
        assert!(
            view.files[2]
                .undo
                .as_ref()
                .unwrap_err()
                .contains("could not be read")
        );
        assert!(matches!(view.files[2].diff, FileDiff::Unavailable { .. }));
    }

    #[test]
    fn changes_current_conflict_is_rechecked_without_permanent_pollution() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(directory.path()).unwrap();
        let mut changes = TaskChanges::default();
        changes.record("file".into(), Original::Missing, b"latest".to_vec());
        fs::write(directory.path().join("file"), b"external").unwrap();
        let blocked = changes.view(&workspace);
        assert!(blocked.files[0].undo.is_err());
        fs::write(directory.path().join("file"), b"latest").unwrap();
        let ready = changes.view(&workspace);
        assert!(ready.files[0].undo.is_ok());
        assert_eq!(blocked.revision, ready.revision);
        assert!(!changes.files[&PathBuf::from("file")].interleaved);
    }

    #[test]
    fn changes_begin_and_clear_invalidate_ids_without_resetting_revision() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = save::Workspace::open(directory.path()).unwrap();
        fs::write(directory.path().join("file"), b"latest").unwrap();
        let mut changes = TaskChanges::default();
        changes.begin();
        changes.record("file".into(), Original::Missing, b"latest".to_vec());
        let first = changes.view(&workspace);
        let first_token = first.files[0].undo.as_ref().unwrap();
        assert_eq!(first_token.task_id, first.task_id);
        assert_eq!(first_token.revision, first.revision);
        assert_eq!(first_token.path, PathBuf::from("file"));

        changes.clear();
        let cleared = changes.view(&workspace);
        assert!(cleared.files.is_empty());
        assert!(cleared.task_id > first.task_id);
        assert!(cleared.revision > first.revision);
        changes.begin();
        changes.record("file".into(), Original::Missing, b"latest".to_vec());
        let next = changes.view(&workspace);
        assert!(next.task_id > cleared.task_id);
        assert!(next.revision > cleared.revision);
        assert_ne!(next.files[0].undo.as_ref().unwrap(), first_token);
    }
}
