//! Actionable diagnostics for failed `replace` edits: duplicate match context, line-number prefix
//! detection, over-escaped literal hints, and closest fuzzy window snippets.
use super::{find_indent_matches, line_number_at, normalize_fuzzy};

fn truncate_snippet(text: &str, max_chars: usize) -> String {
    let mut iter = text.chars();
    let prefix: String = iter.by_ref().take(max_chars).collect();
    if iter.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

pub(super) fn format_duplicate_error(
    original: &str,
    index: usize,
    lines: &[usize],
    total: usize,
) -> String {
    let list = lines
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let suffix = if total > lines.len() {
        format!(", ... ({total} total)")
    } else {
        String::new()
    };
    let mut message = format!(
        "edits[{index}].oldText is not unique (matches at lines {list}{suffix}); provide more context or set replaceAll: true"
    );
    if total <= 3 {
        let src_lines: Vec<&str> = original.lines().collect();
        let contexts: Vec<(usize, String)> = lines
            .iter()
            .filter_map(|&line| {
                let idx = line.checked_sub(1)?;
                let prev = idx
                    .checked_sub(1)
                    .and_then(|p| src_lines.get(p))
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty());
                let next = src_lines
                    .get(idx + 1)
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty());
                match (prev, next) {
                    (Some(p), _) => Some((line, format!("after `{}`", truncate_snippet(p, 72)))),
                    (None, Some(n)) => {
                        Some((line, format!("before `{}`", truncate_snippet(n, 72))))
                    }
                    (None, None) => None,
                }
            })
            .collect();
        if contexts.len() == lines.len() && contexts.iter().any(|(_, c)| *c != contexts[0].1) {
            for (line, ctx) in contexts {
                message.push_str(&format!("\n  line {line}: {ctx}"));
            }
        }
    }
    message
}

pub(super) fn format_missing_error(original: &str, index: usize, old: &str) -> String {
    if let Some(line) = line_number_prefix_hint(original, old) {
        return format!(
            "could not find edits[{index}].oldText (oldText appears to include line-number prefixes matching near line {line}; omit line numbers); read the file again"
        );
    }
    if let Some(line) = over_escaped_hint(original, old) {
        return format!(
            "could not find edits[{index}].oldText (oldText contains over-escaped sequences such as \\\" or \\n that match near line {line} when unescaped; check JSON escaping)"
        );
    }
    if let Some(hint) = missing_edit_hint(original, old) {
        format!("could not find edits[{index}].oldText ({hint}); read the file again")
    } else {
        format!("could not find edits[{index}].oldText; read the file again")
    }
}

fn strip_line_number_prefix(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches([' ', '\t']);
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &trimmed[digits..];
    let after_sep = rest.strip_prefix(':').or_else(|| rest.strip_prefix('|'))?;
    Some(after_sep.strip_prefix(' ').unwrap_or(after_sep))
}

fn line_number_prefix_hint(original: &str, old: &str) -> Option<usize> {
    let mut stripped_lines = Vec::new();
    let mut stripped_any = false;
    for line in old.split('\n') {
        if line.trim().is_empty() {
            stripped_lines.push("");
            continue;
        }
        let rest = strip_line_number_prefix(line)?;
        stripped_any = true;
        stripped_lines.push(rest);
    }
    if !stripped_any {
        return None;
    }
    let candidate = stripped_lines.join("\n");
    if candidate.trim().is_empty() {
        return None;
    }
    if let Some(pos) = original.find(&candidate) {
        return Some(line_number_at(original, pos));
    }
    find_indent_matches(original, &candidate)
        .first()
        .map(|hit| hit.line_number)
}

fn unescape_common_sequences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(&next) = chars.peek()
        {
            match next {
                '"' => {
                    chars.next();
                    out.push('"');
                    continue;
                }
                'n' => {
                    chars.next();
                    out.push('\n');
                    continue;
                }
                't' => {
                    chars.next();
                    out.push('\t');
                    continue;
                }
                '\\' => {
                    chars.next();
                    out.push('\\');
                    continue;
                }
                _ => {}
            }
        }
        out.push(ch);
    }
    out
}

fn over_escaped_hint(original: &str, old: &str) -> Option<usize> {
    if !old.contains('\\') {
        return None;
    }
    let quote_only = old.replace("\\\"", "\"");
    if quote_only != old
        && let Some(pos) = original.find(&quote_only)
    {
        return Some(line_number_at(original, pos));
    }
    let full = unescape_common_sequences(old);
    if full != old {
        if let Some(pos) = original.find(&full) {
            return Some(line_number_at(original, pos));
        }
        if let Some(hit) = find_indent_matches(original, &full).first() {
            return Some(hit.line_number);
        }
    }
    None
}

fn char_bigrams(text: &str) -> Vec<(char, char)> {
    let mut chars = text.chars();
    let Some(mut prev) = chars.next() else {
        return Vec::new();
    };
    let mut grams = Vec::with_capacity(text.len().min(512));
    for next in chars {
        grams.push((prev, next));
        prev = next;
    }
    grams.sort_unstable();
    grams
}

fn line_similarity(a: &str, b: &str) -> f64 {
    let a = a.trim();
    let b = b.trim();
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let ga = char_bigrams(a);
    let gb = char_bigrams(b);
    if ga.is_empty() || gb.is_empty() {
        return 0.0;
    }
    let (mut i, mut j, mut shared) = (0, 0, 0usize);
    while i < ga.len() && j < gb.len() {
        match ga[i].cmp(&gb[j]) {
            std::cmp::Ordering::Equal => {
                shared += 1;
                i += 1;
                j += 1;
            }
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
        }
    }
    (2.0 * shared as f64) / (ga.len() + gb.len()) as f64
}

fn first_line_anchor_lines(src_lines: &[&str], old: &str) -> Option<Vec<usize>> {
    let anchor = old.lines().map(str::trim).find(|line| line.len() >= 6)?;
    let multiline = old.contains('\n');
    let mut lines = Vec::new();
    for (idx, line) in src_lines.iter().enumerate() {
        let trimmed = line.trim();
        let matched = if multiline {
            trimmed == anchor
        } else {
            trimmed == anchor || trimmed.contains(anchor)
        };
        if matched {
            lines.push(idx + 1);
            if lines.len() > 3 {
                return None;
            }
        }
    }
    (!lines.is_empty()).then_some(lines)
}

fn format_window_diff(src_lines: &[&str], start: usize, old_lines: &[&str]) -> Option<String> {
    let mut diffs = Vec::new();
    for (offset, old_line) in old_lines.iter().enumerate() {
        let Some(&src_line) = src_lines.get(start + offset) else {
            break;
        };
        if normalize_fuzzy(src_line) != normalize_fuzzy(old_line) {
            diffs.push(format!(
                "actual line {}: `{}`",
                start + offset + 1,
                truncate_snippet(src_line, 160)
            ));
            if diffs.len() >= 2 {
                break;
            }
        }
    }
    (!diffs.is_empty()).then(|| diffs.join("; "))
}

/// Locate the nearby line in the original from either the first distinctive line or the closest
/// fuzzy window, returning actual differing source line(s) so the model can correct typos without
/// blind retries.
fn missing_edit_hint(original: &str, old: &str) -> Option<String> {
    let src_lines: Vec<&str> = original.lines().collect();
    if src_lines.is_empty() {
        return None;
    }
    let anchor_lines = first_line_anchor_lines(&src_lines, old);
    let first_line_hint = anchor_lines.as_ref().map(|lines| {
        let label = if lines.len() == 1 { "line" } else { "lines" };
        let list = lines
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        format!("first line matches near {label} {list}")
    });

    let old_lines: Vec<&str> = old.strip_suffix('\n').unwrap_or(old).split('\n').collect();
    let scan_limit = src_lines.len().min(8_000);
    let (best_start, best_score) = if old_lines.len() == 1 {
        let target = old_lines[0].trim();
        let target_len = target.chars().count();
        let mut best: Option<(usize, f64)> = None;
        if target_len >= 6 {
            for (idx, line) in src_lines[..scan_limit].iter().enumerate() {
                let len = line.trim().chars().count();
                if len == 0 || len * 3 < target_len || target_len * 3 < len {
                    continue;
                }
                let score = line_similarity(line, target);
                if best.is_none_or(|(_, prev)| score > prev) {
                    best = Some((idx, score));
                }
            }
        }
        best.filter(|(_, score)| *score >= 0.65)
            .map_or((None, 0.0), |(i, s)| (Some(i), s))
    } else {
        let mut candidates: Vec<usize> = Vec::new();
        if let Some(lines) = &anchor_lines {
            let anchor_offset = old_lines
                .iter()
                .position(|l| l.trim().len() >= 6)
                .unwrap_or(0);
            for &line in lines {
                if let Some(start) = line
                    .checked_sub(1)
                    .and_then(|l| l.checked_sub(anchor_offset))
                {
                    candidates.push(start);
                }
            }
        }
        if let Some((longest_idx, longest_line)) = old_lines
            .iter()
            .enumerate()
            .max_by_key(|(_, l)| l.trim().chars().count())
        {
            let target = longest_line.trim();
            let target_len = target.chars().count();
            if target_len >= 6 {
                let mut top: Vec<(usize, f64)> = Vec::new();
                for (idx, line) in src_lines[..scan_limit].iter().enumerate() {
                    let len = line.trim().chars().count();
                    if len == 0 || len * 3 < target_len || target_len * 3 < len {
                        continue;
                    }
                    let score = line_similarity(line, target);
                    if score >= 0.55 {
                        top.push((idx, score));
                    }
                }
                top.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
                for (idx, _) in top.into_iter().take(5) {
                    if let Some(start) = idx.checked_sub(longest_idx)
                        && !candidates.contains(&start)
                    {
                        candidates.push(start);
                    }
                }
            }
        }
        let compare_len = old_lines.len().min(20);
        let mut best: Option<(usize, f64)> = None;
        for start in candidates {
            if start >= src_lines.len() {
                continue;
            }
            let available = (src_lines.len() - start).min(compare_len);
            if available == 0 {
                continue;
            }
            let sum: f64 = (0..available)
                .map(|j| line_similarity(src_lines[start + j], old_lines[j]))
                .sum();
            let score = sum / compare_len as f64;
            if best.is_none_or(|(_, prev)| score > prev) {
                best = Some((start, score));
            }
        }
        best.filter(|(_, score)| *score >= 0.60)
            .map_or((None, 0.0), |(i, s)| (Some(i), s))
    };

    let diff_snippet =
        best_start.and_then(|start| format_window_diff(&src_lines, start, &old_lines));
    match (first_line_hint, best_start, diff_snippet) {
        (Some(hint), _, Some(diff)) => Some(format!("{hint}; {diff}")),
        (Some(hint), _, None) => Some(hint),
        (None, Some(start), Some(diff)) if best_score >= 0.60 => {
            Some(format!("closest match near line {}; {diff}", start + 1))
        }
        _ => None,
    }
}
