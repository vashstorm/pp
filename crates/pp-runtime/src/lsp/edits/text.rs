//! 文本编辑的 UTF-16 应用与版本证明。

use crate::lsp::position::position_offset;
use crate::lsp::types::Position;
use crate::lsp::types::Range;
use crate::lsp::types::TextDocumentEdit;
use crate::lsp::types::TextEdit;
use crate::tools::changes::Original;
use anyhow::{Context, Result, bail, ensure};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::{NodeState, Planner};

const SNIPPET_EDIT: &str = "snippet-formatted LSP edits are unsupported";

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
    /// 版本非 null 时只能由**本次编辑的发起客户端**证明：
    /// - 使用本次查询的发起服务器验证；
    /// - 该文档必须仍处于打开状态，版本精确匹配；
    /// - 文档文本必须等于事务开始前的磁盘基线（不是本批次已 staged 的虚拟内容，
    ///   否则基于旧版本计算的范围会落到新内容上）。
    ///
    /// `null`/缺失版本不要求匹配。
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

    /// 版本证明：(发起服务器打开过的路径, 事务开始前的原始字节)。
    /// 同一批次早先的文本编辑只改虚拟 `current`，不能作为版本基线。
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
