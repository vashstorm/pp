//! UTF-16 application of text edits and version proof.

use crate::position::position_offset;
use crate::types::Position;
use crate::types::Range;
use crate::types::TextDocumentEdit;
use crate::types::TextEdit;
use anyhow::{Context, Result, bail, ensure};
use pp_workspace::diff::Original;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{NodeState, Planner};

const SNIPPET_EDIT: &str = "snippet-formatted LSP edits are unsupported";

/// Apply the text edits of one `WorkspaceEdit` in memory.
///
/// Positions are UTF-16 encoded; out-of-range and reversed ranges, positions inside a surrogate pair, snippet
/// edits and genuine overlaps are all rejected. Edits with identical range and replacement text (including identical
/// insertions at the same position) keep only the first occurrence; insertions of different text at the same position are each kept
/// and applied in declaration order.
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
    // Deduplicate in declaration order first: identical edits keep only the first occurrence. Deduplication runs
    // before sorting, so other insertions at the same position cannot split them apart and hide a duplicate.
    let mut seen: BTreeSet<(Position, Position, &str)> = BTreeSet::new();
    let mut unique: Vec<(usize, &TextEdit)> = Vec::with_capacity(edits.len());
    for (index, edit) in edits.iter().enumerate() {
        if seen.insert((edit.range.start, edit.range.end, edit.new_text.as_str())) {
            unique.push((index, edit));
        }
    }
    // Then apply bottom-up; at the same position apply in descending original index, so array declaration order becomes insertion order.
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

impl<'a> Planner<'a> {
    /// A non-null version can only be proven by **the client that initiated this edit**:
    /// - validate with the originating server of this query;
    /// - the document must still be open, with an exact version match;
    /// - the document text must equal the on-disk baseline from before the transaction began (not the virtual content already staged in this batch,
    ///   otherwise ranges computed against the old version would land on the new content).
    ///
    /// A `null`/absent version requires no match.
    pub(super) fn check_version(&self, document: &TextDocumentEdit) -> Result<()> {
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

    /// Version proof: (path opened by the originating server, original bytes from before the transaction began).
    /// Earlier text edits in the same batch only change the virtual `current` and cannot serve as a version baseline.
    pub(super) fn version_baseline(
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
}
